use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use crate::commit::{CommitRules, committed_messages};
use crate::event::{Event, EventKind};
use crate::message::{Message, Part, Role, ToolCall};
use crate::prompt::{Effort, Lane, LayerFingerprint, ModelRequest, Prompt, ToolSpec};
use crate::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use crate::sink::EventSink;
use crate::steering::Steering;
use crate::tools::{EndTurn, Retention, ToolDef, ToolHost, builtin_tools};
use crate::workspace::Workspace;

const END_TURN_REMINDER: &str =
    "Every turn ends with a call to end_turn. Call it now if you are done, or keep working.";

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
    pub lane: Lane,
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
    pub steered: Vec<String>,
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
    pub steered: Vec<String>,
}

pub(crate) struct TurnIds<'a> {
    pub(crate) session: &'a str,
    pub(crate) turn: &'a str,
}

pub(crate) struct TurnContext<'a> {
    pub(crate) ids: TurnIds<'a>,
    system: &'a str,
    history: &'a [Message],
    pub(crate) workspace: &'a Workspace,
    specs: &'a [ToolSpec],
    pub(crate) retention: &'a HashMap<String, Retention>,
    prefix_fingerprint: LayerFingerprint,
    history_fingerprint: LayerFingerprint,
}

struct Deps<'a, M, H, S> {
    model: &'a M,
    host: &'a H,
    steering: &'a S,
    sink: &'a dyn EventSink,
    cancel: &'a CancellationToken,
}

enum CallResult {
    Response(Box<ModelResponse>),
    Interrupted,
}

struct TurnState {
    steered: Vec<String>,
    scratch: Vec<Message>,
    summaries: HashMap<String, String>,
    harness_notes: HashSet<usize>,
    calls: Vec<CallReport>,
}

#[expect(
    clippy::result_large_err,
    reason = "a failed turn is rare and must hand back what it committed"
)]
pub async fn run_turn<M: Model, H: ToolHost, S: Steering>(
    model: &M,
    host: &H,
    steering: &S,
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
                steered: Vec::new(),
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
        steered: Vec::new(),
        scratch: input.events,
        summaries: HashMap::new(),
        harness_notes: HashSet::new(),
        calls: Vec::new(),
    };

    let deps = Deps {
        model,
        host,
        steering,
        sink,
        cancel,
    };
    let result = drive(&deps, &context, settings, &mut state).await;
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
                steered: state.steered,
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
                steered: state.steered,
            })
        }
    }
}

async fn drive<M: Model, H: ToolHost, S: Steering>(
    deps: &Deps<'_, M, H, S>,
    context: &TurnContext<'_>,
    settings: &TurnSettings<'_>,
    state: &mut TurnState,
) -> Result<EndTurn, TurnError> {
    let mut reminded = false;
    for call in 0..settings.max_model_calls {
        let arrived = deps.steering.take_new().await;
        if !arrived.is_empty() {
            deps.sink.emit(Event::now(EventKind::TurnSteered {
                session: context.ids.session.to_owned(),
                turn: context.ids.turn.to_owned(),
                events: arrived.len(),
            }));
            for steered in arrived {
                state.scratch.push(steered.message);
                state.steered.push(steered.id);
            }
        }
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
            lane: settings.lane,
        };
        let response = match call_model(deps, &request, call, context).await? {
            CallResult::Response(response) => *response,
            CallResult::Interrupted => continue,
        };
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

        let batch =
            crate::batch::run_tools(&tool_calls, context, deps.host, deps.sink, deps.cancel).await;
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

async fn call_model<M: Model, H: ToolHost, S: Steering>(
    deps: &Deps<'_, M, H, S>,
    request: &ModelRequest<'_>,
    call: u32,
    context: &TurnContext<'_>,
) -> Result<CallResult, TurnError> {
    let ids = &context.ids;
    let sink = deps.sink;
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
    let output_started = AtomicBool::new(false);
    let mut on_event = |event: StreamEvent| {
        output_started.store(true, Ordering::Relaxed);
        if let StreamEvent::ToolCallStarted { id, name } = event {
            sink.emit(Event::now(EventKind::ToolStarted {
                session: ids.session.to_owned(),
                turn: ids.turn.to_owned(),
                call_id: id,
                name,
            }));
        }
    };
    let stream = deps.model.stream(request, deps.cancel, &mut on_event);
    tokio::pin!(stream);
    let arrived = deps.steering.arrived();
    tokio::pin!(arrived);
    let mut listening = true;
    let result = loop {
        tokio::select! {
            result = &mut stream => break result,
            () = &mut arrived, if listening => {
                if output_started.load(Ordering::Relaxed) {
                    listening = false;
                } else {
                    sink.emit(Event::now(EventKind::ModelCallInterrupted {
                        session: ids.session.to_owned(),
                        turn: ids.turn.to_owned(),
                        call,
                    }));
                    return Ok(CallResult::Interrupted);
                }
            }
        }
    };
    match result {
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
            Ok(CallResult::Response(Box::new(response)))
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
