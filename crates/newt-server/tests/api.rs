#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use newt_core::sse::SseParser;
use newt_server::http::{AppState, router};
use newt_server::live::spawn_listener;
use newt_server::store::{Accepted, SessionStatus, Store};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn start(pool: PgPool) -> String {
    let (live, _listener) = spawn_listener(pool.clone()).await.unwrap();
    let state = AppState {
        store: Store::new(pool),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live,
        shutdown: tokio_util::sync::CancellationToken::new(),
        observers: newt_server::events::Observers::default(),
        live_meta: Arc::default(),
        live_stream: Arc::default(),
        metrics: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{address}")
}

fn event(id: &str, text: &str) -> Value {
    json!({ "id": id, "text": text })
}

async fn post(base: &str, key: &str, token: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/{key}/events"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_event_is_accepted_once_and_gives_one_read_receipt(pool: PgPool) {
    let base = start(pool).await;

    let first = post(&base, "customer-1", "secret", &event("e1", "hi")).await;
    assert_eq!(first.status(), 202);
    let first: Accepted = first.json().await.unwrap();
    let again: Accepted = post(&base, "customer-1", "secret", &event("e1", "hi"))
        .await
        .json()
        .await
        .unwrap();

    assert!(!first.duplicate);
    assert!(again.duplicate);
    assert_eq!(again.inbox_id, first.inbox_id);
    let status: SessionStatus = reqwest::Client::new()
        .get(format!("{base}/v1/agents/assistant/sessions/customer-1"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status.pending_events, 1);
    assert_eq!(status.live_seq, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn bad_tokens_unknown_agents_and_empty_messages_are_refused(pool: PgPool) {
    let base = start(pool).await;

    assert_eq!(
        post(&base, "k", "wrong", &event("e1", "hi")).await.status(),
        401
    );
    let unknown = reqwest::Client::new()
        .post(format!("{base}/v1/agents/nobody/sessions/k/events"))
        .bearer_auth("secret")
        .json(&event("e1", "hi"))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 401);
    assert_eq!(
        post(&base, "k", "secret", &event("e1", "  "))
            .await
            .status(),
        400
    );
    assert_eq!(
        post(&base, "k", "secret", &event(" ", "hi")).await.status(),
        400
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_live_feed_replays_then_streams_new_events(pool: PgPool) {
    let base = start(pool).await;
    post(&base, "customer-2", "secret", &event("e1", "first")).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{base}/v1/agents/assistant/sessions/customer-2/live?cursor=0"
        ))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut chunks = response.bytes_stream();
    let mut parser = SseParser::default();
    let mut seen = Vec::new();
    let mut posted_second = false;

    let deadline = Instant::now() + Duration::from_secs(10);
    while seen.len() < 2 && Instant::now() < deadline {
        let chunk = tokio::time::timeout(Duration::from_secs(10), chunks.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        for sse in parser.feed(&chunk).unwrap() {
            let data: Value = serde_json::from_str(&sse.data).unwrap();
            seen.push(data["event_id"].as_str().unwrap().to_owned());
        }
        if !posted_second && seen.len() == 1 {
            posted_second = true;
            post(&base, "customer-2", "secret", &event("e2", "second")).await;
        }
    }
    assert_eq!(seen, vec!["e1", "e2"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_read_receipt_is_fast(pool: PgPool) {
    let base = start(pool).await;
    post(&base, "warmup", "secret", &event("w", "warm up")).await;

    let mut timings = Vec::new();
    for index in 0..20 {
        let started = Instant::now();
        let response = post(
            &base,
            "customer-3",
            "secret",
            &event(&format!("e{index}"), "hi"),
        )
        .await;
        timings.push(started.elapsed());
        assert_eq!(response.status(), 202);
    }
    timings.sort();
    let median = timings[timings.len() / 2];
    assert!(
        median < Duration::from_millis(50),
        "median read receipt took {median:?}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn session_keys_with_slashes_or_huge_lengths_are_refused(pool: PgPool) {
    let base = start(pool).await;
    let long = "k".repeat(201);
    assert_eq!(
        post(&base, &long, "secret", &event("e1", "hi"))
            .await
            .status(),
        400
    );
    let slashed = reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/a%2Fb/events"))
        .bearer_auth("secret")
        .json(&event("e1", "hi"))
        .send()
        .await
        .unwrap();
    assert_eq!(slashed.status(), 400);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_event_body_over_two_megabytes_is_refused(pool: PgPool) {
    let base = start(pool).await;
    let huge = "x".repeat(2 * 1024 * 1024 + 1);
    assert_eq!(
        post(&base, "k1", "secret", &event("e1", &huge))
            .await
            .status(),
        413
    );
}
