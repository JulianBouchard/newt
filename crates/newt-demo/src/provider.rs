use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::Demo;

const VERSION: &str = "demo-1";

pub(crate) const INSTRUCTIONS: &str = "You are a personal assistant for one person, reached by chat.\n\
The person only sees what you send with send_message: reply with it, briefly and specifically.\n\
Your workspace holds their notes, their tasks and the messages you already sent. Read it before you answer, and keep it current: add_note for facts worth keeping about them, add_task and complete_task for things to do.\n\
Use remember only for preferences and standing instructions that should shape how you work with them.\n\
End every turn with end_turn.";

#[derive(Deserialize)]
struct TurnRequest {
    session: String,
}

#[derive(Deserialize)]
struct PagesRequest {
    session: String,
    keys: Vec<Vec<String>>,
}

pub(crate) fn routes() -> Router<Arc<Demo>> {
    Router::new()
        .route("/assistant/bootstrap", get(bootstrap))
        .route("/assistant/turn", post(turn))
        .route("/assistant/pages", post(pages))
        .route("/assistant/outbox/{session}", get(outbox))
}

async fn bootstrap(State(demo): State<Arc<Demo>>) -> Json<Value> {
    Json(json!({
        "version": VERSION,
        "instructions": INSTRUCTIONS,
        "model": "default",
        "first_call_effort": "low",
        "effort": "medium",
        "mcp_servers": [{
            "name": "demo",
            "url": format!("{}/mcp", demo.public_url),
            "auth": "agent_token",
        }],
    }))
}

async fn turn(State(demo): State<Arc<Demo>>, Json(request): Json<TurnRequest>) -> Json<Value> {
    let pages = demo.with(&request.session, |session| session.pages());
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    Json(json!({
        "version": VERSION,
        "pages": pages,
        "instructions": format!("It is {now}."),
    }))
}

async fn pages(State(demo): State<Arc<Demo>>, Json(request): Json<PagesRequest>) -> Json<Value> {
    let pages: Vec<_> = demo.with(&request.session, |session| {
        if request.keys.iter().any(Vec::is_empty) {
            return session.pages();
        }
        request
            .keys
            .iter()
            .filter_map(|key| key.first())
            .filter_map(|name| session.page(name))
            .collect()
    });
    Json(json!({ "pages": pages }))
}

async fn outbox(State(demo): State<Arc<Demo>>, Path(session): Path<String>) -> Json<Value> {
    Json(json!({ "sent": demo.sent(&session) }))
}
