use std::collections::HashMap;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use super::FakeModel;
use crate::cache::{CacheScope, Lookup};
use crate::plan::seed_of;

const DUMP_LIMIT: u64 = 400;

pub(super) async fn record_shape(model: &FakeModel, body: &Value) {
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

pub(super) fn cache_key(body: &Value) -> String {
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

pub(super) fn tool_schemas(body: &Value) -> HashMap<String, Value> {
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
pub(super) fn look_up(model: &FakeModel, body: &Value, prompt: &[u8]) -> Lookup {
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
