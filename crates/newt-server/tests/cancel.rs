#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::counters::Counter;
use newt_server::http::{AppState, router};
use newt_server::live::{Channel, Notification, Topics, spawn_listener};
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::Store;
use newt_server::testing::GatedModel;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

struct Node {
    base: String,
    store: Store,
    model: GatedModel,
    topics: Topics,
    runner: Runner<StaticBackend, GatedModel>,
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum Notifications {
    Postgres,
    None,
}

async fn start(pool: &PgPool, lease_ttl: Duration, notifications: Notifications) -> Node {
    let mut tasks = Vec::new();
    let topics = match notifications {
        Notifications::Postgres => {
            let (topics, listener) = spawn_listener(pool.clone()).await.unwrap();
            tasks.push(listener);
            topics
        }
        Notifications::None => Topics::default(),
    };
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
            lease_ttl,
            sweep_every: Duration::from_millis(30),
            backoff_base: Duration::from_millis(10),
            shutdown_grace: Duration::from_secs(1),
            ..RunnerConfig::default()
        },
        None,
        topics.clone(),
    );
    tasks.push(runner.spawn_loops());
    let state = AppState {
        store: store.clone(),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live: topics.clone(),
        shutdown: CancellationToken::new(),
        instrumentation: None,
        live_meta: Arc::default(),
        live_stream: Arc::default(),
        metrics: runner.metrics(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tasks.push(tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    }));
    Node {
        base: format!("http://{address}"),
        store,
        model,
        topics,
        runner,
        tasks,
    }
}

async fn start_one(pool: &PgPool) -> Node {
    start(pool, Duration::from_secs(30), Notifications::Postgres).await
}

impl Node {
    async fn post(&self, path: &str, token: &str, body: &Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!(
                "{}/v1/agents/assistant/sessions/s/{path}",
                self.base
            ))
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .unwrap()
    }

    async fn send(&self, id: &str) {
        let posted = self
            .post("events", "secret", &json!({ "id": id, "text": "hi" }))
            .await;
        assert_eq!(posted.status(), 202);
    }

    async fn cancel(&self, body: &Value) -> reqwest::Response {
        self.post("cancel", "secret", body).await
    }

    async fn until(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn frames(&self) -> Vec<Value> {
        self.store
            .live_since("assistant", "s", 0, 200)
            .await
            .unwrap()
            .into_iter()
            .map(|frame| frame.event)
            .collect()
    }

    async fn frame_of_type(&self, kind: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = self
                .frames()
                .await
                .into_iter()
                .find(|frame| frame["type"] == kind)
            {
                return frame;
            }
            assert!(Instant::now() < deadline, "no {kind} frame");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn count_frames(&self, kind: &str) -> usize {
        self.frames()
            .await
            .iter()
            .filter(|frame| frame["type"] == kind)
            .count()
    }

    async fn stop(self) {
        self.runner.shutdown().await;
        for task in self.tasks {
            task.abort();
        }
    }

    fn cancelled(&self) -> u64 {
        self.runner.metrics().counted(Counter::TurnsCancelled)
    }
}

async fn running_turn(pool: &PgPool) -> String {
    sqlx::query_scalar("select turn_id from turn_journal where kind = 'start'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn flag(pool: &PgPool) -> (Option<String>, bool) {
    sqlx::query_as("select cancel_turn, cancel_requested_at is not null from sessions")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn requested_at(pool: &PgPool) -> String {
    sqlx::query_scalar("select cancel_requested_at::text from sessions")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn cancelling_a_running_turn_fails_it_as_cancelled_and_does_not_retry_it(pool: PgPool) {
    let node = start_one(&pool).await;
    node.send("e1").await;
    node.until("the turn to start", || node.model.calls_started() == 1)
        .await;

    let cancelled = node.cancel(&json!({})).await;
    assert_eq!(cancelled.status(), 202);
    let accepted: Value = cancelled.json().await.unwrap();
    assert_eq!(accepted["turn"], running_turn(&pool).await);

    let failed = node.frame_of_type("turn_failed").await;
    assert_eq!(failed["reason"], "cancelled");

    node.runner.idle().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        node.model.calls_started(),
        1,
        "the cancelled turn ran again"
    );
    let (consumed, reason): (bool, Option<String>) = sqlx::query_as(
        "select consumed_at is not null, dead_letter from inbox where event_id = 'e1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(consumed);
    assert_eq!(reason.as_deref(), Some("cancelled"));
    assert_eq!(
        flag(&pool).await,
        (None, false),
        "the flag outlived the turn"
    );
    assert_eq!(
        node.runner.metrics().counted(Counter::Retries),
        0,
        "a cancel is not a retry"
    );

    node.model.release(1);
    node.send("e2").await;
    node.until("the next turn to finish", || {
        node.model.calls_started() == 2
    })
    .await;
    node.runner.idle().await;
    let history = node
        .store
        .session_status("assistant", "s")
        .await
        .unwrap()
        .unwrap()
        .history_entries;
    assert_eq!(history, 2, "the next turn committed normally");
    node.stop().await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_cancel_taken_by_one_instance_stops_the_turn_running_on_another(pool: PgPool) {
    let first = start_one(&pool).await;
    let second = start_one(&pool).await;
    first.send("e1").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let (owner, other) = loop {
        if first.model.calls_started() == 1 {
            break (&first, &second);
        }
        if second.model.calls_started() == 1 {
            break (&second, &first);
        }
        assert!(Instant::now() < deadline, "no instance started the turn");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let turn = running_turn(&pool).await;
    let cancelled = other.cancel(&json!({ "turn": turn })).await;
    assert_eq!(cancelled.status(), 202);
    let accepted: Value = cancelled.json().await.unwrap();
    assert_eq!(accepted["turn"], turn);

    let failed = owner.frame_of_type("turn_failed").await;
    assert_eq!(failed["reason"], "cancelled");
    owner
        .until("the owner to count the cancel", || owner.cancelled() == 1)
        .await;
    assert_eq!(other.cancelled(), 0);
    assert_eq!(other.model.calls_started(), 0);
    tokio::join!(first.stop(), second.stop());
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_request_whose_notification_was_lost_is_found_at_the_next_heartbeat(pool: PgPool) {
    let node = start(&pool, Duration::from_millis(600), Notifications::None).await;
    node.send("e1").await;
    node.until("the turn to start", || node.model.calls_started() == 1)
        .await;
    assert_eq!(node.count_frames("turn_failed").await, 0);

    let written = sqlx::query(
        "update sessions set cancel_turn = $1, cancel_requested_at = now() where key = 's'",
    )
    .bind(running_turn(&pool).await)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(written.rows_affected(), 1);

    let failed = node.frame_of_type("turn_failed").await;
    assert_eq!(failed["reason"], "cancelled");
    assert_eq!(node.cancelled(), 1);
    node.stop().await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_stale_turn_id_is_a_409_and_a_flag_for_another_turn_cancels_nothing(pool: PgPool) {
    let node = start(&pool, Duration::from_millis(600), Notifications::None).await;
    node.send("e1").await;
    node.until("the turn to start", || node.model.calls_started() == 1)
        .await;
    let turn = running_turn(&pool).await;

    let stale = node.cancel(&json!({ "turn": "an-earlier-turn" })).await;
    assert_eq!(stale.status(), 409);
    let body: Value = stale.json().await.unwrap();
    assert_eq!(body["error"]["code"], "turn_not_running");
    assert_eq!(flag(&pool).await, (None, false));

    sqlx::query("update sessions set cancel_turn = 'an-earlier-turn' where key = 's'")
        .execute(&pool)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(node.count_frames("turn_failed").await, 0);
    assert_eq!(node.cancelled(), 0);

    node.model.release(1);
    let done = node.frame_of_type("turn_completed").await;
    assert_eq!(done["turn"], turn);
    node.runner.idle().await;
    assert_eq!(node.count_frames("turn_failed").await, 0);
    assert_eq!(
        flag(&pool).await,
        (None, false),
        "the stale flag outlived the turn"
    );
    node.stop().await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn cancelling_after_the_turn_ended_or_in_an_unknown_session_is_refused(pool: PgPool) {
    let node = start_one(&pool).await;
    let unknown = node.cancel(&json!({})).await;
    assert_eq!(unknown.status(), 404);
    let body: Value = unknown.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");

    node.model.release(1);
    node.send("e1").await;
    node.frame_of_type("turn_completed").await;
    node.runner.idle().await;

    let ended = node.cancel(&json!({})).await;
    assert_eq!(ended.status(), 409);
    let body: Value = ended.json().await.unwrap();
    assert_eq!(body["error"]["code"], "turn_not_running");
    assert_eq!(flag(&pool).await, (None, false));

    let unauthorized = node.post("cancel", "wrong", &json!({})).await;
    assert_eq!(unauthorized.status(), 401);
    node.stop().await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn repeating_a_cancel_answers_202_again_and_cancels_once(pool: PgPool) {
    let node = start(&pool, Duration::from_secs(30), Notifications::None).await;
    node.send("e1").await;
    node.until("the turn to start", || node.model.calls_started() == 1)
        .await;
    let turn = running_turn(&pool).await;

    let first = node.cancel(&json!({})).await;
    assert_eq!(first.status(), 202);
    let first_at = requested_at(&pool).await;
    let second = node.cancel(&json!({ "turn": turn })).await;
    assert_eq!(second.status(), 202);
    let accepted: Value = second.json().await.unwrap();
    assert_eq!(accepted["turn"], turn);
    assert_eq!(
        first_at,
        requested_at(&pool).await,
        "a repeat moved the request time"
    );

    node.topics.publish(Notification {
        channel: Channel::Cancel,
        topic: "assistant/s".to_owned(),
    });
    node.frame_of_type("turn_failed").await;
    node.runner.idle().await;
    assert_eq!(node.cancelled(), 1);
    assert_eq!(node.count_frames("turn_failed").await, 1);
    node.stop().await;
}
