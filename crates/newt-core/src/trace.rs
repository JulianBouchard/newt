//! The trace and span ids of a turn, derived from the session, the turn and
//! when its attempt started, so the loop can hand a tool call its
//! `traceparent` before any exporter has seen the turn, and an exporter can
//! later emit spans with the same ids.

use time::OffsetDateTime;

use crate::prompt::fnv1a;

fn hex64(parts: &[&str]) -> String {
    format!("{:016x}", fnv1a(parts.join("\u{0}").as_bytes()))
}

/// The attempt a turn's trace belongs to: when it started.
pub fn attempt(started: OffsetDateTime) -> String {
    started.unix_timestamp_nanos().to_string()
}

/// The 32-hex-digit trace id of one attempt of a turn.
pub fn trace_id(session: &str, turn: &str, attempt: &str) -> String {
    format!(
        "{}{}",
        hex64(&["trace", session, turn, attempt]),
        hex64(&["trace-low", session, turn, attempt])
    )
}

/// The span id of the turn's root span in one attempt of a turn.
pub fn root_span_id(session: &str, turn: &str, attempt: &str) -> String {
    hex64(&["root", session, turn, attempt])
}

/// The span id of the model call at `index` in one attempt of a turn.
/// `conversation` is the session, or its [`session_hash`] when content is
/// not captured.
pub fn model_call_span_id(conversation: &str, turn: &str, attempt: &str, index: u32) -> String {
    hex64(&["call", conversation, turn, attempt, &index.to_string()])
}

/// The session as a hash, for traces that must not carry its key.
pub fn session_hash(session: &str) -> String {
    hex64(&["session", session])
}

/// The span id of a tool call in one attempt of a turn.
pub fn tool_span_id(session: &str, turn: &str, attempt: &str, call_id: &str) -> String {
    hex64(&["tool", session, turn, attempt, call_id])
}

/// The W3C `traceparent` of a tool call's span, as the exporter will later
/// emit it, so a tool server can parent its own spans under newt's.
pub fn tool_traceparent(
    session: &str,
    turn: &str,
    started: OffsetDateTime,
    call_id: &str,
) -> String {
    let attempt = attempt(started);
    format!(
        "00-{}-{}-01",
        trace_id(session, turn, &attempt),
        tool_span_id(session, turn, &attempt, call_id)
    )
}
