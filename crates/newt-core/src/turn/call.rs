use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use super::coalesce::{TextCoalescer, ToolInputs};
use super::state::TurnState;
use super::{TurnContext, TurnDeps, TurnError};
use crate::event::{Event, EventKind};
use crate::journal::{Journal, JournalEntry, Replay};
use crate::message::{Message, Part};
use crate::prompt::ModelRequest;
use crate::provider::{CallTiming, Model, ModelResponse, StreamEvent};
use crate::steering::Steering;
use crate::tools::ToolHost;

pub(super) enum CallResult {
    Response(Box<ModelResponse>, crate::batch::Early),
    Interrupted,
}

pub(super) async fn take_arrivals<M, H, S: Steering, J: Journal>(
    deps: &TurnDeps<'_, M, H, S, J>,
    state: &mut TurnState,
) -> Vec<crate::steering::Steered> {
    if !state.replay.exhausted() {
        return state.replay.take_steered();
    }
    let fresh = deps.steering.take_new().await;
    for steered in &fresh {
        deps.journal.append(JournalEntry::Steered {
            id: steered.id.clone(),
            message: steered.message.clone(),
        });
    }
    fresh
}

pub(super) async fn next_response<M: Model, H: ToolHost, S: Steering, J: Journal>(
    deps: &TurnDeps<'_, M, H, S, J>,
    request: &ModelRequest<'_>,
    call: u32,
    context: &TurnContext<'_>,
    replay: &mut Replay,
    traced: usize,
) -> Result<Option<(ModelResponse, crate::batch::Early)>, TurnError> {
    if let Some((message, usage, stop)) = replay.take_response() {
        deps.sink.emit(Event::now(EventKind::ModelCallReplayed {
            session: context.ids.session.to_owned(),
            turn: context.ids.turn.to_owned(),
            call,
        }));
        return Ok(Some((
            ModelResponse {
                message,
                usage,
                stop,
                timing: CallTiming::default(),
                response_id: None,
                request_ids: BTreeMap::new(),
            },
            crate::batch::Early::new(),
        )));
    }
    match call_model(deps, request, call, context, traced).await? {
        CallResult::Response(response, early) => {
            deps.journal.append(JournalEntry::ModelResponse {
                call,
                message: response.message.clone(),
                usage: response.usage,
                stop: response.stop.clone(),
            });
            Ok(Some((*response, early)))
        }
        CallResult::Interrupted => Ok(None),
    }
}

pub(super) async fn call_model<M: Model, H: ToolHost, S: Steering, J: Journal>(
    deps: &TurnDeps<'_, M, H, S, J>,
    request: &ModelRequest<'_>,
    call: u32,
    context: &TurnContext<'_>,
    traced: usize,
) -> Result<CallResult, TurnError> {
    let ids = &context.ids;
    let sink = deps.sink;
    sink.emit(model_call_started(request, call, context, traced));
    let output_started = AtomicBool::new(false);
    let (ready_tx, mut ready) = tokio::sync::mpsc::unbounded_channel();
    let mut text = TextCoalescer::default();
    let emit_text = |text: String| {
        sink.emit(Event::now(EventKind::TextDelta {
            session: ids.session.to_owned(),
            turn: ids.turn.to_owned(),
            text,
        }));
    };
    let mut inputs = ToolInputs::default();
    let emit_input = |call_id: &str, name: &str, delta: String| {
        sink.emit(Event::now(EventKind::ToolInputDelta {
            session: ids.session.to_owned(),
            turn: ids.turn.to_owned(),
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            delta,
        }));
    };
    let mut on_event = |event: StreamEvent| {
        output_started.store(true, Ordering::Relaxed);
        match event {
            StreamEvent::TextDelta(delta) => text.push(&delta, &emit_text),
            StreamEvent::ToolCallStarted { id, name } => {
                text.flush(&emit_text);
                inputs.start(id.clone(), name.clone());
                sink.emit(Event::now(EventKind::ToolStarted {
                    session: ids.session.to_owned(),
                    turn: ids.turn.to_owned(),
                    call_id: id,
                    name,
                }));
            }
            StreamEvent::ToolInputDelta { id, delta } => inputs.push(&id, &delta, &emit_input),
            StreamEvent::ToolCallReady(call) => {
                inputs.flush(&call.id, &emit_input);
                sink.emit(Event::now(EventKind::ToolInput {
                    session: ids.session.to_owned(),
                    turn: ids.turn.to_owned(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.arguments.clone(),
                }));
                let _ = ready_tx.send(call);
            }
            StreamEvent::OutputStarted => {}
        }
    };
    let mut running = futures_util::stream::FuturesUnordered::new();
    let mut early = crate::batch::Early::new();
    let result = {
        let stream = deps.model.stream(request, deps.cancel, &mut on_event);
        tokio::pin!(stream);
        let arrived = deps.steering.arrived();
        tokio::pin!(arrived);
        let mut listening = true;
        loop {
            tokio::select! {
                result = &mut stream => break result,
                Some(ready_call) = ready.recv() => {
                    if let Some(started) = crate::batch::start_early(ready_call, context, deps) {
                        running.push(started);
                    }
                }
                Some((id, output)) = futures_util::StreamExt::next(&mut running), if !running.is_empty() => {
                    early.insert(id, output);
                }
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
        }
    };
    text.flush(&emit_text);
    inputs.flush_all(&emit_input);
    let response = call_outcome(sink, ids, call, result)?;
    while let Some((id, output)) = futures_util::StreamExt::next(&mut running).await {
        early.insert(id, output);
    }
    Ok(CallResult::Response(Box::new(response), early))
}

fn call_outcome(
    sink: &dyn crate::sink::EventSink,
    ids: &super::TurnIds<'_>,
    call: u32,
    result: Result<ModelResponse, crate::provider::ProviderError>,
) -> Result<ModelResponse, TurnError> {
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
                output: without_reasoning(&response.message),
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

fn model_call_started(
    request: &ModelRequest<'_>,
    call: u32,
    context: &TurnContext<'_>,
    traced: usize,
) -> Event {
    Event::now(EventKind::ModelCallStarted {
        session: context.ids.session.to_owned(),
        turn: context.ids.turn.to_owned(),
        call,
        model: request.model.to_owned(),
        cache_key: request.cache_key.to_owned(),
        effort: request.effort,
        prefix: context.prefix_fingerprint.clone(),
        history: context.history_fingerprint.clone(),
        scratch: request.prompt.scratch_fingerprint(),
        input: request
            .prompt
            .scratch
            .get(traced..)
            .unwrap_or_default()
            .iter()
            .map(without_reasoning)
            .collect(),
    })
}

fn without_reasoning(message: &Message) -> Message {
    Message {
        role: message.role,
        parts: message
            .parts
            .iter()
            .filter(|part| !matches!(part, Part::Reasoning(_)))
            .cloned()
            .collect(),
    }
}
