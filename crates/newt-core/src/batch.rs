use std::collections::{HashMap, HashSet};
use std::time::Instant;

use futures_util::future::join_all;
use futures_util::stream::{self, StreamExt};

use crate::batch_tool::{Expanded, expand, fold, project};
use crate::checkpoint::{CHECKPOINT, Checkpoint};
use crate::event::{Event, EventKind};
use crate::journal::{Journal, JournalEntry, Replay};
use crate::message::{ToolCall, ToolResult};
use crate::sink::EventSink;
use crate::tools::{
    END_TURN, EndTurn, REMEMBER, Retention, ToolHost, ToolOutput, is_workspace_tool, remember,
    run_workspace_tool,
};
use crate::turn::{TurnContext, TurnDeps};

const TOOL_CONCURRENCY: usize = 5;

type Timed = (ToolOutput, u64);
const END_TURN_BLOCKED: &str = "Turn not ended: a tool call in this batch failed. Deal with the failure, then call end_turn again.";
const END_TURN_REPEATED: &str = "Only one end_turn per batch; this one was ignored.";
const OUTWARD_SKIPPED: &str =
    "Not run: an earlier outward call in this batch failed, so later ones were skipped.";
const OUTWARD_CANCELLED: &str = "Not run: the turn was cancelled before this call started.";
const CHECKPOINT_OUTSIDE: &str = "Only call checkpoint when newt asks you to compact.";
const CHECKPOINT_ALONE: &str =
    "Call checkpoint on its own, after your saves have succeeded, not alongside other calls.";
const NO_SENDS_WHILE_COMPACTING: &str = "Not run: nothing is sent while compacting.";
const END_TURN_WHILE_COMPACTING: &str = "Compaction ends with checkpoint, not end_turn.";

pub(crate) struct Batch {
    pub(crate) results: Vec<ToolResult>,
    pub(crate) summaries: HashMap<String, String>,
    pub(crate) kept: HashSet<String>,
    pub(crate) end: Option<EndTurn>,
    pub(crate) checkpoint: Option<Checkpoint>,
    pub(crate) effects: bool,
}

enum Plan {
    Done(ToolOutput, u64),
    Workspace,
    Concurrent,
    Outward,
}

fn failure(content: impl Into<String>) -> ToolOutput {
    ToolOutput::error(content)
}

fn success(content: impl Into<String>) -> ToolOutput {
    ToolOutput::ok(content)
}

struct Planned {
    plans: Vec<Plan>,
    end: Option<EndTurn>,
    end_slot: Option<usize>,
    checkpoint: Option<Checkpoint>,
}

fn plan_calls(
    tool_calls: &[ToolCall],
    preset: &HashMap<String, ToolOutput>,
    context: &TurnContext<'_>,
    replay: &Replay,
) -> Planned {
    let mut end = None;
    let mut end_slot = None;
    let mut checkpoint = None;
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
            if let Some(output) = preset.get(&call.id) {
                return Plan::Done(output.clone(), 0);
            }
            if let Some(output) = replay.output(&call.id) {
                return Plan::Done(output.clone(), 0);
            }
            if call.name == CHECKPOINT {
                return Plan::Done(plan_checkpoint(call, tool_calls.len(), context, &mut checkpoint), 0);
            }
            if call.name == END_TURN {
                if context.compaction.is_some() {
                    return Plan::Done(failure(END_TURN_WHILE_COMPACTING), 0);
                }
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
            if call.name == REMEMBER {
                return Plan::Done(remember(call), 0);
            }
            if is_workspace_tool(&call.name) {
                return Plan::Workspace;
            }
            match context.retention.get(&call.name) {
                Some(Retention::Outward) if context.compaction.is_some() => {
                    Plan::Done(failure(NO_SENDS_WHILE_COMPACTING), 0)
                }
                Some(Retention::Outward) => Plan::Outward,
                Some(_) => Plan::Concurrent,
                None => Plan::Done(failure(format!("unknown tool `{}`", call.name)), 0),
            }
        })
        .collect();
    Planned {
        plans,
        end,
        end_slot,
        checkpoint,
    }
}

fn plan_checkpoint(
    call: &ToolCall,
    calls_in_batch: usize,
    context: &TurnContext<'_>,
    accepted: &mut Option<Checkpoint>,
) -> ToolOutput {
    let Some(compaction) = context.compaction else {
        return failure(CHECKPOINT_OUTSIDE);
    };
    if calls_in_batch != 1 {
        return failure(CHECKPOINT_ALONE);
    }
    match Checkpoint::accept(
        call,
        compaction.open_commitments,
        compaction.max_checkpoint_chars,
    ) {
        Ok(checkpoint) => {
            *accepted = Some(checkpoint);
            success("Checkpoint saved.")
        }
        Err(message) => failure(message),
    }
}

pub(crate) async fn run_tools<M, H: ToolHost, S, J: Journal>(
    model_calls: &[ToolCall],
    context: &TurnContext<'_>,
    deps: &TurnDeps<'_, M, H, S, J>,
    replay: &Replay,
) -> Batch {
    let expanded = expand(model_calls, context.retention);
    let tool_calls = expanded.calls.as_slice();
    let (host, journal) = (deps.host, deps.journal);
    let Planned {
        plans,
        mut end,
        end_slot,
        checkpoint,
    } = plan_calls(tool_calls, &expanded.preset, context, replay);

    let mut slots: Vec<Option<(ToolOutput, u64)>> = Vec::with_capacity(plans.len());
    let mut workspace_reads = Vec::new();
    let mut concurrent = Vec::new();
    let mut outward = Vec::new();
    for (slot, plan) in plans.into_iter().enumerate() {
        match plan {
            Plan::Done(output, ms) => slots.push(Some((output, ms))),
            Plan::Workspace => {
                workspace_reads.push(slot);
                slots.push(None);
            }
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
    let reads: Vec<_> = workspace_reads
        .iter()
        .map(|&slot| timed_read(context, &tool_calls[slot]))
        .collect();
    let (read_outputs, outputs): (Vec<Timed>, Vec<Timed>) = futures_util::join!(
        join_all(reads),
        stream::iter(calls).buffered(TOOL_CONCURRENCY).collect()
    );
    for (&slot, output) in workspace_reads.iter().zip(read_outputs) {
        slots[slot] = Some(output);
    }
    for (&slot, output) in concurrent.iter().zip(outputs) {
        record_output(journal, &tool_calls[slot], &output.0);
        slots[slot] = Some(output);
    }

    run_outward(
        tool_calls, &outward, &keys, &mut slots, context, deps, replay,
    )
    .await;

    let mut outputs: Vec<(ToolOutput, u64)> = slots
        .into_iter()
        .map(|slot| {
            slot.unwrap_or_else(|| (failure("internal error: tool was never scheduled"), 0))
        })
        .collect();
    for (slot, projection) in &expanded.projections {
        if let Some((output, _)) = outputs.get_mut(*slot)
            && !output.is_error
        {
            output.content = project(&output.content, projection);
        }
    }
    let batch_failed = tool_calls
        .iter()
        .zip(&outputs)
        .any(|(call, (output, _))| output.is_error && call.name != END_TURN);
    if batch_failed
        && end.take().is_some()
        && let Some(slot) = end_slot
        && let Some(entry) = outputs.get_mut(slot)
    {
        entry.0 = failure(END_TURN_BLOCKED);
    }

    apply_workspace_changes(tool_calls, &mut outputs, context, replay);
    let mut batch = finish_batch(&expanded, outputs, end, context, deps.sink);
    batch.checkpoint = checkpoint;
    batch
}

fn finish_batch(
    expanded: &Expanded,
    outputs: Vec<(ToolOutput, u64)>,
    end: Option<EndTurn>,
    context: &TurnContext<'_>,
    sink: &dyn EventSink,
) -> Batch {
    let tool_calls = expanded.calls.as_slice();
    let mut summaries = HashMap::new();
    let mut results = Vec::with_capacity(outputs.len());
    let effects = tool_calls.iter().zip(&outputs).any(|(call, (output, _))| {
        !output.is_error
            && call.name != REMEMBER
            && matches!(
                context.retention.get(&call.name),
                Some(Retention::Durable | Retention::Outward)
            )
    });
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
        results.push(Some(ToolResult {
            call_id: call.id.clone(),
            content: output.content,
            is_error: output.is_error,
        }));
    }
    let folded = fold(expanded, results, summaries, context.retention);
    Batch {
        results: folded.results,
        summaries: folded.summaries,
        kept: folded.kept,
        end,
        checkpoint: None,
        effects,
    }
}

async fn run_outward<M, H: ToolHost, S, J: Journal>(
    tool_calls: &[ToolCall],
    outward: &[usize],
    keys: &[String],
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
            let output = timed_call(deps.host, call, &keys[slot]).await;
            record_output(deps.journal, call, &output.0);
            if output.0.is_error {
                skip_reason = Some(OUTWARD_SKIPPED.to_owned());
            }
            output
        };
        slots[slot] = Some(output);
    }
}

fn apply_workspace_changes(
    tool_calls: &[ToolCall],
    outputs: &mut [(ToolOutput, u64)],
    context: &TurnContext<'_>,
    replay: &Replay,
) {
    let workspace = context.workspace;
    let mut changed = false;
    for (call, (output, _)) in tool_calls.iter().zip(outputs.iter_mut()) {
        if output.is_error {
            continue;
        }
        let mut prefixes = std::mem::take(&mut output.invalidate);
        if let Some(declared) = context.invalidates.get(&call.name) {
            prefixes.extend(declared.iter().cloned());
        }
        let mut touched = workspace.invalidate(&prefixes);
        let pages = std::mem::take(&mut output.pages);
        if replay.output(&call.id).is_none() {
            touched.extend(workspace.put(pages));
        }
        if touched.is_empty() {
            continue;
        }
        changed = true;
        touched.sort();
        touched.dedup();
        output.content.push_str(
            "
(workspace pages changed: ",
        );
        output.content.push_str(&touched.join(", "));
        output.content.push(')');
    }
    if changed {
        workspace.refresh_read_pages_in_background();
    }
}

async fn timed_read(context: &TurnContext<'_>, call: &ToolCall) -> (ToolOutput, u64) {
    let started = Instant::now();
    let output = run_workspace_tool(call, context.workspace).await;
    (output, elapsed_ms(started))
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
