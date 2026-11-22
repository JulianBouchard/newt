use futures_util::future::BoxFuture;

use super::calls::{call_context, record_output, timed_call, timed_read};
use super::{Early, Timed};
use crate::journal::Journal;
use crate::message::ToolCall;
use crate::tools::{Retention, ToolHost};
use crate::turn::{TurnContext, TurnDeps};

/// Starts a call before its model response has finished streaming, if running it
/// early is safe: a workspace read, or a host tool whose retention is `ephemeral`.
/// Writes and sends wait for the whole response, so a crash before the response is
/// journaled can never repeat them.
pub(crate) fn start_early<'a, M, H: ToolHost, S, J>(
    call: ToolCall,
    context: &'a TurnContext<'a>,
    deps: &'a TurnDeps<'a, M, H, S, J>,
) -> Option<BoxFuture<'a, (String, Timed)>>
where
    M: Sync,
    S: Sync,
    J: Sync,
{
    if call.invalid_arguments().is_some() {
        return None;
    }
    if crate::tools::is_workspace_tool(&call.name) {
        return Some(Box::pin(async move {
            let output = timed_read(context, &call).await;
            (call.id, output)
        }));
    }
    if context.retention.get(&call.name) != Some(&Retention::Ephemeral) {
        return None;
    }
    let call_context = call_context(context, &call);
    Some(Box::pin(async move {
        let output = timed_call(deps.host, &call, &call_context, context, deps.sink).await;
        (call.id, output)
    }))
}

/// Fills the slots of calls that already ran while the model was streaming and
/// returns the ones still to run. Host calls are journaled as they would be after
/// running; workspace reads are not journaled.
pub(super) fn take_early<J: Journal>(
    tool_calls: &[ToolCall],
    lane: Vec<usize>,
    slots: &mut [Option<Timed>],
    early: &mut Early,
    journal: Option<&J>,
) -> Vec<usize> {
    lane.into_iter()
        .filter(|&slot| {
            let call = &tool_calls[slot];
            let Some(output) = early.remove(&call.id) else {
                return true;
            };
            if let Some(journal) = journal {
                record_output(journal, call, &output.0);
            }
            slots[slot] = Some(output);
            false
        })
        .collect()
}
