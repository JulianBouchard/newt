use std::collections::HashMap;

use super::Batch;
use crate::batch_tool::{BATCH, Expanded, fold};
use crate::event::{Event, EventKind};
use crate::journal::Replay;
use crate::message::{ToolCall, ToolResult};
use crate::sink::EventSink;
use crate::tools::{EndTurn, REMEMBER, Retention, ToolOutput};
use crate::turn::TurnContext;

/// The end of the turn when a successful call of a tool that ends it (its
/// definition's `ends_turn`, or its result's override) is in a batch with no failures.
pub(super) fn ended_by_tool(
    tool_calls: &[ToolCall],
    outputs: &[(ToolOutput, u64)],
    context: &TurnContext<'_>,
) -> Option<EndTurn> {
    tool_calls
        .iter()
        .zip(outputs)
        .find(|(call, (output, _))| {
            !output.is_error
                && output
                    .ends_turn
                    .unwrap_or_else(|| context.ends_turn.contains(&call.name))
        })
        .map(|(call, _)| EndTurn::by_tool(&call.name))
}

pub(super) fn finish_batch(
    expanded: &Expanded,
    outputs: Vec<(ToolOutput, u64)>,
    end: Option<EndTurn>,
    context: &TurnContext<'_>,
    sink: &dyn EventSink,
) -> Batch {
    let tool_calls = expanded.calls.as_slice();
    let mut summaries = HashMap::new();
    let mut results = Vec::with_capacity(outputs.len());
    let durations: Vec<u64> = outputs.iter().map(|(_, ms)| *ms).collect();
    let effects = tool_calls.iter().zip(&outputs).any(|(call, (output, _))| {
        !output.is_error
            && call.name != REMEMBER
            && matches!(
                context.retention.get(&call.name),
                Some(Retention::Durable | Retention::Outward)
            )
    });
    for (call, (output, ms)) in tool_calls.iter().zip(outputs) {
        sink.emit(Event::now(EventKind::ToolOutput {
            session: context.ids.session.to_owned(),
            turn: context.ids.turn.to_owned(),
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: crate::event::live_excerpt(
                &output.content,
                crate::event::TOOL_OUTPUT_LIVE_CHARS,
            ),
            is_error: output.is_error,
        }));
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
    let folded = fold(expanded, results, summaries, context.retention, &durations);
    for result in &folded.results {
        if let Some(inner) = folded.batch_ms.get(&result.call_id) {
            sink.emit(Event::now(EventKind::ToolFinished {
                session: context.ids.session.to_owned(),
                turn: context.ids.turn.to_owned(),
                call_id: result.call_id.clone(),
                name: BATCH.to_owned(),
                retention: Retention::Ephemeral,
                is_error: result.is_error,
                ms: *inner,
            }));
        }
    }
    Batch {
        results: folded.results,
        summaries: folded.summaries,
        kept: folded.kept,
        end,
        checkpoint: None,
        effects,
    }
}

pub(super) fn apply_workspace_changes(
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
        let prefetch = std::mem::take(&mut output.prefetch);
        if replay.output(&call.id).is_none() {
            workspace.prefetch(prefetch);
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
