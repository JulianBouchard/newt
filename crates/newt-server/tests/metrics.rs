#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::http::{AppState, router};
use newt_server::live::spawn_listener;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use newt_server::testing::GatedModel;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

struct Harness {
    base: String,
    model: GatedModel,
    runner: Runner<StaticBackend, GatedModel>,
    notifications: tokio::task::JoinHandle<()>,
}

async fn start(pool: &PgPool) -> Harness {
    let store = Store::new(pool.clone()).with_max_pending(HashMap::from([("assistant".into(), 2)]));
    let model = GatedModel::closed();
    let (live, notifications) = spawn_listener(pool.clone()).await.unwrap();
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
        lease_ttl: Duration::from_millis(600),
        sweep_every: Duration::from_millis(30),
        backoff_base: Duration::from_millis(10),
        ..RunnerConfig::default()
    })
    .live(live.clone())
    .build();
    let _loops = runner.spawn_loops();
    let state = AppState {
        store,
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live,
        shutdown: CancellationToken::new(),
        observers: newt_server::events::Observers::default(),
        live_meta: Arc::default(),
        live_stream: Arc::default(),
        metrics: runner.metrics(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    Harness {
        base: format!("http://{address}"),
        model,
        runner,
        notifications,
    }
}

impl Harness {
    async fn post(&self, key: &str, path: &str, body: &Value) -> u16 {
        reqwest::Client::new()
            .post(format!(
                "{}/v1/agents/assistant/sessions/{key}/{path}",
                self.base
            ))
            .bearer_auth("secret")
            .json(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn scrape(&self) -> HashMap<String, f64> {
        let response = reqwest::get(format!("{}/metrics", self.base))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let content_type = response.headers()["content-type"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(content_type.starts_with("text/plain"), "{content_type}");
        parse_exposition(&response.text().await.unwrap())
    }

    async fn until(&self, what: &str, series: &str, wanted: f64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let scraped = self.scrape().await;
            if scraped.get(series).copied() == Some(wanted) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {scraped:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn model_calls(&self, what: &str, wanted: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.model.calls_started() < wanted {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn parse_exposition(text: &str) -> HashMap<String, f64> {
    let mut typed = HashSet::new();
    let mut samples = HashMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').unwrap();
            assert!(["counter", "gauge", "histogram"].contains(&kind), "{line}");
            typed.insert(name.to_owned());
        } else if line.starts_with("# HELP ") {
            assert!(line.len() > "# HELP x ".len(), "{line}");
        } else {
            let (series, value) = line.rsplit_once(' ').unwrap();
            let value: f64 = value.parse().unwrap();
            let name = series.split('{').next().unwrap();
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|suffix| name.strip_suffix(suffix))
                .filter(|family| typed.contains(*family))
                .unwrap_or(name);
            assert!(typed.contains(family), "sample without a TYPE line: {line}");
            assert!(
                series.matches('{').count() == series.matches('}').count(),
                "{line}"
            );
            assert!(samples.insert(series.to_owned(), value).is_none(), "{line}");
        }
    }
    samples
}

#[sqlx::test(migrations = "../../migrations")]
async fn metrics_count_events_turns_cancels_and_slot_waits(pool: PgPool) {
    let harness = start(&pool).await;
    assert_eq!(
        harness
            .post("s", "events", &json!({ "id": "e1", "text": "hi" }))
            .await,
        202
    );
    assert_eq!(
        harness
            .post("s", "events", &json!({ "id": "e1", "text": "hi" }))
            .await,
        202
    );
    assert_eq!(
        harness
            .post("s", "events", &json!({ "id": "e1", "text": "other" }))
            .await,
        409
    );
    harness
        .until(
            "the turn to start",
            "newt_turns_total{outcome=\"started\"}",
            1.0,
        )
        .await;
    let busy = harness.scrape().await;
    assert_eq!(busy["newt_running_turns"], 1.0);
    assert_eq!(busy["newt_waiting_turns"], 0.0);

    harness.model_calls("the first model call", 1).await;
    assert_eq!(
        harness
            .post("s", "events", &json!({ "id": "e2", "text": "hi" }))
            .await,
        202
    );
    assert_eq!(
        harness
            .post("s", "events", &json!({ "id": "e3", "text": "hi" }))
            .await,
        429
    );
    harness.model_calls("e2 to steer the turn", 2).await;

    assert_eq!(harness.post("s", "cancel", &json!({})).await, 202);
    harness
        .until(
            "the cancel to count",
            "newt_turns_total{outcome=\"cancelled\"}",
            1.0,
        )
        .await;

    harness.model.release(1);
    assert_eq!(
        harness
            .post("t", "events", &json!({ "id": "e1", "text": "hi" }))
            .await,
        202
    );
    harness
        .until(
            "a turn to complete",
            "newt_turns_total{outcome=\"completed\"}",
            1.0,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(10), harness.runner.idle())
        .await
        .unwrap();

    let scraped = harness.scrape().await;
    assert_eq!(scraped["newt_events_total{outcome=\"accepted\"}"], 3.0);
    assert_eq!(scraped["newt_events_total{outcome=\"duplicate\"}"], 1.0);
    assert_eq!(scraped["newt_events_total{outcome=\"conflicted\"}"], 1.0);
    assert_eq!(scraped["newt_events_total{outcome=\"limited\"}"], 1.0);
    assert_eq!(scraped["newt_turns_total{outcome=\"started\"}"], 2.0);
    assert_eq!(scraped["newt_turns_total{outcome=\"failed\"}"], 0.0);
    assert_eq!(scraped["newt_retries_total"], 0.0);
    assert_eq!(scraped["newt_dead_letters_total"], 2.0);
    assert_eq!(scraped["newt_commits_fenced_total"], 0.0);
    assert_eq!(scraped["newt_running_turns"], 0.0);
    assert_eq!(scraped["newt_max_sessions"], 16.0);
    assert_eq!(scraped["newt_slot_wait_seconds_count"], 2.0);
    assert_eq!(scraped["newt_slot_wait_seconds_bucket{le=\"+Inf\"}"], 2.0);
    let buckets: Vec<f64> = [
        "0.001", "0.005", "0.025", "0.1", "0.5", "1", "5", "30", "120", "+Inf",
    ]
    .iter()
    .map(|bound| scraped[&format!("newt_slot_wait_seconds_bucket{{le=\"{bound}\"}}")])
    .collect();
    assert!(
        buckets.windows(2).all(|pair| pair[0] <= pair[1]),
        "{buckets:?}"
    );
    harness.runner.shutdown().await;
    harness.notifications.abort();
}
