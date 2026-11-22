#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use newt_server::http::{AppState, router};
use newt_server::live::Topics;
use newt_server::store::Store;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

fn state(pool: PgPool, cap: usize) -> AppState {
    AppState {
        store: Store::new(pool).with_max_pending(HashMap::from([("assistant".to_owned(), cap)])),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live: Topics::default(),
        shutdown: CancellationToken::new(),
        instrumentation: None,
        live_meta: Arc::default(),
        live_stream: Arc::default(),
        metrics: Arc::default(),
    }
}

async fn serve(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{address}")
}

async fn assert_error(
    response: reqwest::Response,
    status: u16,
    code: &str,
    retry_after: bool,
) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(
        response.headers().contains_key("retry-after"),
        retry_after,
        "retry-after on {status}"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "{body}"
    );
    assert_eq!(body["error"].as_object().unwrap().len(), 2, "{body}");
    body
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_error_has_one_json_shape_and_retry_after_where_a_retry_helps(pool: PgPool) {
    let base = serve(state(pool, 1)).await;
    let client = reqwest::Client::new();
    let events = format!("{base}/v1/agents/assistant/sessions/s/events");
    let post = |body: Value| {
        let (client, events) = (client.clone(), events.clone());
        async move {
            client
                .post(events)
                .bearer_auth("secret")
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    assert_error(
        client.post(&events).json(&json!({})).send().await.unwrap(),
        401,
        "unauthorized",
        false,
    )
    .await;
    assert_error(
        post(json!({ "id": "e1", "text": " " })).await,
        400,
        "invalid_request",
        false,
    )
    .await;
    assert_error(
        client
            .get(format!("{base}/v1/agents/assistant/sessions/missing"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap(),
        404,
        "not_found",
        false,
    )
    .await;
    assert_error(
        client
            .get(&events)
            .bearer_auth("secret")
            .send()
            .await
            .unwrap(),
        405,
        "method_not_allowed",
        false,
    )
    .await;
    assert_error(
        client
            .get(format!("{base}/nothing/here"))
            .send()
            .await
            .unwrap(),
        404,
        "not_found",
        false,
    )
    .await;
    assert_error(
        client
            .post(&events)
            .bearer_auth("secret")
            .body("x".repeat(2 * 1024 * 1024 + 1))
            .send()
            .await
            .unwrap(),
        413,
        "payload_too_large",
        false,
    )
    .await;

    assert_eq!(
        post(json!({ "id": "e1", "text": "hi" })).await.status(),
        202
    );
    assert_error(
        post(json!({ "id": "e1", "text": "changed" })).await,
        409,
        "event_id_conflict",
        false,
    )
    .await;
    assert_error(
        post(json!({ "id": "e2", "text": "hi" })).await,
        429,
        "inbox_full",
        true,
    )
    .await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn healthz_is_503_with_retry_after_when_postgres_is_down(pool: PgPool) {
    let unreachable = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://newt:newt@127.0.0.1:1/newt")
        .unwrap();
    let base = serve(state(unreachable, 1)).await;
    drop(pool);

    let response = reqwest::get(format!("{base}/healthz")).await.unwrap();

    assert_eq!(response.status(), 503);
    assert!(response.headers().contains_key("retry-after"));
}
