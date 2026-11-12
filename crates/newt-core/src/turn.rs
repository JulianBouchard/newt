use std::collections::{HashMap, HashSet};
use std::time::Instant;

use futures_util::stream::{self, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::commit::{CommitRules, committed_messages};
use crate::event::{Event, EventKind};
use crate::message::{Message, Part, Role, ToolCall, ToolResult};
use crate::prompt::{Effort, LayerFingerprint, ModelRequest, Prompt, ToolSpec};
use crate::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use crate::sink::EventSink;
use crate::tools::{
    END_TURN, EndTurn, Retention, ToolDef, ToolHost, ToolOutput, builtin_tools, run_workspace_tool,
};
use crate::workspace::Workspace;

const TOOL_CONCURRENCY: usize = 5;
const END_TURN_REMINDER: &str =
    "Every turn ends with a call to end_turn. Call it now if you are done, or keep working.";
const END_TURN_BLOCKED: &str = "Turn not ended: a tool call in this batch failed. Deal with the failure, then call end_turn again.";
const END_TURN_REPEATED: &str = "Only one end_turn per batch; this one was ignored.";
const OUTWARD_SKIPPED: &str =
    "Not run: an earlier outward call in this batch failed, so later ones were skipped.";
const OUTWARD_CANCELLED: &str = "Not run: the turn was cancelled before this call started.";

#[derive(Debug)]
pub struct TurnInput<'a> {
    pub session: &'a str,
    pub turn: &'a str,
    pub system: &'a str,
    pub external_tools: &'a [ToolDef],
    pub history: &'a [Message],
    pub events: Vec<Message>,
    pub workspace: &'a Workspace,
}

#[derive(Debug, Clone, Copy)]
pub struct TurnSettings<'a> {
    pub model: &'a str,
    pub first_call_effort: Option<Effort>,
    pub effort: Option<Effort>,
    pub max_output_tokens: u32,
    pub max_model_calls: u32,
}

#[derive(Debug, Clone)]
pub struct CallReport {
    pub usage: Usage,
    pub timing: CallTiming,
    pub stop: StopReason,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub committed: Vec<Message>,
    pub end: EndTurn,
    pub calls: Vec<CallReport>,
}

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("the model stopped twice without calling end_turn")]
    NoEndTurn,
    #[error("the turn used all {0} model calls without ending")]
    TooManyModelCalls(u32),
    #[error("two tools share the name `{0}`")]
    DuplicateTool(String),
}

#[derive(Debug, thiserror::Error)]
#[error("turn failed: {error}")]
pub struct TurnFailure {
    #[source]
    pub error: TurnError,
    pub committed: Vec<Message>,
    pub calls: Vec<CallReport>,
}

struct TurnIds<'a> {
    session: &'a str,
    turn: &'a str,
}

struct TurnContext<'a> {
    ids: TurnIds<'a>,
    system: &'a str,
    history: &'a [Message],
    workspace: &'a Workspace,
    specs: &'a [ToolSpec],
    retention: &'a HashMap<String, Retention>,
    prefix_fingerprint: LayerFingerprint,
    history_fingerprint: LayerFingerprint,
}

struct TurnState {
    scratch: Vec<Message>,
    summaries: HashMap<String, String>,
    harness_notes: HashSet<usize>,
    calls: Vec<CallReport>,
}

pub async fn run_turn<M: Model, H: ToolHost>(
    model: &M,
    host: &H,
    input: TurnInput<'_>,
    settings: &TurnSettings<'_>,
    sink: &dyn EventSink,
    cancel: &CancellationToken,
) -> Result<TurnOutcome, TurnFailure> {
    let (specs, retention) = match tool_table(input.external_tools) {
        Ok(table) => table,
        Err(error) => {
            return Err(TurnFailure {
                error,
                committed: input.events,
                calls: Vec::new(),
            });
        }
    };
    let layers = Prompt {
        system: input.system,
        tools: &specs,
        history: input.history,
        scratch: &[],
    };
    let context = TurnContext {
        ids: TurnIds {
            session: input.session,
            turn: input.turn,
        },
        system: input.system,
        history: input.history,
        workspace: input.workspace,
        specs: &specs,
        retention: &retention,
        prefix_fingerprint: layers.prefix_fingerprint(),
        history_fingerprint: layers.history_fingerprint(),
    };
    sink.emit(Event::now(EventKind::TurnStarted {
        session: input.session.to_owned(),
        turn: input.turn.to_owned(),
        history_messages: input.history.len(),
        events: input.events.len(),
    }));
    let mut state = TurnState {
        scratch: input.events,
        summaries: HashMap::new(),
        harness_notes: HashSet::new(),
        calls: Vec::new(),
    };

    let result = drive(model, host, &context, settings, &mut state, sink, cancel).await;
    let committed = committed_messages(
        state.scratch,
        &CommitRules {
            retention: &retention,
            summaries: &state.summaries,
            harness_notes: &state.harness_notes,
        },
    );
    let ids = &context.ids;
    match result {
        Ok(end) => {
            sink.emit(Event::now(EventKind::TurnCompleted {
                session: ids.session.to_owned(),
                turn: ids.turn.to_owned(),
                model_calls: u32::try_from(state.calls.len()).unwrap_or(u32::MAX),
                committed_messages: committed.len(),
            }));
            Ok(TurnOutcome {
                committed,
                end,
                calls: state.calls,
            })
        }
        Err(error) => {
            sink.emit(Event::now(EventKind::TurnFailed {
                session: ids.session.to_owned(),
                turn: ids.turn.to_owned(),
                error: error.to_string(),
                committed_messages: committed.len(),
            }));
            Err(TurnFailure {
                error,
                committed,
                calls: state.calls,
            })
        }
    }
}

async fn drive<M: Model, H: ToolHost>(
    model: &M,
    host: &H,
    context: &TurnContext<'_>,
    settings: &TurnSettings<'_>,
    state: &mut TurnState,
    sink: &dyn EventSink,
    cancel: &CancellationToken,
) -> Result<EndTurn, TurnError> {
    let mut reminded = false;
    for call in 0..settings.max_model_calls {
        let request = ModelRequest {
            model: settings.model,
            prompt: Prompt {
                system: context.system,
                tools: context.specs,
                history: context.history,
                scratch: &state.scratch,
            },
            effort: if call == 0 {
                settings.first_call_effort
            } else {
                settings.effort
            },
            max_output_tokens: settings.max_output_tokens,
            cache_key: context.ids.session,
        };
        let response = call_model(model, &request, call, context, sink, cancel).await?;
        state.calls.push(CallReport {
            usage: response.usage,
            timing: response.timing,
            stop: response.stop,
        });

        let tool_calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
        if !response.message.parts.is_empty() {
            state.scratch.push(response.message);
        }
        if tool_calls.is_empty() {
            if reminded {
                return Err(TurnError::NoEndTurn);
            }
            reminded = true;
            state.harness_notes.insert(state.scratch.len());
            state.scratch.push(Message::user_text(END_TURN_REMINDER));
            continue;
        }

        let batch = run_tools(&tool_calls, context, host, sink, cancel).await;
        state.summaries.extend(batch.summaries);
        state.scratch.push(Message {
            role: Role::User,
            parts: batch.results.into_iter().map(Part::ToolResult).collect(),
        });
        if let Some(end) = batch.end {
            return Ok(end);
        }
    }
    Err(TurnError::TooManyModelCalls(settings.max_model_calls))
}

fn tool_table(
    external_tools: &[ToolDef],
) -> Result<(Vec<ToolSpec>, HashMap<String, Retention>), TurnError> {
    let tools: Vec<ToolDef> = builtin_tools()
        .into_iter()
        .chain(external_tools.iter().cloned())
        .collect();
    let mut retention = HashMap::new();
    for tool in &tools {
        if retention
            .insert(tool.spec.name.clone(), tool.retention)
            .is_some()
        {
            return Err(TurnError::DuplicateTool(tool.spec.name.clone()));
        }
    }
    let specs = tools.into_iter().map(|tool| tool.spec).collect();
    Ok((specs, retention))
}

async fn call_model<M: Model>(
    model: &M,
    request: &ModelRequest<'_>,
    call: u32,
    context: &TurnContext<'_>,
    sink: &dyn EventSink,
    cancel: &CancellationToken,
) -> Result<ModelResponse, TurnError> {
    let ids = &context.ids;
    sink.emit(Event::now(EventKind::ModelCallStarted {
        session: ids.session.to_owned(),
        turn: ids.turn.to_owned(),
        call,
        model: request.model.to_owned(),
        effort: request.effort,
        prefix: context.prefix_fingerprint.clone(),
        history: context.history_fingerprint.clone(),
        scratch: request.prompt.scratch_fingerprint(),
    }));
    let mut on_event = |event: StreamEvent| {
        if let StreamEvent::ToolCallStarted { id, name } = event {
            sink.emit(Event::now(EventKind::ToolStarted {
                session: ids.session.to_owned(),
                turn: ids.turn.to_owned(),
                call_id: id,
                name,
            }));
        }
    };
    match model.stream(request, cancel, &mut on_event).await {
        Ok(response) => {
            sink.emit(Event::now(EventKind::ModelCallFinished {
                session: ids.session.to_owned(),
                turn: ids.turn.to_owned(),
                call,
                usage: response.usage,
                timing: response.timing,
                stop: response.stop.clone(),
                response_id: response.response_id.clone(),
                request_ids: response.request_ids.clone(),
            }));
            Ok(response)
        }
        Err(error) => {
            sink.emit(Event::now(EventKind::ModelCallFailed {
                session: ids.session.to_owned(),
                turn: ids.turn.to_owned(),
                call,
                error: error.to_string(),
            }));
            Err(error.into())
        }
    }
}

struct Batch {
    results: Vec<ToolResult>,
    summaries: HashMap<String, String>,
    end: Option<EndTurn>,
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

async fn run_tools<H: ToolHost>(
    tool_calls: &[ToolCall],
    context: &TurnContext<'_>,
    host: &H,
    sink: &dyn EventSink,
    cancel: &CancellationToken,
) -> Batch {
    let (plans, mut end, end_slot) = plan_calls(tool_calls, context);

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

    let outputs: Vec<(ToolOutput, u64)> = stream::iter(
        concurrent
            .iter()
            .map(|&slot| timed_call(host, &tool_calls[slot])),
    )
    .buffered(TOOL_CONCURRENCY)
    .collect()
    .await;
    for (&slot, output) in concurrent.iter().zip(outputs) {
        slots[slot] = Some(output);
    }

    let mut skip_reason = None;
    for slot in outward {
        if skip_reason.is_none() && cancel.is_cancelled() {
            skip_reason = Some(OUTWARD_CANCELLED);
        }
        let output = if let Some(reason) = skip_reason {
            (failure(reason), 0)
        } else {
            let output = timed_call(host, &tool_calls[slot]).await;
            if output.0.is_error {
                skip_reason = Some(OUTWARD_SKIPPED);
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

    finish_batch(tool_calls, outputs, end, context, sink)
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

async fn timed_call<H: ToolHost>(host: &H, call: &ToolCall) -> (ToolOutput, u64) {
    let started = Instant::now();
    let output = host.call(call).await;
    (output, elapsed_ms(started))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
