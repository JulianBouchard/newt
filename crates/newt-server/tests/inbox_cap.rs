#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;

use newt_server::http::{AppState, router};
use newt_server::live::Topics;
use newt_server::store::{Accepted, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

async fn start(pool: PgPool, cap: usize) -> String {
    let state = AppState {
        store: Store::new(pool).with_max_pending(HashMap::from([("assistant".to_owned(), cap)])),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live: Topics::default(),
        shutdown: CancellationToken::new(),
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

async fn post(base: &str, key: &str, id: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/{key}/events"))
        .bearer_auth("secret")
        .json(&json!({ "id": id, "text": "hi" }))
        .send()
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_event_past_the_cap_is_refused_with_retry_after(pool: PgPool) {
    let base = start(pool, 2).await;
    assert_eq!(post(&base, "s", "e1").await.status(), 202);
    assert_eq!(post(&base, "s", "e2").await.status(), 202);

    let refused = post(&base, "s", "e3").await;

    assert_eq!(refused.status(), 429);
    assert!(refused.headers().contains_key("retry-after"));
    let body: Value = refused.json().await.unwrap();
    assert!(
        body["error"].is_string() || body["error"].is_object(),
        "{body}"
    );
    let other = post(&base, "another-session", "e1").await;
    assert_eq!(other.status(), 202, "the cap is per session");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_repeat_of_a_held_event_is_a_duplicate_even_at_the_cap(pool: PgPool) {
    let base = start(pool, 1).await;
    let first: Accepted = post(&base, "s", "e1").await.json().await.unwrap();

    let again = post(&base, "s", "e1").await;

    assert_eq!(again.status(), 202);
    let again: Accepted = again.json().await.unwrap();
    assert!(again.duplicate);
    assert_eq!(again.inbox_id, first.inbox_id);
}

#[sqlx::test(migrations = "../../migrations")]
async fn consuming_events_frees_room(pool: PgPool) {
    let base = start(pool.clone(), 1).await;
    assert_eq!(post(&base, "s", "e1").await.status(), 202);
    assert_eq!(post(&base, "s", "e2").await.status(), 429);

    sqlx::query("update inbox set consumed_at = now()")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(post(&base, "s", "e2").await.status(), 202);
}

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_posts_never_exceed_the_cap(pool: PgPool) {
    let base = start(pool.clone(), 5).await;

    let posts = (0..30).map(|index| {
        let base = base.clone();
        tokio::spawn(async move {
            post(&base, "s", &format!("e{index}"))
                .await
                .status()
                .as_u16()
        })
    });
    let statuses = futures_util::future::join_all(posts).await;

    let accepted = statuses
        .iter()
        .filter(|status| *status.as_ref().unwrap() == 202)
        .count();
    assert_eq!(accepted, 5);
    let held: i64 = sqlx::query_scalar("select count(*) from inbox where consumed_at is null")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(held, 5);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_accept_that_waited_for_a_writer_counts_what_the_writer_added(pool: PgPool) {
    let base = start(pool.clone(), 3).await;
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("select newt_locked_pending_events('assistant', 's')")
        .execute(&mut *writer)
        .await
        .unwrap();
    let waiting = {
        let base = base.clone();
        tokio::spawn(async move { post(&base, "s", "late").await.status().as_u16() })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !waiting.is_finished(),
        "the accept did not wait for the lock"
    );

    sqlx::query("insert into sessions (agent, key) values ('assistant', 's')")
        .execute(&mut *writer)
        .await
        .unwrap();
    for index in 0..3 {
        sqlx::query(
            "insert into inbox (agent, key, event_id, kind, policy, priority, payload) \
             values ('assistant', 's', $1, 'message', 'steer', 0, '{}')",
        )
        .bind(format!("w{index}"))
        .execute(&mut *writer)
        .await
        .unwrap();
    }
    writer.commit().await.unwrap();

    assert_eq!(waiting.await.unwrap(), 429);
}
