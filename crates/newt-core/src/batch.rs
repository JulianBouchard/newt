use std::collections::{HashMap, HashSet};

use futures_util::future::join_all;
use futures_util::stream::{self, StreamExt};

use crate::batch_tool::{expand, project};
use crate::checkpoint::Checkpoint;
use crate::journal::{Journal, Replay};
use crate::message::{ToolCall, ToolResult};
use crate::tools::{CallContext, END_TURN, EndTurn, ToolHost, ToolOutput};
use crate::turn::{TurnContext, TurnDeps};

mod calls;
mod early;
mod finish;
mod outward;
mod plan;

use calls::{call_context, record_output, timed_call, timed_read, timed_skill_load};
pub(crate) use early::start_early;
use early::take_early;
use finish::{apply_workspace_changes, ended_by_tool, finish_batch};
use outward::run_outward;
use plan::{END_TURN_BLOCKED, Plan, Planned, failure, plan_calls};

pub(crate) struct Batch {
    pub(crate) results: Vec<ToolResult>,
    pub(crate) summaries: HashMap<String, String>,
    pub(crate) kept: HashSet<String>,
    pub(crate) end: Option<EndTurn>,
    pub(crate) checkpoint: Option<Checkpoint>,
    pub(crate) effects: bool,
}

const TOOL_CONCURRENCY: usize = 5;

pub(crate) type Timed = (ToolOutput, u64);

/// Outputs of calls that started while the model was still streaming, by call id.
pub(crate) type Early = HashMap<String, Timed>;

/// Which slots run where: done already, workspace reads, served skill loads
/// (with the skill's index), concurrent host calls, and outward calls in order.
struct Lanes {
    slots: Vec<Option<Timed>>,
    workspace_reads: Vec<usize>,
    skill_loads: Vec<(usize, usize)>,
    concurrent: Vec<usize>,
    outward: Vec<usize>,
}

impl Lanes {
    fn of(plans: Vec<Plan>) -> Self {
        let mut lanes = Self {
            slots: Vec::with_capacity(plans.len()),
            workspace_reads: Vec::new(),
            skill_loads: Vec::new(),
            concurrent: Vec::new(),
            outward: Vec::new(),
        };
        for (slot, plan) in plans.into_iter().enumerate() {
            match plan {
                Plan::Done(output, ms) => {
                    lanes.slots.push(Some((output, ms)));
                    continue;
                }
                Plan::Skill(index) => lanes.skill_loads.push((slot, index)),
                Plan::Workspace => lanes.workspace_reads.push(slot),
                Plan::Concurrent => lanes.concurrent.push(slot),
                Plan::Outward => lanes.outward.push(slot),
            }
            lanes.slots.push(None);
        }
        lanes
    }
}

pub(crate) async fn run_tools<M, H: ToolHost, S, J: Journal>(
    model_calls: &[ToolCall],
    context: &TurnContext<'_>,
    deps: &TurnDeps<'_, M, H, S, J>,
    replay: &Replay,
    early: &mut Early,
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

    let Lanes {
        mut slots,
        workspace_reads,
        skill_loads,
        concurrent,
        outward,
    } = Lanes::of(plans);

    let workspace_reads = take_early::<J>(tool_calls, workspace_reads, &mut slots, early, None);
    let concurrent = take_early(tool_calls, concurrent, &mut slots, early, Some(journal));
    let contexts: Vec<CallContext> = concurrent
        .iter()
        .map(|&slot| call_context(context, &tool_calls[slot]))
        .collect();
    let calls: Vec<_> = concurrent
        .iter()
        .zip(&contexts)
        .map(|(&slot, call_context)| {
            timed_call(host, &tool_calls[slot], call_context, context, deps.sink)
        })
        .collect();
    let reads: Vec<_> = workspace_reads
        .iter()
        .map(|&slot| timed_read(context, &tool_calls[slot]))
        .collect();
    let loads: Vec<_> = skill_loads
        .iter()
        .map(|&(slot, index)| timed_skill_load(host, &tool_calls[slot], &context.skills[index]))
        .collect();
    let (read_outputs, load_outputs, outputs): (Vec<Timed>, Vec<Timed>, Vec<Timed>) = futures_util::join!(
        join_all(reads),
        join_all(loads),
        stream::iter(calls).buffered(TOOL_CONCURRENCY).collect()
    );
    for (&slot, output) in workspace_reads.iter().zip(read_outputs) {
        slots[slot] = Some(output);
    }
    for (&(slot, _), output) in skill_loads.iter().zip(load_outputs) {
        let call = &tool_calls[slot];
        if !output.0.is_error {
            context.mark_skill_loaded(call);
        }
        record_output(journal, call, &output.0);
        slots[slot] = Some(output);
    }
    for (&slot, output) in concurrent.iter().zip(outputs) {
        record_output(journal, &tool_calls[slot], &output.0);
        slots[slot] = Some(output);
    }

    run_outward(tool_calls, &outward, &mut slots, context, deps, replay).await;

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
    if end.is_none() && !batch_failed && context.compaction.is_none() {
        end = ended_by_tool(tool_calls, &outputs, context);
    }

    apply_workspace_changes(tool_calls, &mut outputs, context, replay);
    let mut batch = finish_batch(&expanded, outputs, end, context, deps.sink);
    batch.checkpoint = checkpoint;
    batch
}
