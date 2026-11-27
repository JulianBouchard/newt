#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::time::{Duration, Instant};

use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::counters::Counter;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, Store};
use newt_server::testing::GatedModel;
use serde_json::json;
use sqlx::PgPool;

fn event() -> InboundEvent {
    serde_json::from_value(json!({ "id": "e1", "text": "hi" })).unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn sessions_past_the_waiting_cap_stay_in_the_inbox_until_a_slot_frees(pool: PgPool) {
    let store = Store::new(pool.clone());
    let model = GatedModel::closed();
    let runner = Runner::builder(
        store.clone(),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        model.clone(),
    )
    .config(RunnerConfig {
        max_sessions: 1,
        reserved_interactive: 0,
        max_waiting: 2,
        sweep_every: Duration::from_millis(20),
        ..RunnerConfig::default()
    })
    .build();
    let metrics = runner.metrics();
    let _loops = runner.spawn_loops();
    for index in 0..6 {
        store
            .accept_event("assistant", &format!("s{index}"), &event(), false)
            .await
            .unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while metrics.counted(Counter::WakesDropped) == 0 {
        assert!(Instant::now() < deadline, "no wake was ever dropped");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for _ in 0..20 {
        let load = metrics.load();
        assert_eq!(load.running, 1);
        assert!(load.waiting <= 2, "{load:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(model.calls_started(), 1);
    let text = metrics.render();
    assert!(
        text.lines()
            .any(|line| line.starts_with("newt_wakes_dropped_total ") && !line.ends_with(" 0")),
        "{text}"
    );

    model.release(6);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pending: i64 =
            sqlx::query_scalar("select count(*) from inbox where consumed_at is null")
                .fetch_one(&pool)
                .await
                .unwrap();
        if pending == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{pending} events never ran after the slot freed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    runner.shutdown().await;
}
