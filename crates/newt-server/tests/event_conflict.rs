#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;

use newt_server::http::{AppState, router};
use newt_server::live::Topics;
use newt_server::store::{Accepted, Store};
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
        live_stream: Arc::default(),
        metrics: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{address}")
}

async fn post(base: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/s/events"))
        .bearer_auth("secret")
        .json(body)
        .send()
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_repeat_id_with_the_same_content_is_a_duplicate(pool: PgPool) {
    let base = start(pool).await;
    let first: Accepted = post(
        &base,
        &json!({ "id": "e1", "text": "hi", "meta": { "a": 1, "b": 2 } }),
    )
    .await
    .json()
    .await
    .unwrap();

    let again = post(
        &base,
        &json!({ "id": "e1", "text": "hi", "meta": { "b": 2, "a": 1 }, "priority": "background" }),
    )
    .await;

    assert_eq!(again.status(), 202);
    let again: Accepted = again.json().await.unwrap();
    assert!(again.duplicate);
    assert_eq!(again.inbox_id, first.inbox_id);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_repeat_id_with_other_content_is_a_conflict(pool: PgPool) {
    let base = start(pool.clone()).await;
    assert_eq!(
        post(&base, &json!({ "id": "e1", "text": "hi" }))
            .await
            .status(),
        202
    );

    let changed_text = post(&base, &json!({ "id": "e1", "text": "something else" })).await;
    assert_eq!(changed_text.status(), 409);
    let body: Value = changed_text.json().await.unwrap();
    assert!(
        body["error"].to_string().contains("different content"),
        "{body}"
    );
    let changed_meta = post(
        &base,
        &json!({ "id": "e1", "text": "hi", "meta": { "k": "v" } }),
    )
    .await;
    assert_eq!(changed_meta.status(), 409);

    let stored: i64 = sqlx::query_scalar("select count(*) from inbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_event_stored_before_hashes_existed_stays_a_duplicate(pool: PgPool) {
    let base = start(pool.clone()).await;
    sqlx::query("insert into sessions (agent, key) values ('assistant', 's')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into inbox (agent, key, event_id, kind, policy, priority, payload) \
         values ('assistant', 's', 'old', 'message', 'steer', 0, '{\"text\":\"hi\",\"meta\":null}')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let again = post(&base, &json!({ "id": "old", "text": "different now" })).await;

    assert_eq!(again.status(), 202);
    let again: Accepted = again.json().await.unwrap();
    assert!(again.duplicate);
}
