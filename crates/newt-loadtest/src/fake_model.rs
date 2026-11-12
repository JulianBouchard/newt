use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use serde_json::{Value, json};

use crate::args::arguments_for;
use crate::cache::{CacheRules, PrefixCache, prompt_bytes};
use crate::profile::{Kind, ModelTiming, ASSISTANT_TOOLS};
use crate::sample::Rng;

const CACHE_TTL: Duration = Duration::from_mins(5);
const COMPACTING: &str = "Newt is compacting";

#[derive(Debug, Default)]
pub struct Stats {
    pub calls: AtomicU64,
    pub input_tokens: AtomicU64,
    pub cached_tokens: AtomicU64,
    pub output_tokens: AtomicU64,
    pub tool_calls: AtomicU64,
    pub failures: AtomicU64,
}

impl Stats {
    pub fn snapshot(&self) -> Value {
        json!({
            "calls": self.calls.load(Ordering::Relaxed),
            "input_tokens": self.input_tokens.load(Ordering::Relaxed),
            "cached_tokens": self.cached_tokens.load(Ordering::Relaxed),
            "output_tokens": self.output_tokens.load(Ordering::Relaxed),
            "tool_calls": self.tool_calls.load(Ordering::Relaxed),
            "failures": self.failures.load(Ordering::Relaxed),
        })
    }
}

#[derive(Debug)]
pub struct FakeModel {
    rng: Rng,
    timing: ModelTiming,
    speed: f64,
    failure_rate: f64,
    cache: PrefixCache,
    ids: AtomicU64,
    pub stats: Stats,
}

impl FakeModel {
    pub fn new(timing: ModelTiming, speed: f64, failure_rate: f64, rng_seed: u64) -> Arc<Self> {
        Arc::new(Self {
            rng: Rng::new(rng_seed),
            timing,
            speed,
            failure_rate,
            cache: PrefixCache::new(CacheRules {
                ttl: CACHE_TTL.div_f64(speed.max(0.001)),
                readable_after: timing.cache_readable_after.div_f64(speed.max(0.001)),
            }),
            ids: AtomicU64::new(rng_seed),
            stats: Stats::default(),
        })
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed)
    }
}

pub fn router(model: Arc<FakeModel>) -> axum::Router {
    axum::Router::new()
        .route("/stats", get(stats))
        .fallback(respond)
        .with_state(model)
}

async fn stats(State(model): State<Arc<FakeModel>>) -> Json<Value> {
    Json(model.stats.snapshot())
}

struct Turn {
    done_calls: usize,
    last_user: String,
    compacting: bool,
}

fn read_turn(input: &[Value]) -> Turn {
    let last_user = input
        .iter()
        .rposition(|item| item.get("role").and_then(Value::as_str) == Some("user"));
    let start = last_user.map_or(0, |index| index + 1);
    let user_text = |item: &Value| -> String {
        match item.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect(),
            _ => String::new(),
        }
    };
    let compacting = input
        .iter()
        .filter(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .any(|item| user_text(item).starts_with(COMPACTING));
    Turn {
        done_calls: input
            .get(start..)
            .unwrap_or_default()
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
            .count(),
        last_user: last_user
            .and_then(|index| input.get(index))
            .map(user_text)
            .unwrap_or_default(),
        compacting,
    }
}

fn seed_of(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

struct Call {
    name: String,
    arguments: Value,
}

fn pick_tools(model: &FakeModel, tools: &HashMap<String, Value>, count: usize) -> Vec<Call> {
    let available: Vec<_> = ASSISTANT_TOOLS
        .iter()
        .filter(|tool| !matches!(tool.kind, Kind::End))
        .filter(|tool| {
            tools.contains_key(tool.name)
                || (tool.kind == Kind::Skill && tools.contains_key("read_file"))
        })
        .collect();
    let total: f64 = available.iter().map(|tool| tool.share).sum();
    let mut calls = Vec::with_capacity(count);
    let mut sent = false;
    while calls.len() < count && total > 0.0 {
        let mut roll = model.rng.unit() * total;
        let Some(tool) = available.iter().find(|tool| {
            roll -= tool.share;
            roll <= 0.0
        }) else {
            break;
        };
        if tool.kind == Kind::Send && std::mem::replace(&mut sent, true) {
            continue;
        }
        let name = if tool.kind == Kind::Skill && !tools.contains_key(tool.name) {
            "read_file"
        } else {
            tool.name
        };
        let schema = tools.get(name).cloned().unwrap_or(Value::Null);
        calls.push(Call {
            name: name.to_owned(),
            arguments: arguments_for(name, tool.kind, &schema, &model.rng),
        });
    }
    calls
}

fn finish(tools: &HashMap<String, Value>, compacting: bool) -> Vec<Call> {
    if compacting && tools.contains_key("checkpoint") {
        return vec![Call {
            name: "checkpoint".into(),
            arguments: json!({ "memory": ["load test"], "open_threads": [], "commitments": [], "decisions": [], "tone": "short" }),
        }];
    }
    if tools.contains_key("end_turn") {
        return vec![Call {
            name: "end_turn".into(),
            arguments: json!({ "nothing_new": "load test turn" }),
        }];
    }
    Vec::new()
}

fn cache_key(body: &Value) -> String {
    body.get("prompt_cache_key")
        .and_then(Value::as_str)
        .map_or_else(
            || {
                seed_of(
                    &body
                        .get("instructions")
                        .map(Value::to_string)
                        .unwrap_or_default(),
                )
                .to_string()
            },
            str::to_owned,
        )
}

fn tool_schemas(body: &Value) -> HashMap<String, Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            Some((
                name.to_owned(),
                tool.get("parameters").cloned().unwrap_or(Value::Null),
            ))
        })
        .collect()
}

async fn respond(State(model): State<Arc<FakeModel>>, Json(body): Json<Value>) -> Response {
    model.stats.calls.fetch_add(1, Ordering::Relaxed);
    let timing = model.timing;
    let key = cache_key(&body);
    let lookup = model.cache.lookup_and_store(&key, &prompt_bytes(&body));
    let uncached = lookup.input_tokens - lookup.cached_tokens;
    #[expect(clippy::cast_precision_loss, reason = "token counts far below 2^52")]
    let prefill = Duration::from_secs_f64(
        uncached as f64 / 1000.0 * timing.prefill_ms_per_1k_uncached
            / 1000.0
            / model.speed.max(0.001),
    );
    tokio::time::sleep(timing.headers_ms.millis(&model.rng, model.speed) + prefill).await;
    if model.rng.chance(model.failure_rate) {
        model.stats.failures.fetch_add(1, Ordering::Relaxed);
        return (StatusCode::SERVICE_UNAVAILABLE, "fake overload").into_response();
    }
    let input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tools = tool_schemas(&body);
    let turn = read_turn(&input);
    let target = timing
        .tool_calls_per_turn
        .count(&Rng::new(seed_of(&turn.last_user)));
    let calls = if turn.compacting || turn.done_calls >= target {
        finish(&tools, turn.compacting)
    } else {
        let step = timing
            .tool_calls_per_step
            .count(&model.rng)
            .min(target - turn.done_calls);
        pick_tools(&model, &tools, step.max(1))
    };
    let (input_tokens, cached) = (lookup.input_tokens, lookup.cached_tokens);
    let text = calls
        .is_empty()
        .then(|| model.rng.words(timing.text_tokens.count(&model.rng)));
    let calls = calls_json(&calls);
    let events = crate::stream::response_events(
        &model,
        crate::stream::Reply {
            model_name: body.get("model").and_then(Value::as_str).unwrap_or("fake"),
            calls: &calls,
            text,
            input_tokens,
            cached,
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
    let thinking = timing.thinking_ms.millis(&model.rng, model.speed);
    let per_token =
        Duration::from_secs_f64(1.0 / timing.tokens_per_second / model.speed.max(0.001));
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

fn calls_json(calls: &[Call]) -> Vec<(String, String)> {
    calls
        .iter()
        .map(|call| (call.name.clone(), call.arguments.to_string()))
        .collect()
}
