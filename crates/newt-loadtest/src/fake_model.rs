use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use serde_json::Value;

use crate::cache::prompt_bytes;
pub use crate::plan::Finish;
use crate::plan::{finish, pick_tools, read_turn, seed_of};
use crate::sample::Rng;

mod model;
mod request;
mod step;

pub use model::{FakeModel, FakeOptions, Stats};
use request::{look_up, record_shape, tool_schemas};
use step::{audit_reply, call_rng, calls_json, replay_step, tail_tokens};

pub fn router(model: Arc<FakeModel>) -> axum::Router {
    axum::Router::new()
        .route("/stats", get(stats))
        .fallback(respond)
        .with_state(model)
}

async fn stats(State(model): State<Arc<FakeModel>>) -> Json<Value> {
    let mut snapshot = model.stats.snapshot();
    if let (Some(totals), Value::Object(audit)) = (snapshot.as_object_mut(), model.audit.snapshot())
    {
        totals.extend(audit);
    }
    Json(snapshot)
}

#[expect(
    clippy::too_many_lines,
    reason = "one handler walks the whole scripted reply"
)]
async fn respond(State(model): State<Arc<FakeModel>>, Json(body): Json<Value>) -> Response {
    model.stats.calls.fetch_add(1, Ordering::Relaxed);
    let timing = model.timing;
    let prompt = prompt_bytes(&body);
    record_shape(&model, &body).await;
    let lookup = look_up(&model, &body, &prompt);
    let uncached = lookup.input_tokens - lookup.cached_tokens;
    #[expect(clippy::cast_precision_loss, reason = "token counts far below 2^52")]
    let prefill = Duration::from_secs_f64(
        uncached as f64 / 1000.0 * timing.prefill_ms_per_1k_uncached
            / 1000.0
            / model.speed.max(0.001),
    );
    let input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let turn = read_turn(&input, model.turn_marker.as_deref());
    let rng = call_rng(&turn.last_user, turn.done_calls);
    let arrived = std::time::SystemTime::now();
    let tag = crate::intervals::turn_key(&turn.last_user);
    let tools = tool_schemas(&body);
    let replayed = replay_step(&model, tag, &turn, &tools, &rng);
    let wait = match &replayed {
        Some(step) => {
            Duration::from_secs_f64(step.first_token_ms / 1000.0 / model.speed.max(0.001))
        }
        None => timing.headers_ms.millis(&rng, model.speed) + prefill,
    };
    tokio::time::sleep(wait).await;
    if model.rng.chance(model.failure_rate) {
        if let Some(tag) = tag {
            model.intervals.record(tag, "model", arrived, wait);
        }
        model.stats.failures.fetch_add(1, Ordering::Relaxed);
        return (StatusCode::SERVICE_UNAVAILABLE, "fake overload").into_response();
    }
    model.audit.observe_outputs(&input);
    let in_replay = model.replay.is_some();
    let target = match &replayed {
        Some(step) => turn.done_calls + step.calls.len(),
        None if in_replay => turn.done_calls,
        None => timing
            .tool_calls_per_turn
            .count(&Rng::new(seed_of(&turn.last_user))),
    };
    let stream_ms = replayed.as_ref().map(|step| step.stream_ms);
    let schedule = replayed.as_ref().map(|step| crate::stream::Schedule {
        call_ready_ms: step
            .ready_ms
            .iter()
            .map(|ready| (ready - step.first_token_ms).max(0.0))
            .collect(),
        end_ms: step.stream_ms,
    });
    let (mut calls, reply) = if let Some(step) = replayed.filter(|step| !step.reply) {
        (step.calls, false)
    } else if turn.compacting || turn.done_calls >= target {
        let ending = finish(&tools, &turn, model.finish);
        (ending.calls, ending.reply)
    } else {
        let step = timing
            .tool_calls_per_step
            .count(&rng)
            .min(target - turn.done_calls);
        (pick_tools(&model.profile, &rng, &tools, step.max(1)), false)
    };
    audit_reply(&model, &body, &tools, &turn, target, &calls);
    if let Some(tag) = tag {
        for call in &mut calls {
            crate::intervals::tag_arguments(&mut call.arguments, tag);
        }
    }
    let (input_tokens, cached) = (lookup.input_tokens, lookup.cached_tokens);
    let text = reply.then(|| rng.words(timing.text_tokens.count(&rng)));
    let calls = calls_json(&model, &calls);
    model.audit.emitted(&calls, turn.done_calls, target);
    let events = crate::stream::response_events(
        &model,
        crate::stream::Reply {
            model_name: body.get("model").and_then(Value::as_str).unwrap_or("fake"),
            calls: &calls,
            text,
            input_tokens,
            cached,
            tail_tokens: if in_replay {
                0
            } else {
                tail_tokens(&timing, &rng)
            },
            schedule,
        },
    );
    model
        .stats
        .input_tokens
        .fetch_add(input_tokens, Ordering::Relaxed);
    model
        .stats
        .cached_tokens
        .fetch_add(cached, Ordering::Relaxed);
    model
        .stats
        .tool_calls
        .fetch_add(u64::try_from(calls.len()).unwrap_or(0), Ordering::Relaxed);
    let (thinking, per_token) = match stream_ms {
        Some(_) => (
            Duration::ZERO,
            Duration::from_secs_f64(1e-6 / model.speed.max(0.001)),
        ),
        None => (
            timing.thinking_ms.millis(&rng, model.speed),
            Duration::from_secs_f64(1.0 / timing.tokens_per_second / model.speed.max(0.001)),
        ),
    };
    if let Some(tag) = tag {
        let tokens: usize = events.iter().map(|(tokens, _)| *tokens).sum();
        let streaming = per_token * u32::try_from(tokens).unwrap_or(u32::MAX);
        model
            .intervals
            .record(tag, "model", arrived, wait + thinking + streaming);
    }
    let paced = stream::unfold(
        (events.into_iter(), true),
        move |(mut rest, first)| async move {
            let (tokens, event) = rest.next()?;
            let pause = if first {
                thinking
            } else {
                per_token * u32::try_from(tokens).unwrap_or(u32::MAX)
            };
            tokio::time::sleep(pause).await;
            Some((Ok::<Event, Infallible>(event), (rest, false)))
        },
    );
    Sse::new(paced).into_response()
}
