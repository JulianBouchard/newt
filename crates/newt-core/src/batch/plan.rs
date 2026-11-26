use std::collections::HashMap;

use crate::checkpoint::{CHECKPOINT, Checkpoint};
use crate::journal::Replay;
use crate::message::ToolCall;
use crate::skills::{self, LOAD_SKILL};
use crate::tools::{
    END_TURN, EndTurn, REMEMBER, Retention, ToolOutput, is_workspace_tool, remember,
};
use crate::turn::TurnContext;

pub(super) const END_TURN_BLOCKED: &str = "Turn not ended: a tool call in this batch failed. Deal with the failure, then call end_turn again.";
const END_TURN_REPEATED: &str = "Only one end_turn per batch; this one was ignored.";
pub(super) const OUTWARD_SKIPPED: &str =
    "Not run: an earlier outward call in this batch failed, so later ones were skipped.";
pub(super) const OUTWARD_CANCELLED: &str =
    "Not run: the turn was cancelled before this call started.";
const CHECKPOINT_OUTSIDE: &str = "Only call checkpoint when newt asks you to compact.";
const CHECKPOINT_ALONE: &str =
    "Call checkpoint on its own, after your saves have succeeded, not alongside other calls.";
const INTERRUPTED: &str = "Interrupted: newt stopped while this call was running, so it may or may not have taken effect. Check the current state before calling it again.";
const NO_SENDS_WHILE_COMPACTING: &str = "Not run: nothing is sent while compacting.";
const END_TURN_WHILE_COMPACTING: &str = "Compaction ends with checkpoint, not end_turn.";

pub(super) enum Plan {
    Done(ToolOutput, u64),
    Skill(usize),
    Workspace,
    Concurrent,
    Outward,
}

pub(super) fn failure(content: impl Into<String>) -> ToolOutput {
    ToolOutput::error(content)
}

fn success(content: impl Into<String>) -> ToolOutput {
    ToolOutput::ok(content)
}

pub(super) struct Planned {
    pub(super) plans: Vec<Plan>,
    pub(super) end: Option<EndTurn>,
    pub(super) end_slot: Option<usize>,
    pub(super) checkpoint: Option<Checkpoint>,
}

pub(super) fn plan_calls(
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
                if call.name == LOAD_SKILL && !output.is_error {
                    context.mark_skill_loaded(call);
                }
                return Plan::Done(output.clone(), 0);
            }
            if !context.retention.contains_key(&call.name) {
                return Plan::Done(failure(format!("unknown tool `{}`", call.name)), 0);
            }
            if call.name == LOAD_SKILL {
                if let Some(index) = skills::served(call, context.skills) {
                    return Plan::Skill(index);
                }
                let output = skills::load(call, context.skills);
                if !output.is_error {
                    context.mark_skill_loaded(call);
                }
                return Plan::Done(output, 0);
            }
            if call.name == CHECKPOINT {
                return Plan::Done(plan_checkpoint(call, tool_calls.len(), context, &mut checkpoint), 0);
            }
            if call.name == END_TURN {
                if context.ending.is_compaction() {
                    return Plan::Done(failure(END_TURN_WHILE_COMPACTING), 0);
                }
                if end_slot.is_some() {
                    return Plan::Done(failure(END_TURN_REPEATED), 0);
                }
                end_slot = Some(slot);
                return Plan::Done(
                    match EndTurn::parse(&call.arguments, context.ending.end_turn_must_record()) {
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
                Some(Retention::Outward) if context.ending.is_compaction() => {
                    Plan::Done(failure(NO_SENDS_WHILE_COMPACTING), 0)
                }
                Some(Retention::Outward) => Plan::Outward,
                Some(Retention::Durable)
                    if replay.started(&call.id) && !context.idempotent.contains(&call.name) =>
                {
                    Plan::Done(failure(INTERRUPTED), 0)
                }
                _ => Plan::Concurrent,
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
    let Some(compaction) = context.ending.compaction() else {
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
