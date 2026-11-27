#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::http::{AppState, router};
use newt_server::live::Topics;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use newt_server::testing::GatedModel;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

async fn serve(store_pool: PgPool, runner: &Runner<StaticBackend, GatedModel>) -> String {
    let state = AppState {
        store: Store::new(store_pool),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live: Topics::default(),
        shutdown: CancellationToken::new(),
        observers: newt_server::events::Observers::default(),
        live_meta: Arc::default(),
        live_stream: Arc::default(),
        metrics: runner.metrics(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    format!("http://{address}")
}

fn runner(pool: &PgPool, model: GatedModel) -> Runner<StaticBackend, GatedModel> {
    Runner::builder(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        model,
    )
    .config(RunnerConfig {
        max_sessions: 1,
        sweep_every: Duration::from_millis(50),
        ..RunnerConfig::default()
    })
    .build()
}

async fn health(base: &str) -> (u16, Value) {
    let response = reqwest::get(format!("{base}/healthz")).await.unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

async fn post(base: &str, key: &str) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/agents/assistant/sessions/{key}/events"))
        .bearer_auth("secret")
        .json(&json!({ "id": "e1", "text": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
}

#[sqlx::test(migrations = "../../migrations")]
async fn healthz_reports_running_and_waiting_sessions(pool: PgPool) {
    let model = GatedModel::closed();
    let runner = runner(&pool, model.clone());
    let _loops = runner.spawn_loops();
    let base = serve(pool, &runner).await;

    let (status, idle) = health(&base).await;
    assert_eq!(status, 200);
    assert_eq!(
        idle,
        json!({ "ready": true, "running": 0, "waiting": 0, "max_sessions": 1, "oldest_wait_ms": 0 })
    );

    post(&base, "a").await;
    post(&base, "b").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let busy = loop {
        let (status, body) = health(&base).await;
        assert_eq!(status, 200);
        if body["running"] == 1 && body["waiting"] == 1 {
            break body;
        }
        assert!(Instant::now() < deadline, "never saturated: {body}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(busy["ready"], true);
    assert_eq!(busy["max_sessions"], 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(health(&base).await.1["oldest_wait_ms"].as_u64().unwrap() >= 40);

    model.release(2);
    runner.idle().await;
    let (_, drained) = health(&base).await;
    assert_eq!(drained["running"], 0);
    assert_eq!(drained["waiting"], 0);
    runner.shutdown().await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn healthz_is_503_when_postgres_does_not_answer(pool: PgPool) {
    let runner = runner(&pool, GatedModel::closed());
    let unreachable = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://newt:newt@127.0.0.1:1/newt")
        .unwrap();
    let base = serve(unreachable, &runner).await;

    let (status, body) = health(&base).await;

    assert_eq!(status, 503);
    assert_eq!(body["ready"], false);
    assert_eq!(body["max_sessions"], 1);
}
