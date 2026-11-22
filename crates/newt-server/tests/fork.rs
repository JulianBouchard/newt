#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;

use newt_server::http::{AppState, router};
use newt_server::live::spawn_listener;
use newt_server::store::Store;
use serde_json::{Value, json};
use sqlx::PgPool;

async fn start(pool: PgPool) -> String {
    let (live, _listener) = spawn_listener(pool.clone()).await.unwrap();
    let state = AppState {
        store: Store::new(pool),
        tokens: Arc::new(HashMap::from([
            ("assistant".to_owned(), "secret".to_owned()),
            ("canary".to_owned(), "secret".to_owned()),
            ("other".to_owned(), "other-secret".to_owned()),
        ])),
        live,
        shutdown: tokio_util::sync::CancellationToken::new(),
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

/// Ana's session: two turns of two messages each, and a checkpoint after the
/// first turn.
async fn seed(pool: &PgPool) {
    sqlx::query(
        "insert into sessions (agent, key, next_history_seq) values ('assistant', 'ana', 4)",
    )
    .execute(pool)
    .await
    .unwrap();
    for (seq, turn, text) in [
        (0, "t1", "add milk"),
        (1, "t1", "Added milk."),
        (2, "t2", "and eggs"),
        (3, "t2", "Added eggs."),
    ] {
        sqlx::query(
            "insert into history (agent, key, seq, turn_id, kind, entry, tokens) \
             values ('assistant', 'ana', $1, $2, 'turn', $3, 3)",
        )
        .bind(seq)
        .bind(turn)
        .bind(json!({ "text": text }))
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "insert into checkpoints (agent, key, through_seq, snapshot, summary) \
         values ('assistant', 'ana', 1, '{}', 'milk')",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn fork(base: &str, token: &str, body: &Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/ana/fork"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

async fn texts(base: &str, agent: &str, key: &str) -> Vec<String> {
    let body: Value = reqwest::Client::new()
        .get(format!("{base}/v1/agents/{agent}/sessions/{key}/history"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["entry"]["text"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_fork_copies_history_through_a_turn_into_a_new_session(pool: PgPool) {
    seed(&pool).await;
    let base = start(pool.clone()).await;

    let (status, body) = fork(
        &base,
        "secret",
        &json!({ "into": "ana-try", "through_turn": "t1" }),
    )
    .await;
    assert_eq!((status, body["history_entries"].as_i64()), (201, Some(2)));
    assert_eq!(
        texts(&base, "assistant", "ana-try").await,
        ["add milk", "Added milk."]
    );
    let (next,): (i64,) = sqlx::query_as(
        "select next_history_seq from sessions where agent = 'assistant' and key = 'ana-try'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(next, 2);
    let (checkpoints,): (i64,) = sqlx::query_as(
        "select count(*) from checkpoints where agent = 'assistant' and key = 'ana-try'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(checkpoints, 1);

    let (status, _) = fork(
        &base,
        "secret",
        &json!({ "into": "ana-all", "agent": "canary" }),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(texts(&base, "canary", "ana-all").await.len(), 4);
    assert_eq!(texts(&base, "assistant", "ana").await.len(), 4);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_fork_refuses_an_existing_target_an_unknown_turn_and_a_foreign_agent(pool: PgPool) {
    seed(&pool).await;
    let base = start(pool).await;

    assert_eq!(
        fork(&base, "secret", &json!({ "into": "ana" })).await.0,
        409
    );
    assert_eq!(
        fork(
            &base,
            "secret",
            &json!({ "into": "x", "through_turn": "t9" })
        )
        .await
        .0,
        400
    );
    assert_eq!(
        fork(&base, "secret", &json!({ "into": "x", "agent": "other" }))
            .await
            .0,
        401
    );
    let missing = reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/nobody/fork"))
        .bearer_auth("secret")
        .json(&json!({ "into": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}
