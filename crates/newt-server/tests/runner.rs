#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::ModelRequest;
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::workspace::Workspace;
use newt_server::backend::StaticBackend;
use newt_server::http::{AppState, router};
use newt_server::live::{Notification, spawn_listener};
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, SessionStatus, Store};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct ScriptedModel {
    script: Mutex<VecDeque<Message>>,
    steal_lease: Option<PgPool>,
    delay: Duration,
    started: Arc<tokio::sync::Notify>,
}

fn respond(message: Message) -> ModelResponse {
    ModelResponse {
        message,
        usage: Usage::default(),
        stop: StopReason::ToolUse,
        timing: CallTiming::default(),
        response_id: None,
        request_ids: BTreeMap::new(),
    }
}

impl Model for ScriptedModel {
    async fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        if let Some(pool) = &self.steal_lease {
            sqlx::query(
                "update sessions set lease_owner = 'thief', lease_epoch = lease_epoch + 1, \
                 lease_expires_at = now() + interval '1 hour'",
            )
            .execute(pool)
            .await
            .unwrap();
        }
        self.started.notify_one();
        tokio::time::sleep(self.delay).await;
        let next = self.script.lock().unwrap().pop_front();
        next.map(respond)
            .ok_or_else(|| ProviderError::Protocol("script exhausted".into()))
    }
}

fn reply_and_end(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![
            Part::text(text),
            Part::ToolCall(ToolCall {
                id: format!("end-{text}"),
                name: "end_turn".into(),
                arguments: json!({ "nothing_new": "just a chat" }),
            }),
        ],
    }
}

fn fast() -> RunnerConfig {
    RunnerConfig {
        lease_ttl: Duration::from_secs(1),
        sweep_every: Duration::from_millis(50),
        shutdown_grace: Duration::from_secs(1),
        backoff_base: Duration::from_millis(10),
        max_attempts: 3,
        max_sessions: 8,
        cache_key_buckets: newt_server::cache_keys::Buckets::Fixed(1),
    }
}

fn event(id: &str, text: &str) -> InboundEvent {
    serde_json::from_value(json!({ "id": id, "text": text })).unwrap()
}

fn runner(
    pool: &PgPool,
    script: Vec<Message>,
    steal: bool,
    notifications: broadcast::Sender<Notification>,
) -> Runner<StaticBackend, ScriptedModel> {
    slow_runner(
        pool,
        script,
        steal,
        notifications,
        Duration::ZERO,
        Arc::new(tokio::sync::Notify::new()),
    )
}

fn slow_runner(
    pool: &PgPool,
    script: Vec<Message>,
    steal: bool,
    notifications: broadcast::Sender<Notification>,
    delay: Duration,
    started: Arc<tokio::sync::Notify>,
) -> Runner<StaticBackend, ScriptedModel> {
    Runner::new(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        ScriptedModel {
            script: Mutex::new(script.into()),
            steal_lease: steal.then(|| pool.clone()),
            delay,
            started,
        },
        fast(),
        None,
        notifications,
    )
}

async fn wait_for(
    store: &Store,
    key: &str,
    done: impl Fn(&SessionStatus) -> bool,
) -> SessionStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = store.session_status("assistant", key).await.unwrap();
        if let Some(status) = &status
            && done(status)
        {
            return status.clone();
        }
        assert!(
            Instant::now() < deadline,
            "session {key} never reached the expected state; last seen {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_posted_event_runs_a_turn_and_commits_it(pool: PgPool) {
    let (live, _listener) = spawn_listener(pool.clone()).await.unwrap();
    let runner = runner(&pool, vec![reply_and_end("Hello!")], false, live.clone());
    let _loops = runner.spawn_loops();
    let state = AppState {
        store: Store::new(pool.clone()),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live,
        shutdown: CancellationToken::new(),
        instrumentation: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

    let response = reqwest::Client::new()
        .post(format!(
            "http://{address}/v1/agents/assistant/sessions/t1/events"
        ))
        .bearer_auth("secret")
        .json(&json!({ "id": "e1", "text": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);

    let store = Store::new(pool);
    let status = wait_for(&store, "t1", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;
    assert_eq!(status.history_entries, 2);
    let kinds: Vec<String> = store
        .live_since("assistant", "t1", 0, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.event["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds.first().map(String::as_str), Some("received"));
    assert!(kinds.iter().any(|kind| kind == "turn_started"));
    assert_eq!(kinds.last().map(String::as_str), Some("turn_completed"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn queued_events_are_all_processed(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t2", &event("e1", "one"))
        .await
        .unwrap();
    store
        .accept_event("assistant", "t2", &event("e2", "two"))
        .await
        .unwrap();
    let runner = runner(
        &pool,
        vec![reply_and_end("a"), reply_and_end("b")],
        false,
        broadcast::channel(16).0,
    );

    runner.wake("assistant", "t2");
    let status = wait_for(&store, "t2", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;

    assert!(status.history_entries >= 3, "{status:?}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_failing_turn_backs_off_and_keeps_its_events(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t3", &event("e1", "hi"))
        .await
        .unwrap();
    let thinking_aloud = Message {
        role: Role::Assistant,
        parts: vec![Part::text("Let me think about that...")],
    };
    let runner = runner(&pool, vec![thinking_aloud], false, broadcast::channel(16).0);

    runner.wake("assistant", "t3");
    runner.idle().await;

    let status = store
        .session_status("assistant", "t3")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.pending_events, 1);
    assert_eq!(status.history_entries, 0);
    assert!(status.lease_owner.is_none());
    let (attempts, backing_off): (i32, bool) = sqlx::query_as(
        "select failed_attempts, retry_after is not null from sessions where key = 't3'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    assert!(backing_off);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_runner_that_lost_its_lease_cannot_commit(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t4", &event("e1", "hi"))
        .await
        .unwrap();
    let runner = runner(
        &pool,
        vec![reply_and_end("too late")],
        true,
        broadcast::channel(16).0,
    );

    runner.wake("assistant", "t4");
    runner.idle().await;

    let status = store
        .session_status("assistant", "t4")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.history_entries, 0);
    assert_eq!(status.pending_events, 1);
    assert_eq!(status.lease_owner.as_deref(), Some("thief"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_sweeper_picks_up_an_orphaned_session(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t5", &event("e1", "hi"))
        .await
        .unwrap();
    sqlx::query(
        "update sessions set lease_owner = 'crashed', lease_epoch = 7, \
         lease_expires_at = now() - interval '1 second' where key = 't5'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let runner = runner(
        &pool,
        vec![reply_and_end("recovered")],
        false,
        broadcast::channel(16).0,
    );

    let _loops = runner.spawn_loops();
    let status = wait_for(&store, "t5", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;

    assert_eq!(status.history_entries, 2);
    assert_eq!(status.lease_epoch, 8);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_message_sent_mid_turn_joins_that_turn(pool: PgPool) {
    let (live, _listener) = spawn_listener(pool.clone()).await.unwrap();
    let store = Store::new(pool.clone());
    let started = Arc::new(tokio::sync::Notify::new());
    let runner = slow_runner(
        &pool,
        vec![reply_and_end("Berlin it is")],
        false,
        live,
        Duration::from_millis(400),
        Arc::clone(&started),
    );
    store
        .accept_event("assistant", "t6", &event("e1", "roles in Lisbon"))
        .await
        .unwrap();

    runner.wake("assistant", "t6");
    started.notified().await;
    store
        .accept_event("assistant", "t6", &event("e2", "actually Berlin"))
        .await
        .unwrap();
    let status = wait_for(&store, "t6", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;

    assert_eq!(status.history_entries, 3);
    let turns: Vec<String> =
        sqlx::query_scalar("select distinct turn_id from history where key = 't6'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(turns.len(), 1);
    let kinds: Vec<String> = store
        .live_since("assistant", "t6", 0, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.event["type"].as_str().unwrap().to_owned())
        .collect();
    assert!(kinds.iter().any(|kind| kind == "turn_steered"), "{kinds:?}");
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| *kind == "turn_completed")
            .count(),
        1
    );
}

struct PanicsOnce {
    fired: std::sync::atomic::AtomicBool,
    inner: StaticBackend,
}

impl newt_server::backend::Backend for PanicsOnce {
    type Host = newt_core::tools::NoTools;

    fn prepare(
        &self,
        agent: &str,
        key: &str,
    ) -> impl std::future::Future<
        Output = Result<
            newt_server::backend::Prepared<newt_core::tools::NoTools>,
            newt_server::backend::BackendError,
        >,
    > + Send {
        let first = !self.fired.swap(true, std::sync::atomic::Ordering::Relaxed);
        assert!(!first, "simulated crash while preparing the turn");
        self.inner.prepare(agent, key)
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_session_whose_task_panics_recovers_after_its_lease_expires(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t7", &event("e1", "hi"))
        .await
        .unwrap();
    let runner = Runner::new(
        Store::new(pool.clone()),
        PanicsOnce {
            fired: std::sync::atomic::AtomicBool::new(false),
            inner: StaticBackend {
                system: "You are a helpful agent.".into(),
                model: "test".into(),
                workspace: Workspace::default(),
            },
        },
        ScriptedModel {
            script: Mutex::new(vec![reply_and_end("back")].into()),
            steal_lease: None,
            delay: Duration::ZERO,
            started: Arc::new(tokio::sync::Notify::new()),
        },
        fast(),
        None,
        broadcast::channel(16).0,
    );

    let _loops = runner.spawn_loops();
    let status = wait_for(&store, "t7", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;

    assert_eq!(status.history_entries, 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_turn_that_keeps_failing_is_dead_lettered(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t8", &event("e1", "hi"))
        .await
        .unwrap();
    let runner = runner(&pool, Vec::new(), false, broadcast::channel(16).0);

    let _loops = runner.spawn_loops();
    wait_for(&store, "t8", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;

    let (reason, attempts): (Option<String>, i32) = sqlx::query_as(
        "select i.dead_letter, s.failed_attempts from inbox i \
         join sessions s on s.agent = i.agent and s.key = i.key where i.key = 't8'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(reason.unwrap().contains("script exhausted"));
    assert_eq!(attempts, 0);
    let status = store
        .session_status("assistant", "t8")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.history_entries, 0);
}
