use super::{
    AtomicBool, BTreeMap, CallResult, CallTiming, Event, EventKind, Journal, JournalEntry, Message,
    Model, ModelRequest, ModelResponse, Ordering, Part, Replay, Steering, StreamEvent, ToolHost,
    TurnContext, TurnDeps, TurnError, TurnState,
};

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
) -> Result<Option<ModelResponse>, TurnError> {
    if let Some((message, usage, stop)) = replay.take_response() {
        deps.sink.emit(Event::now(EventKind::ModelCallReplayed {
            session: context.ids.session.to_owned(),
            turn: context.ids.turn.to_owned(),
            call,
        }));
        return Ok(Some(ModelResponse {
            message,
            usage,
            stop,
            timing: CallTiming::default(),
            response_id: None,
            request_ids: BTreeMap::new(),
        }));
    }
    match call_model(deps, request, call, context, traced).await? {
        CallResult::Response(response) => {
            deps.journal.append(JournalEntry::ModelResponse {
                call,
                message: response.message.clone(),
                usage: response.usage,
                stop: response.stop.clone(),
            });
            Ok(Some(*response))
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
    sink.emit(Event::now(EventKind::ModelCallStarted {
        session: ids.session.to_owned(),
        turn: ids.turn.to_owned(),
        call,
        model: request.model.to_owned(),
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
                output: without_reasoning(&response.message),
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
