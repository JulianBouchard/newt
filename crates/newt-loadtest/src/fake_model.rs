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

use crate::audit::Audit;
use crate::cache::{CacheRules, CacheScope, Lookup, PrefixCache, Router, prompt_bytes};
use crate::plan::{Call, Turn, finish, pick_tools, read_turn, seed_of};
use crate::profile::{ModelTiming, Profile};
use crate::sample::Rng;

const CACHE_TTL: Duration = Duration::from_mins(5);

#[derive(Debug, Default)]
pub struct Stats {
    pub calls: AtomicU64,
    pub input_tokens: AtomicU64,
    pub cached_tokens: AtomicU64,
    pub output_tokens: AtomicU64,
    pub tool_calls: AtomicU64,
    pub failures: AtomicU64,
    pub instructions_bytes: AtomicU64,
    pub tools_bytes: AtomicU64,
    pub input_bytes: AtomicU64,
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
            "instructions_bytes": self.instructions_bytes.load(Ordering::Relaxed),
            "tools_bytes": self.tools_bytes.load(Ordering::Relaxed),
            "input_bytes": self.input_bytes.load(Ordering::Relaxed),
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
    scope: CacheScope,
    router: Router,
    audit: Audit,
    dump_dir: Option<std::path::PathBuf>,
    turn_marker: Option<String>,
    profile: Profile,
    pub stats: Stats,
}

#[derive(Debug, Clone, Default)]
pub struct FakeOptions {
    pub speed: f64,
    pub failure_rate: f64,
    pub seed: u64,
    pub cache_scope: CacheScope,
    pub dump_dir: Option<std::path::PathBuf>,
    pub turn_marker: Option<String>,
    pub decision_log: Option<std::path::PathBuf>,
    pub cache_log: Option<std::path::PathBuf>,
    pub profile: Profile,
}

const DUMP_LIMIT: u64 = 400;

impl FakeModel {
    pub fn new(timing: ModelTiming, options: FakeOptions) -> std::io::Result<Arc<Self>> {
        let FakeOptions {
            speed,
            failure_rate,
            seed: rng_seed,
            cache_scope,
            dump_dir,
            turn_marker,
            decision_log,
            cache_log,
            profile,
        } = options;
        let machines = match cache_scope {
            CacheScope::Routed { machines } => machines,
            CacheScope::Key | CacheScope::Prefix => 1,
        };
        Ok(Arc::new(Self {
            rng: Rng::new(rng_seed),
            timing,
            speed,
            failure_rate,
            cache: PrefixCache::new(CacheRules {
                ttl: CACHE_TTL.div_f64(speed.max(0.001)),
                readable_after: timing.cache_readable_after.div_f64(speed.max(0.001)),
            }),
            ids: AtomicU64::new(rng_seed),
            scope: cache_scope,
            router: Router::new(machines),
            audit: Audit::new(decision_log.as_deref(), cache_log.as_deref())?,
            dump_dir,
            turn_marker,
            profile,
            stats: Stats::default(),
        }))
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
    let mut snapshot = model.stats.snapshot();
    if let (Some(totals), Value::Object(audit)) = (snapshot.as_object_mut(), model.audit.snapshot())
    {
        totals.extend(audit);
    }
    Json(snapshot)
}

async fn record_shape(model: &FakeModel, body: &Value) {
    let size = |field: &str| {
        u64::try_from(body.get(field).map_or(0, |value| value.to_string().len())).unwrap_or(0)
    };
    model
        .stats
        .instructions_bytes
        .fetch_add(size("instructions"), Ordering::Relaxed);
    model
        .stats
        .tools_bytes
        .fetch_add(size("tools"), Ordering::Relaxed);
    model
        .stats
        .input_bytes
        .fetch_add(size("input"), Ordering::Relaxed);
    let number = model.stats.calls.load(Ordering::Relaxed);
    if let Some(dir) = &model.dump_dir
        && number <= DUMP_LIMIT
        && let Err(error) =
            tokio::fs::write(dir.join(format!("{number:03}.json")), body.to_string()).await
    {
        tracing::warn!(%error, "could not dump a request");
    }
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

/// Routes the request to a cache, looks its prompt up, and logs where it
/// went and what the machine held.
fn look_up(model: &FakeModel, body: &Value, prompt: &[u8]) -> Lookup {
    let mut route = None;
    let key = match model.scope {
        CacheScope::Key => cache_key(body),
        CacheScope::Prefix => "shared".to_owned(),
        CacheScope::Routed { .. } => {
            let requested = body.get("prompt_cache_key").and_then(Value::as_str);
            route = Some(model.router.route(requested, prompt, model.rng.next_u64()));
            format!("machine-{}", route.map_or(0, |route| route.machine))
        }
    };
    let lookup = model.cache.lookup_and_store(&key, prompt);
    model.audit.log_cache(&json!({
        "cache_key": body.get("prompt_cache_key"),
        "machine": route.map(|route| route.machine),
        "home": route.map(|route| route.home),
        "per_minute": route.map(|route| route.per_minute),
        "input_tokens": lookup.input_tokens,
        "cached_tokens": lookup.cached_tokens,
        "blocks": lookup.blocks,
        "held": lookup.held,
        "readable": lookup.readable,
    }));
    lookup
}

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
    model.audit.observe_outputs(&input);
    let turn = read_turn(&input, model.turn_marker.as_deref());
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
        pick_tools(&model.profile, &model.rng, &tools, step.max(1))
    };
    audit_reply(&model, &body, &tools, &turn, target, &calls);
    let (input_tokens, cached) = (lookup.input_tokens, lookup.cached_tokens);
    let text = calls
        .is_empty()
        .then(|| model.rng.words(timing.text_tokens.count(&model.rng)));
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

fn audit_reply(
    model: &FakeModel,
    body: &Value,
    tools: &HashMap<String, Value>,
    turn: &Turn,
    target: usize,
    calls: &[Call],
) {
    for call in calls {
        model.audit.check_call(
            &call.name,
            tools.get(&call.name).unwrap_or(&Value::Null),
            &call.arguments,
        );
    }
    model.audit.log_decision(&json!({
        "cache_key": body.get("prompt_cache_key"),
        "turn": turn.last_user,
        "done": turn.done_calls,
        "target": target,
        "returned": calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
    }));
}

fn calls_json(model: &FakeModel, calls: &[Call]) -> Vec<(String, String, String)> {
    calls
        .iter()
        .map(|call| {
            (
                call.name.clone(),
                call.arguments.to_string(),
                format!("call_{:016x}", model.next_id()),
            )
        })
        .collect()
}
