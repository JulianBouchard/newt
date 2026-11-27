#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests fail loudly on setup errors"
)]

use std::time::Duration;

use newt_core::message::Message;
use newt_server::sessions::{Lease, TurnCommit};
use newt_server::store::{InboundEvent, Store};
use newt_server::turn_start::NextTurn;
use serde_json::{Value, json};
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
        .accept_event("assistant", "s1", &event("e1", "hi"), false)
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
        .accept_event("assistant", "s2", &event("e1", "hi"), false)
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
        .accept_event("assistant", "s3", &event("e1", "hi"), false)
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
        .accept_event("assistant", "s4", &event("e1", "hi"), false)
        .await
        .unwrap();
    let crashed = claimed(&store, "s4", "crashed").await;
    store
        .begin_turn(&crashed, "t1", &[first.inbox_id], true)
        .await
        .unwrap()
        .unwrap();
    store
        .accept_event("assistant", "s4", &event("e2", "later"), false)
        .await
        .unwrap();
    expire(&pool, "s4").await;
    let lease = claimed(&store, "s4", "me").await;

    let next = store.next_turn_or_release(&lease).await.unwrap();

    let NextTurn::Resume { open, events, .. } = next else {
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
        .accept_event("assistant", "s5", &event("e1", "hi"), false)
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
            TurnCommit {
                turn_id: "t1".into(),
                messages: messages.clone(),
                consumed: vec![accepted.inbox_id],
                ..TurnCommit::default()
            },
            TTL,
        )
        .await
        .unwrap();
    store
        .commit_turn(
            &lease,
            TurnCommit {
                turn_id: "t2".into(),
                messages,
                consumed: Vec::new(),
                ..TurnCommit::default()
            },
            TTL,
        )
        .await
        .unwrap();
    store
        .commit_turn(
            &lease,
            TurnCommit {
                turn_id: "t3".into(),
                messages: Vec::new(),
                consumed: Vec::new(),
                ..TurnCommit::default()
            },
            TTL,
        )
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

#[sqlx::test(migrations = "../../migrations")]
async fn a_commit_saves_the_provider_state_and_queues_its_turn_end(pool: PgPool) {
    let store = Store::new(pool.clone());
    let first = store
        .accept_event("assistant", "s6", &event("e1", "hi"), false)
        .await
        .unwrap();
    let lease = claimed(&store, "s6", "me").await;
    store
        .begin_turn(&lease, "t1", &[first.inbox_id], true)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_turn(
            &lease,
            TurnCommit {
                turn_id: "t1".into(),
                consumed: vec![first.inbox_id],
                provider_state: Some(json!({ "seen": 1 })),
                turn_end: Some(json!({ "turn": "t1" })),
                ..TurnCommit::default()
            },
            TTL,
        )
        .await
        .unwrap();
    store
        .commit_turn(
            &lease,
            TurnCommit {
                turn_id: "t2".into(),
                ..TurnCommit::default()
            },
            TTL,
        )
        .await
        .unwrap();
    store
        .accept_event("assistant", "s6", &event("e2", "again"), false)
        .await
        .unwrap();

    let next = store.next_turn_or_release(&lease).await.unwrap();

    let NextTurn::Fresh { state, .. } = next else {
        panic!("expected a fresh turn, got {next:?}");
    };
    assert_eq!(
        state,
        Some(json!({ "seen": 1 })),
        "a commit without state keeps the last one"
    );
    let due = store.claim_turn_ends(10).await.unwrap();
    assert_eq!(
        due.len(),
        1,
        "only the commit that asked for a turn end queued one"
    );
    assert_eq!(due[0].payload["turn"], "t1");
    assert!(
        store.claim_turn_ends(10).await.unwrap().is_empty(),
        "a claimed turn end waits for its backoff"
    );
    store.delete_turn_end(due[0].id).await.unwrap();
    let left: i64 = sqlx::query_scalar("select count(*) from turn_ends")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_stale_commit_queues_no_turn_end_and_keeps_the_state(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "s7", &event("e1", "hi"), false)
        .await
        .unwrap();
    let lease = claimed(&store, "s7", "me").await;
    steal(&pool, "s7").await;

    let committed = store
        .commit_turn(
            &lease,
            TurnCommit {
                turn_id: "t1".into(),
                provider_state: Some(json!({ "seen": 9 })),
                turn_end: Some(json!({ "turn": "t1" })),
                ..TurnCommit::default()
            },
            TTL,
        )
        .await;

    assert!(committed.is_err());
    let state: Option<Value> =
        sqlx::query_scalar("select provider_state from sessions where key = 's7'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, None);
    assert_eq!(store.claim_turn_ends(10).await.unwrap(), Vec::new());
}

fn with_meta(id: &str, channel: &str) -> InboundEvent {
    serde_json::from_value(json!({ "id": id, "text": "hi", "meta": { "channel": channel } }))
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_turn_end_lists_every_event_the_turn_consumed_with_its_meta(pool: PgPool) {
    let store = Store::new(pool.clone());
    let first = store
        .accept_event("assistant", "s8", &with_meta("e1", "email"), false)
        .await
        .unwrap();
    let lease = claimed(&store, "s8", "me").await;
    store
        .begin_turn(&lease, "t1", &[first.inbox_id], true)
        .await
        .unwrap()
        .unwrap();
    let steered = store
        .accept_event("assistant", "s8", &with_meta("e2", "sms"), false)
        .await
        .unwrap();
    store
        .commit_turn(
            &lease,
            TurnCommit {
                turn_id: "t1".into(),
                consumed: vec![first.inbox_id, steered.inbox_id],
                turn_end: Some(json!({ "turn": "t1", "outcome": "completed" })),
                ..TurnCommit::default()
            },
            TTL,
        )
        .await
        .unwrap();

    let due = store.claim_turn_ends(10).await.unwrap();

    assert_eq!(due[0].attempts, 1);
    assert_eq!(due[0].payload["outcome"], "completed");
    assert_eq!(
        due[0].payload["events"],
        json!([
            { "id": "e1", "meta": { "channel": "email" } },
            { "id": "e2", "meta": { "channel": "sms" } },
        ]),
        "a steered event is listed with the ones the turn started on"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn dead_lettered_events_queue_a_turn_end_saying_so(pool: PgPool) {
    let store = Store::new(pool.clone());
    let first = store
        .accept_event("assistant", "s9", &with_meta("e1", "email"), false)
        .await
        .unwrap();
    let lease = claimed(&store, "s9", "me").await;

    store
        .dead_letter(
            &lease,
            vec![first.inbox_id],
            "model refused".into(),
            Some(json!({ "turn": "t1", "outcome": "dead_lettered" })),
        )
        .await
        .unwrap();

    let due = store.claim_turn_ends(10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].payload["outcome"], "dead_lettered");
    assert_eq!(
        due[0].payload["events"],
        json!([{ "id": "e1", "meta": { "channel": "email" } }])
    );
    let status = store
        .session_status("assistant", "s9")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.pending_events, 0);
}

async fn received_frame(pool: &PgPool, key: &str) -> Value {
    sqlx::query_scalar(
        "select event from live_events where key = $1 and event->>'type' = 'received'",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_received_frame_carries_meta_only_for_agents_that_opt_in(pool: PgPool) {
    let store = Store::new(pool.clone());
    let with_meta: InboundEvent =
        serde_json::from_value(json!({ "id": "e1", "text": "hi", "meta": { "from": "ana" } }))
            .unwrap();

    store
        .accept_event("assistant", "m1", &with_meta, true)
        .await
        .unwrap();
    store
        .accept_event("assistant", "m2", &with_meta, false)
        .await
        .unwrap();
    store
        .accept_event("assistant", "m3", &event("e1", "hi"), true)
        .await
        .unwrap();

    assert_eq!(
        received_frame(&pool, "m1").await["meta"],
        json!({ "from": "ana" })
    );
    assert!(received_frame(&pool, "m2").await.get("meta").is_none());
    assert!(received_frame(&pool, "m3").await.get("meta").is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn taking_over_an_expired_lease_counts_an_attempt_and_a_released_one_does_not(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "crash", &event("e1", "hi"), false)
        .await
        .unwrap();
    let first = claimed(&store, "crash", "a").await;
    assert_eq!((first.failed_attempts, first.taken_over), (0, false));
    expire(&pool, "crash").await;
    let second = claimed(&store, "crash", "b").await;
    assert_eq!(
        (second.failed_attempts, second.taken_over),
        (1, true),
        "a crash mid-turn leaves the lease to expire"
    );
    expire(&pool, "crash").await;
    let third = claimed(&store, "crash", "c").await;
    assert_eq!(third.failed_attempts, 2);
    store.release(&third).await.unwrap();
    let fourth = claimed(&store, "crash", "d").await;
    assert_eq!(
        (fourth.failed_attempts, fourth.taken_over),
        (2, false),
        "a released lease is no crash"
    );
}
