use std::collections::HashMap;
use std::time::Instant;

use futures_util::stream::{self, StreamExt};

use crate::event::{Event, EventKind};
use crate::journal::{Journal, JournalEntry, Replay};
use crate::message::{ToolCall, ToolResult};
use crate::sink::EventSink;
use crate::tools::{END_TURN, EndTurn, Retention, ToolHost, ToolOutput, run_workspace_tool};
use crate::turn::{TurnContext, TurnDeps};

const TOOL_CONCURRENCY: usize = 5;
const END_TURN_BLOCKED: &str = "Turn not ended: a tool call in this batch failed. Deal with the failure, then call end_turn again.";
const END_TURN_REPEATED: &str = "Only one end_turn per batch; this one was ignored.";
const OUTWARD_SKIPPED: &str =
    "Not run: an earlier outward call in this batch failed, so later ones were skipped.";
const OUTWARD_CANCELLED: &str = "Not run: the turn was cancelled before this call started.";

pub(crate) struct Batch {
    pub(crate) results: Vec<ToolResult>,
    pub(crate) summaries: HashMap<String, String>,
    pub(crate) end: Option<EndTurn>,
}

enum Plan {
    Done(ToolOutput, u64),
    Concurrent,
    Outward,
}

fn failure(content: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: content.into(),
        is_error: true,
        summary: None,
    }
}

fn success(content: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: content.into(),
        is_error: false,
        summary: None,
    }
}

fn plan_calls(
    tool_calls: &[ToolCall],
    context: &TurnContext<'_>,
    replay: &Replay,
) -> (Vec<Plan>, Option<EndTurn>, Option<usize>) {
    let mut end = None;
    let mut end_slot = None;
    let plans = tool_calls
        .iter()
        .enumerate()
        .map(|(slot, call)| {
            if let Some(raw) = call.invalid_arguments() {
                return Plan::Done(
                    failure(format!(
                        "The arguments for {} were not a complete JSON object (possibly cut off by the output limit): call it again. Received: {raw}",
                        call.name
                    )),
                    0,
                );
            }
            if let Some(output) = replay.output(&call.id) {
                return Plan::Done(output.clone(), 0);
            }
            if call.name == END_TURN {
                if end_slot.is_some() {
                    return Plan::Done(failure(END_TURN_REPEATED), 0);
                }
                end_slot = Some(slot);
                return Plan::Done(
                    match EndTurn::parse(&call.arguments) {
                        Ok(parsed) => {
                            end = Some(parsed);
                            success("Turn ended.")
                        }
                        Err(message) => failure(message),
                    },
                    0,
                );
            }
            let started = Instant::now();
            if let Some(output) = run_workspace_tool(call, context.workspace) {
                return Plan::Done(output, elapsed_ms(started));
            }
            match context.retention.get(&call.name) {
                Some(Retention::Outward) => Plan::Outward,
                Some(_) => Plan::Concurrent,
                None => Plan::Done(failure(format!("unknown tool `{}`", call.name)), 0),
            }
        })
        .collect();
    (plans, end, end_slot)
}

pub(crate) async fn run_tools<M, H: ToolHost, S, J: Journal>(
    tool_calls: &[ToolCall],
    context: &TurnContext<'_>,
    deps: &TurnDeps<'_, M, H, S, J>,
    replay: &Replay,
) -> Batch {
    let (host, journal) = (deps.host, deps.journal);
    let (plans, mut end, end_slot) = plan_calls(tool_calls, context, replay);

    let mut slots: Vec<Option<(ToolOutput, u64)>> = Vec::with_capacity(plans.len());
    let mut concurrent = Vec::new();
    let mut outward = Vec::new();
    for (slot, plan) in plans.into_iter().enumerate() {
        match plan {
            Plan::Done(output, ms) => slots.push(Some((output, ms))),
            Plan::Concurrent => {
                concurrent.push(slot);
                slots.push(None);
            }
            Plan::Outward => {
                outward.push(slot);
                slots.push(None);
            }
        }
    }

    let keys: Vec<String> = tool_calls
        .iter()
        .map(|call| idempotency_key(context, call))
        .collect();
    let calls: Vec<_> = concurrent
        .iter()
        .map(|&slot| timed_call(host, &tool_calls[slot], &keys[slot]))
        .collect();
    let outputs: Vec<(ToolOutput, u64)> = stream::iter(calls)
        .buffered(TOOL_CONCURRENCY)
        .collect()
        .await;
    for (&slot, output) in concurrent.iter().zip(outputs) {
        record_output(journal, &tool_calls[slot], &output.0);
        slots[slot] = Some(output);
    }

    let mut skip_reason = None;
    for slot in outward {
        let call = &tool_calls[slot];
        if skip_reason.is_none() && deps.cancel.is_cancelled() {
            skip_reason = Some(OUTWARD_CANCELLED.to_owned());
        }
        if skip_reason.is_none()
            && !replay.has_intent(&call.id)
            && let Err(error) = journal
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
            let output = timed_call(host, call, &keys[slot]).await;
            record_output(journal, call, &output.0);
            if output.0.is_error {
                skip_reason = Some(OUTWARD_SKIPPED.to_owned());
            }
            output
        };
        slots[slot] = Some(output);
    }

    let mut outputs: Vec<(ToolOutput, u64)> = slots
        .into_iter()
        .map(|slot| {
            slot.unwrap_or_else(|| (failure("internal error: tool was never scheduled"), 0))
        })
        .collect();
    let batch_failed = outputs
        .iter()
        .enumerate()
        .any(|(slot, (output, _))| output.is_error && Some(slot) != end_slot);
    if batch_failed
        && end.take().is_some()
        && let Some(slot) = end_slot
        && let Some(entry) = outputs.get_mut(slot)
    {
        entry.0 = failure(END_TURN_BLOCKED);
    }

    finish_batch(tool_calls, outputs, end, context, deps.sink)
}

fn finish_batch(
    tool_calls: &[ToolCall],
    outputs: Vec<(ToolOutput, u64)>,
    end: Option<EndTurn>,
    context: &TurnContext<'_>,
    sink: &dyn EventSink,
) -> Batch {
    let mut summaries = HashMap::new();
    let mut results = Vec::with_capacity(outputs.len());
    for (call, (output, ms)) in tool_calls.iter().zip(outputs) {
        sink.emit(Event::now(EventKind::ToolFinished {
            session: context.ids.session.to_owned(),
            turn: context.ids.turn.to_owned(),
            call_id: call.id.clone(),
            name: call.name.clone(),
            retention: context
                .retention
                .get(&call.name)
                .copied()
                .unwrap_or(Retention::Ephemeral),
            is_error: output.is_error,
            ms,
        }));
        if !output.is_error
            && let Some(summary) = output.summary
        {
            summaries.insert(call.id.clone(), summary);
        }
        results.push(ToolResult {
            call_id: call.id.clone(),
            content: output.content,
            is_error: output.is_error,
        });
    }
    Batch {
        results,
        summaries,
        end,
    }
}

async fn timed_call<H: ToolHost>(host: &H, call: &ToolCall, key: &str) -> (ToolOutput, u64) {
    let started = Instant::now();
    let output = host.call(call, key).await;
    (output, elapsed_ms(started))
}

fn idempotency_key(context: &TurnContext<'_>, call: &ToolCall) -> String {
    format!("{}:{}:{}", context.ids.session, context.ids.turn, call.id)
}

fn record_output<J: Journal>(journal: &J, call: &ToolCall, output: &ToolOutput) {
    journal.append(JournalEntry::ToolOutput {
        call_id: call.id.clone(),
        output: output.clone(),
    });
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
