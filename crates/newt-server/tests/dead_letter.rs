#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::time::Duration;

use newt_server::sessions::Lease;
use newt_server::store::{InboundEvent, Store};
use serde_json::json;
use sqlx::PgPool;

const TTL: Duration = Duration::from_secs(60);

fn event(id: &str) -> InboundEvent {
    serde_json::from_value(json!({ "id": id, "text": "hi" })).unwrap()
}

async fn claimed(store: &Store, owner: &str) -> Lease {
    let (lease, _waited) = store.claim("assistant", "s", owner, TTL).await.unwrap();
    lease.unwrap()
}

async fn consumed(pool: &PgPool, inbox_id: i64) -> bool {
    sqlx::query_scalar("select consumed_at is not null from inbox where id = $1")
        .bind(inbox_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn journal_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from turn_journal where key = 's'")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_dead_letter_after_the_lease_moved_leaves_the_new_owners_turn_alone(pool: PgPool) {
    let store = Store::new(pool.clone());
    let accepted = store
        .accept_event("assistant", "s", &event("e1"), false)
        .await
        .unwrap();
    let stale = claimed(&store, "first").await;
    store.release(&stale).await.unwrap();
    let current = claimed(&store, "second").await;
    sqlx::query(
        "insert into turn_journal (agent, key, turn_id, seq, kind, payload) \
         values ('assistant', 's', 't2', 0, 'start', '{}'::jsonb)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let _dead_lettered = store
        .dead_letter(&stale, vec![accepted.inbox_id], "cancelled".into(), None)
        .await
        .unwrap();

    assert!(
        !consumed(&pool, accepted.inbox_id).await,
        "the event is the new owner's to answer"
    );
    assert_eq!(
        journal_rows(&pool).await,
        1,
        "the new owner's journal is kept"
    );
    let owner: Option<String> =
        sqlx::query_scalar("select lease_owner from sessions where key = 's'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(owner.as_deref(), Some(current.owner.as_str()));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_dead_letter_consumes_the_events_and_releases_the_session_in_one_step(pool: PgPool) {
    let store = Store::new(pool.clone());
    let accepted = store
        .accept_event("assistant", "s", &event("e1"), false)
        .await
        .unwrap();
    let lease = claimed(&store, "first").await;

    let _dead_lettered = store
        .dead_letter(&lease, vec![accepted.inbox_id], "cancelled".into(), None)
        .await
        .unwrap();

    assert!(consumed(&pool, accepted.inbox_id).await);
    let owner: Option<String> =
        sqlx::query_scalar("select lease_owner from sessions where key = 's'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(owner, None, "nothing else holds the session up");
}
