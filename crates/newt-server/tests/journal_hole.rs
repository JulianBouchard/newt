#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::time::Duration;

use newt_core::journal::{Journal, JournalEntry};
use newt_server::journal::TurnJournal;
use newt_server::store::{InboundEvent, Store};
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

fn call(id: &str) -> JournalEntry {
    JournalEntry::OutwardIntent {
        call_id: id.to_owned(),
    }
}

async fn set_epoch(pool: &PgPool, epoch: i64) {
    sqlx::query("update sessions set lease_epoch = $1 where key = 's'")
        .bind(epoch)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn after_a_journal_write_fails_no_later_entry_is_written(pool: PgPool) {
    let store = Store::new(pool.clone());
    let event: InboundEvent = serde_json::from_value(json!({ "id": "e1", "text": "hi" })).unwrap();
    store
        .accept_event("assistant", "s", &event, false)
        .await
        .unwrap();
    let (lease, _waited) = store
        .claim("assistant", "s", "owner", Duration::from_secs(60))
        .await
        .unwrap();
    let lease = lease.unwrap();
    let (journal, writer) = TurnJournal::spawn(
        store,
        lease.clone(),
        "t1".to_owned(),
        1,
        CancellationToken::new(),
    );

    set_epoch(&pool, lease.epoch + 1).await;
    journal.append(call("c1"));
    assert!(journal.append_and_wait(call("c2")).await.is_err());
    set_epoch(&pool, lease.epoch).await;
    journal.append(call("c3"));
    drop(journal);
    writer.await.unwrap();

    let rows: i64 = sqlx::query_scalar("select count(*) from turn_journal where turn_id = 't1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 0,
        "an entry written after the failed one would leave a hole"
    );
}
