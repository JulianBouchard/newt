use super::Timed;
use super::calls::{call_context, record_output, timed_call};
use super::plan::{OUTWARD_CANCELLED, OUTWARD_SKIPPED, failure};
use crate::journal::{Journal, JournalEntry, Replay};
use crate::message::ToolCall;
use crate::tools::{Retention, ToolHost};
use crate::turn::{TurnContext, TurnDeps};

pub(super) async fn run_outward<M, H: ToolHost, S, J: Journal>(
    tool_calls: &[ToolCall],
    outward: &[usize],
    slots: &mut [Option<Timed>],
    context: &TurnContext<'_>,
    deps: &TurnDeps<'_, M, H, S, J>,
    replay: &Replay,
) {
    let replayed_outward_failed = tool_calls.iter().any(|call| {
        context.retention.get(&call.name) == Some(&Retention::Outward)
            && replay
                .output(&call.id)
                .is_some_and(|output| output.is_error)
    });
    let mut skip_reason = replayed_outward_failed.then(|| OUTWARD_SKIPPED.to_owned());
    for &slot in outward {
        let call = &tool_calls[slot];
        if skip_reason.is_none() && deps.cancel.is_cancelled() {
            skip_reason = Some(OUTWARD_CANCELLED.to_owned());
        }
        if skip_reason.is_none()
            && let Err(error) = deps
                .journal
                .append_and_wait(JournalEntry::OutwardIntent {
                    call_id: call.id.clone(),
                })
                .await
        {
            skip_reason = Some(format!("Not sent: {error}"));
        }
        let output = if let Some(reason) = &skip_reason {
            (failure(reason.clone()), 0)
        } else {
            let output = timed_call(
                deps.host,
                call,
                &call_context(context, call),
                context,
                deps.sink,
            )
            .await;
            record_output(deps.journal, call, &output.0);
            if output.0.is_error {
                skip_reason = Some(OUTWARD_SKIPPED.to_owned());
            }
            output
        };
        slots[slot] = Some(output);
    }
}
