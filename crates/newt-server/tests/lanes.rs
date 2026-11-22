#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::time::{Duration, Instant};

use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::live::Topics;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, Store};
use newt_server::testing::GatedModel;
use serde_json::json;
use sqlx::PgPool;

fn event(id: &str, priority: &str) -> InboundEvent {
    serde_json::from_value(json!({ "id": id, "text": "hi", "priority": priority })).unwrap()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn background_turns_leave_the_reserved_slots_to_interactive_ones(pool: PgPool) {
    let store = Store::new(pool.clone());
    let model = GatedModel::closed();
    let runner = Runner::new(
        store.clone(),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        model.clone(),
        RunnerConfig {
            max_sessions: 4,
            reserved_interactive: 1,
            sweep_every: Duration::from_millis(20),
            ..RunnerConfig::default()
        },
        None,
        Topics::default(),
    );
    let metrics = runner.metrics();
    let _loops = runner.spawn_loops();

    for index in 0..4 {
        store
            .accept_event(
                "assistant",
                &format!("background-{index}"),
                &event("e1", "background"),
                false,
            )
            .await
            .unwrap();
    }
    until("three background turns to start", || {
        model.calls_started() == 3
    })
    .await;
    until("the fourth background turn to queue", || {
        metrics.load().waiting == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(model.calls_started(), 3, "a fourth background turn started");
    assert_eq!(metrics.load().running, 3);

    store
        .accept_event("assistant", "chat", &event("e1", "interactive"), false)
        .await
        .unwrap();
    until("the interactive turn to start", || {
        model.calls_started() == 4
    })
    .await;
    let load = metrics.load();
    assert_eq!((load.running, load.waiting), (4, 1));

    model.release(5);
    runner.idle().await;
    runner.shutdown().await;
    let pending: i64 = sqlx::query_scalar("select count(*) from inbox where consumed_at is null")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 0, "every turn ran once the slots freed up");
}
