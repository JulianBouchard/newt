#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use newt_server::http::{AppState, router};
use newt_server::live::Topics;
use newt_server::store::Store;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

async fn start(pool: PgPool) -> String {
    let state = AppState {
        store: Store::new(pool),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live: Topics::default(),
        shutdown: CancellationToken::new(),
        instrumentation: None,
        live_meta: Arc::default(),
        live_stream: Arc::new(HashSet::from(["assistant".to_owned()])),
        metrics: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{address}")
}

async fn status_and_body(base: &str, method: Method, path: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .request(
            method,
            format!("{base}/v1/agents/assistant/sessions/{path}"),
        )
        .bearer_auth("secret")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_nul_character_anywhere_in_a_request_is_a_400(pool: PgPool) {
    let base = start(pool).await;
    let nul = "a\u{0}b";
    let seeded = status_and_body(
        &base,
        Method::POST,
        "s/events",
        json!({ "id": "seed", "text": "hi" }),
    )
    .await;
    assert_eq!(seeded.0, 202);
    let cases = [
        ("events: id", "s/events", json!({ "id": nul, "text": "hi" })),
        (
            "events: text",
            "s/events",
            json!({ "id": "e", "text": nul }),
        ),
        (
            "events: meta value",
            "s/events",
            json!({ "id": "e", "text": "hi", "meta": { "k": [nul] } }),
        ),
        (
            "events: meta key",
            "s/events",
            json!({ "id": "e", "text": "hi", "meta": { nul: 1 } }),
        ),
        (
            "events: session key",
            "a%00b/events",
            json!({ "id": "e", "text": "hi" }),
        ),
        (
            "chat: message id",
            "s/chat",
            json!({ "messages": [{ "id": nul, "role": "user", "parts": [{ "type": "text", "text": "hi" }] }] }),
        ),
        (
            "chat: text",
            "s/chat",
            json!({ "messages": [{ "id": "m", "role": "user", "parts": [{ "type": "text", "text": nul }] }] }),
        ),
        ("cancel: turn", "s/cancel", json!({ "turn": nul })),
        ("fork: into", "s/fork", json!({ "into": nul })),
        (
            "fork: through_turn",
            "s/fork",
            json!({ "into": "copy", "through_turn": nul }),
        ),
    ];
    let mut wrong = Vec::new();
    for (label, path, body) in cases {
        let (status, answer) = status_and_body(&base, Method::POST, path, body).await;
        if status != 400 || answer["error"]["code"] != "invalid_request" {
            wrong.push(format!("{label}: {status} {answer}"));
        }
    }
    for path in ["a%00b", "a%00b/history", "a%00b/live"] {
        let method = if path.ends_with("live") || path.ends_with("history") {
            Method::GET
        } else {
            Method::DELETE
        };
        let (status, answer) = status_and_body(&base, method, path, Value::Null).await;
        if status != 400 {
            wrong.push(format!("{path}: {status} {answer}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
