//! What makes the demo an agent: its instructions, the settings it declares
//! in the `newt` extension, and its answer to `newt/turn`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use rmcp::model::JsonObject;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::Demo;

pub(crate) const INSTRUCTIONS: &str = "You are a personal assistant for one person, reached by chat.\n\
The person only sees what you send with send_message: reply with it, briefly and specifically.\n\
Your workspace holds their notes, their tasks and the messages you already sent. Read it before you answer, and keep it current: add_note for facts worth keeping about them, add_task and complete_task for things to do.\n\
Use remember only for preferences and standing instructions that should shape how you work with them.\n\
End every turn with end_turn.";

/// The agent's settings, declared as the `newt` extension.
pub(crate) fn settings() -> JsonObject {
    let Value::Object(settings) = json!({
        "model": "default",
        "first_call_effort": "low",
        "effort": "medium",
        "turn": true,
    }) else {
        unreachable!("a JSON object literal")
    };
    settings
}

/// The demo's answer to `newt/turn`: the time, for this turn only.
pub(crate) fn turn() -> Value {
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    json!({ "instructions": format!("It is {now}.") })
}

/// What the assistant sent each person, for the quickstart to show.
pub(crate) fn routes() -> Router<Arc<Demo>> {
    Router::new().route("/outbox/{session}", get(outbox))
}

async fn outbox(State(demo): State<Arc<Demo>>, Path(session): Path<String>) -> Json<Value> {
    Json(json!({ "sent": demo.sent(&session) }))
}
