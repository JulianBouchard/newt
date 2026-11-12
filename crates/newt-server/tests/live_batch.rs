#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use newt_server::store::{InboundEvent, Store};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn batched_live_events_keep_gapless_ordered_sequence_numbers(pool: PgPool) {
    let store = Store::new(pool);
    let event: InboundEvent = serde_json::from_value(json!({ "id": "e1", "text": "hi" })).unwrap();
    store.accept_event("assistant", "k", &event).await.unwrap();
    let before = store
        .live_since("assistant", "k", 0, 100)
        .await
        .unwrap()
        .len();

    store
        .append_live_batch(
            "assistant",
            "k",
            (0..3).map(|n| json!({ "n": n })).collect(),
            true,
        )
        .await
        .unwrap();
    store
        .append_live_batch(
            "assistant",
            "k",
            vec![json!({ "n": 3 }), json!({ "n": 4 })],
            false,
        )
        .await
        .unwrap();

    let live = store.live_since("assistant", "k", 0, 100).await.unwrap();
    let seqs: Vec<i64> = live.iter().map(|event| event.seq).collect();
    let expected: Vec<i64> = (1..=i64::try_from(before + 5).unwrap()).collect();
    assert_eq!(seqs, expected);
    let ns: Vec<i64> = live[before..]
        .iter()
        .map(|event| event.event["n"].as_i64().unwrap())
        .collect();
    assert_eq!(ns, vec![0, 1, 2, 3, 4]);
}
