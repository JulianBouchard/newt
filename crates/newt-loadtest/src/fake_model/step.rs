use std::collections::HashMap;

use serde_json::{Value, json};

use super::FakeModel;
use crate::plan::{Call, Turn, seed_of};
use crate::profile::ModelTiming;
use crate::sample::Rng;

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a sampled wait of a few hundred milliseconds as a small token count"
)]
pub(super) fn tail_tokens(timing: &ModelTiming, rng: &Rng) -> usize {
    let tail_ms = rng.lognormal(timing.completion_tail_ms.p50, timing.completion_tail_ms.p90);
    (tail_ms / 1000.0 * timing.tokens_per_second)
        .round()
        .max(0.0) as usize
}

pub(super) fn replay_step(
    model: &FakeModel,
    tag: Option<u64>,
    turn: &Turn,
    tools: &HashMap<String, Value>,
    rng: &Rng,
) -> Option<crate::replay::Planned> {
    let replay = model.replay.as_ref()?;
    if turn.replied || turn.compacting {
        return None;
    }
    let key = tag?;
    let mut progress = model.replay_progress.lock().ok()?;
    let index = progress.entry(key).or_insert(0);
    let planned = replay.plan(key, *index, tools, &model.profile, rng)?;
    *index += 1;
    Some(planned)
}

pub(super) fn call_rng(last_user: &str, done_calls: usize) -> Rng {
    let step = u64::try_from(done_calls).unwrap_or(u64::MAX);
    Rng::new(seed_of(last_user) ^ step.wrapping_add(1).wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

pub(super) fn audit_reply(
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

pub(super) fn calls_json(model: &FakeModel, calls: &[Call]) -> Vec<(String, String, String)> {
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
