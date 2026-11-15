#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests fail loudly on setup errors"
)]

use std::time::Duration;

use newt_core::message::Message;
use newt_server::sessions::Lease;
use newt_server::store::{InboundEvent, Store};
use newt_server::turn_start::NextTurn;
use serde_json::json;
use sqlx::PgPool;

const TTL: Duration = Duration::from_secs(60);

fn event(id: &str, text: &str) -> InboundEvent {
    serde_json::from_value(json!({ "id": id, "text": text })).unwrap()
}

async fn claimed(store: &Store, key: &str, owner: &str) -> Lease {
    let (lease, _waited) = store.claim("assistant", key, owner, TTL).await.unwrap();
    lease.unwrap()
}

async fn steal(pool: &PgPool, key: &str) {
    sqlx::query(
        "update sessions set lease_owner = 'thief', lease_epoch = lease_epoch + 1 where key = $1",
    )
    .bind(key)
    .execute(pool)
    .await
    .unwrap();
}

async fn expire(pool: &PgPool, key: &str) {
    sqlx::query(
        "update sessions set lease_expires_at = now() - interval '1 second' where key = $1",
    )
    .bind(key)
    .execute(pool)
    .await
    .unwrap();
}

async fn journal_rows(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar("select count(*) from turn_journal where key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_stale_owner_cannot_begin_a_turn(pool: PgPool) {
    let store = Store::new(pool.clone());
    let accepted = store
        .accept_event("assistant", "s1", &event("e1", "hi"))
        .await
        .unwrap();
    let lease = claimed(&store, "s1", "me").await;
    steal(&pool, "s1").await;

    let begun = store
        .begin_turn(&lease, "t1", &[accepted.inbox_id], true)
        .await
        .unwrap();

    assert!(begun.is_none());
    assert_eq!(journal_rows(&pool, "s1").await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_stale_owner_of_an_idle_session_releases_nothing(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "s2", &event("e1", "hi"))
        .await
        .unwrap();
    let lease = claimed(&store, "s2", "me").await;
    sqlx::query("update inbox set consumed_at = now() where key = 's2'")
        .execute(&pool)
        .await
        .unwrap();
    steal(&pool, "s2").await;

    let next = store.next_turn_or_release(&lease).await.unwrap();

    assert!(
        matches!(next, NextTurn::Idle { released: false }),
        "{next:?}"
    );
    let status = store
        .session_status("assistant", "s2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.lease_owner.as_deref(), Some("thief"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_owner_with_nothing_to_do_releases_its_lease(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "s3", &event("e1", "hi"))
        .await
        .unwrap();
    let lease = claimed(&store, "s3", "me").await;
    sqlx::query("update inbox set consumed_at = now() where key = 's3'")
        .execute(&pool)
        .await
        .unwrap();

    let next = store.next_turn_or_release(&lease).await.unwrap();

    assert!(
        matches!(next, NextTurn::Idle { released: true }),
        "{next:?}"
    );
    let status = store
        .session_status("assistant", "s3")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.lease_owner, None);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_crashed_turn_resumes_with_exactly_the_events_it_started_on(pool: PgPool) {
    let store = Store::new(pool.clone());
    let first = store
        .accept_event("assistant", "s4", &event("e1", "hi"))
        .await
        .unwrap();
    let crashed = claimed(&store, "s4", "crashed").await;
    store
        .begin_turn(&crashed, "t1", &[first.inbox_id], true)
        .await
        .unwrap()
        .unwrap();
    store
        .accept_event("assistant", "s4", &event("e2", "later"))
        .await
        .unwrap();
    expire(&pool, "s4").await;
    let lease = claimed(&store, "s4", "me").await;

    let next = store.next_turn_or_release(&lease).await.unwrap();

    let NextTurn::Resume { open, events } = next else {
        panic!("expected a resumed turn, got {next:?}");
    };
    assert_eq!(open.turn_id, "t1");
    let ids: Vec<i64> = events.iter().map(|event| event.inbox_id).collect();
    assert_eq!(
        ids,
        vec![first.inbox_id],
        "the later event waits for its own turn"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn commits_number_history_contiguously_and_consume_their_events(pool: PgPool) {
    let store = Store::new(pool.clone());
    let accepted = store
        .accept_event("assistant", "s5", &event("e1", "hi"))
        .await
        .unwrap();
    let lease = claimed(&store, "s5", "me").await;
    let messages = vec![Message::user_text("hi"), Message::user_text("again")];
    store
        .begin_turn(&lease, "t1", &[accepted.inbox_id], true)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_turn(
            &lease,
            "t1".into(),
            messages.clone(),
            vec![accepted.inbox_id],
            TTL,
        )
        .await
        .unwrap();
    store
        .commit_turn(&lease, "t2".into(), messages, Vec::new(), TTL)
        .await
        .unwrap();
    store
        .commit_turn(&lease, "t3".into(), Vec::new(), Vec::new(), TTL)
        .await
        .unwrap();

    let seqs: Vec<i64> =
        sqlx::query_scalar("select seq from history where key = 's5' order by seq")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(seqs.len(), 4);
    assert!(
        seqs.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{seqs:?}"
    );
    let status = store
        .session_status("assistant", "s5")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.pending_events, 0);
    assert_eq!(
        journal_rows(&pool, "s5").await,
        0,
        "commit clears the turn's journal"
    );
    let begun = store
        .begin_turn(&lease, "t4", &[], true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        begun.0.len(),
        4,
        "history reads back every committed message"
    );
}
