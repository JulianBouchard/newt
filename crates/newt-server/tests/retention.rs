#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use newt_server::http::{AppState, router};
use newt_server::live::spawn_listener;
use newt_server::retention::{Pruned, Retention};
use newt_server::store::{InboundEvent, Store};
use sqlx::PgPool;

fn event(id: &str) -> InboundEvent {
    serde_json::from_value(serde_json::json!({ "id": id, "text": "hi" })).unwrap()
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn pruning_deletes_only_old_consumed_events_and_old_live_rows(pool: PgPool) {
    let store = Store::new(pool.clone());
    for id in ["old", "recent", "pending"] {
        store
            .accept_event("assistant", "k", &event(id), false)
            .await
            .unwrap();
    }
    sqlx::query("update inbox set consumed_at = now() - interval '40 days' where event_id = 'old'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "update inbox set consumed_at = now() - interval '1 day' where event_id = 'recent'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "update live_events set created_at = now() - interval '10 days' \
         where seq = (select min(seq) from live_events)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let live_before = count(&pool, "select count(*) from live_events").await;

    let pruned = store.prune(Retention::default()).await.unwrap();

    assert_eq!(pruned, Pruned { inbox: 1, live: 1 });
    let left: Vec<String> = sqlx::query_scalar("select event_id from inbox order by id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, ["recent", "pending"]);
    assert_eq!(
        count(&pool, "select count(*) from live_events").await,
        live_before - 1
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn retention_set_to_forever_deletes_nothing(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "k", &event("old"), false)
        .await
        .unwrap();
    sqlx::query("update inbox set consumed_at = now() - interval '400 days'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("update live_events set created_at = now() - interval '400 days'")
        .execute(&pool)
        .await
        .unwrap();

    let pruned = store
        .prune(Retention {
            inbox: None,
            live: None,
        })
        .await
        .unwrap();

    assert_eq!(pruned, Pruned::default());
}

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

async fn delete(base: &str, key: &str) -> u16 {
    reqwest::Client::new()
        .delete(format!("{base}/v1/agents/assistant/sessions/{key}"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[sqlx::test(migrations = "../../migrations")]
async fn deleting_a_session_removes_everything_stored_for_it(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "gone", &event("e1"), false)
        .await
        .unwrap();
    store
        .accept_event("assistant", "kept", &event("e1"), false)
        .await
        .unwrap();
    let base = start(pool.clone()).await;

    assert_eq!(delete(&base, "gone").await, 204);

    assert_eq!(delete(&base, "gone").await, 404);
    assert_eq!(
        count(&pool, "select count(*) from inbox where key = 'gone'").await,
        0
    );
    assert_eq!(
        count(&pool, "select count(*) from live_events where key = 'gone'").await,
        0
    );
    assert_eq!(
        count(&pool, "select count(*) from sessions where key = 'kept'").await,
        1
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_session_running_a_turn_is_not_deleted(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "busy", &event("e1"), false)
        .await
        .unwrap();
    sqlx::query(
        "update sessions set lease_owner = 'runner-1', \
           lease_expires_at = now() + make_interval(secs => $1) where key = 'busy'",
    )
    .bind(Duration::from_secs(30).as_secs_f64())
    .execute(&pool)
    .await
    .unwrap();
    let base = start(pool.clone()).await;

    assert_eq!(delete(&base, "busy").await, 409);

    assert_eq!(
        count(&pool, "select count(*) from sessions where key = 'busy'").await,
        1
    );
}
