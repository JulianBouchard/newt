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
use newt_server::failures::{AfterFailure, FailedTurn, FailurePolicy, GiveUpAfter};
use newt_server::http::{AppState, router};
use newt_server::live::{Topics, spawn_listener};
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, SessionStatus, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
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
        max_sessions: 8,
        reserved_interactive: 2,
        max_waiting: 64,
        cache_key_buckets: newt_server::cache_keys::Buckets::Fixed(1),
        retention: newt_server::retention::Retention::default(),
    }
}

fn event(id: &str, text: &str) -> InboundEvent {
    serde_json::from_value(json!({ "id": id, "text": text })).unwrap()
}

fn runner(
    pool: &PgPool,
    script: Vec<Message>,
    steal: bool,
    notifications: Topics,
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
    notifications: Topics,
    delay: Duration,
    started: Arc<tokio::sync::Notify>,
) -> Runner<StaticBackend, ScriptedModel> {
    Runner::builder(
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
    )
    .config(fast())
    .failures(GiveUpAfter { attempts: 3 })
    .live(notifications)
    .build()
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
        observers: newt_server::events::Observers::default(),
        live_meta: Arc::default(),
        live_stream: Arc::default(),
        metrics: Arc::default(),
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
        .accept_event("assistant", "t2", &event("e1", "one"), false)
        .await
        .unwrap();
    store
        .accept_event("assistant", "t2", &event("e2", "two"), false)
        .await
        .unwrap();
    let runner = runner(
        &pool,
        vec![reply_and_end("a"), reply_and_end("b")],
        false,
        Topics::default(),
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
        .accept_event("assistant", "t3", &event("e1", "hi"), false)
        .await
        .unwrap();
    let silent = Message {
        role: Role::Assistant,
        parts: Vec::new(),
    };
    let runner = runner(&pool, vec![silent], false, Topics::default());

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
        .accept_event("assistant", "t4", &event("e1", "hi"), false)
        .await
        .unwrap();
    let runner = runner(
        &pool,
        vec![reply_and_end("too late")],
        true,
        Topics::default(),
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
        .accept_event("assistant", "t5", &event("e1", "hi"), false)
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
        Topics::default(),
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
        .accept_event("assistant", "t6", &event("e1", "roles in Lisbon"), false)
        .await
        .unwrap();

    runner.wake("assistant", "t6");
    started.notified().await;
    store
        .accept_event("assistant", "t6", &event("e2", "actually Berlin"), false)
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
        request: &newt_server::backend::TurnRequest,
    ) -> impl std::future::Future<
        Output = Result<
            newt_server::backend::Prepared<newt_core::tools::NoTools>,
            newt_server::backend::BackendError,
        >,
    > + Send {
        let first = !self.fired.swap(true, std::sync::atomic::Ordering::Relaxed);
        assert!(!first, "simulated crash while preparing the turn");
        self.inner.prepare(agent, key, request)
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_session_whose_task_panics_recovers_after_its_lease_expires(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t7", &event("e1", "hi"), false)
        .await
        .unwrap();
    let runner = Runner::builder(
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
    )
    .config(fast())
    .build();

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
        .accept_event("assistant", "t8", &event("e1", "hi"), false)
        .await
        .unwrap();
    let runner = runner(&pool, Vec::new(), false, Topics::default());

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

/// A backend that counts its turns in the provider state and wants turn ends.
struct Remembers {
    inner: StaticBackend,
    requests: Mutex<Vec<newt_server::backend::TurnRequest>>,
    turn_ends: Mutex<Vec<Value>>,
    refuse_turn_ends: std::sync::atomic::AtomicBool,
}

impl newt_server::backend::Backend for Remembers {
    type Host = newt_core::tools::NoTools;

    async fn prepare(
        &self,
        agent: &str,
        key: &str,
        request: &newt_server::backend::TurnRequest,
    ) -> Result<
        newt_server::backend::Prepared<newt_core::tools::NoTools>,
        newt_server::backend::BackendError,
    > {
        self.requests.lock().unwrap().push(request.clone());
        let turns = request
            .state
            .as_ref()
            .and_then(|state| state["turns"].as_i64())
            .unwrap_or(0);
        let mut prepared = self.inner.prepare(agent, key, request).await?;
        prepared.state.replace(json!({ "turns": turns + 1 }));
        if let Value::Object(patch) = json!({ "patched": true }) {
            prepared.state.patch(&patch);
        }
        prepared.turn_end = true;
        prepared.turn_state.set(json!({ "sent": false }));
        Ok(prepared)
    }

    fn turn_end(
        &self,
        _agent: &str,
        payload: &Value,
    ) -> impl std::future::Future<Output = Result<(), newt_server::backend::BackendError>> + Send
    {
        if self
            .refuse_turn_ends
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return std::future::ready(Err(newt_server::backend::BackendError(
                "provider down".into(),
            )));
        }
        self.turn_ends.lock().unwrap().push(payload.clone());
        std::future::ready(Ok(()))
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_provider_state_carries_to_the_next_turn_and_each_turn_end_is_delivered(pool: PgPool) {
    let store = Store::new(pool.clone());
    let backend = Remembers {
        inner: StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        requests: Mutex::new(Vec::new()),
        turn_ends: Mutex::new(Vec::new()),
        refuse_turn_ends: std::sync::atomic::AtomicBool::new(false),
    };
    let runner = Runner::builder(
        store.clone(),
        backend,
        ScriptedModel {
            script: Mutex::new(vec![reply_and_end("one"), reply_and_end("two")].into()),
            steal_lease: None,
            delay: Duration::ZERO,
            started: Arc::new(tokio::sync::Notify::new()),
        },
    )
    .config(fast())
    .build();
    let first: InboundEvent =
        serde_json::from_value(json!({ "id": "e1", "text": "hi", "meta": { "channel": "email" } }))
            .unwrap();

    store
        .accept_event("assistant", "s1", &first, false)
        .await
        .unwrap();
    runner.wake("assistant", "s1");
    runner.idle().await;
    store
        .accept_event("assistant", "s1", &event("e2", "again"), false)
        .await
        .unwrap();
    runner.wake("assistant", "s1");
    runner.idle().await;
    let delivered = runner.deliver_turn_ends().await;

    assert_eq!(delivered, 2);
    let backend = runner.backend();
    let requests = backend.requests.lock().unwrap().clone();
    assert_eq!(requests[0].events[0].id, "e1");
    assert_eq!(
        requests[0].events[0].meta,
        Some(json!({ "channel": "email" }))
    );
    assert_eq!(requests[0].state, None);
    assert_eq!(
        requests[1].state,
        Some(json!({ "turns": 1, "patched": true })),
        "the saved state includes the turn's patches"
    );
    let mut ends = backend.turn_ends.lock().unwrap().clone();
    ends.sort_by_key(|end| end["state"]["turns"].as_i64());
    assert_eq!(ends[0]["session"], "s1");
    assert_eq!(ends[0]["outcome"], "completed");
    assert_eq!(
        ends[0]["reply"], "one",
        "the turn-end carries what the agent said"
    );
    assert_eq!(
        ends[0]["events"],
        json!([{ "id": "e1", "meta": { "channel": "email" } }])
    );
    assert_eq!(ends[0]["turn_state"], json!({ "sent": false }));
    assert_eq!(ends[1]["state"], json!({ "turns": 2, "patched": true }));
    assert_eq!(
        runner.deliver_turn_ends().await,
        0,
        "delivered turn ends are gone"
    );
}

fn remembering_runner(pool: &PgPool, script: Vec<Message>) -> Runner<Remembers, ScriptedModel> {
    Runner::builder(
        Store::new(pool.clone()),
        Remembers {
            inner: StaticBackend {
                system: "You are a helpful agent.".into(),
                model: "test".into(),
                workspace: Workspace::default(),
            },
            requests: Mutex::new(Vec::new()),
            turn_ends: Mutex::new(Vec::new()),
            refuse_turn_ends: std::sync::atomic::AtomicBool::new(true),
        },
        ScriptedModel {
            script: Mutex::new(script.into()),
            steal_lease: None,
            delay: Duration::ZERO,
            started: Arc::new(tokio::sync::Notify::new()),
        },
    )
    .config(fast())
    .build()
}

async fn make_turn_ends_due(pool: &PgPool) {
    sqlx::query("update turn_ends set next_attempt_at = now()")
        .execute(pool)
        .await
        .unwrap();
}

async fn turn_ends_left(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from turn_ends")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_refused_turn_end_waits_for_its_backoff_and_is_sent_again(pool: PgPool) {
    let store = Store::new(pool.clone());
    let runner = remembering_runner(&pool, vec![reply_and_end("one")]);
    store
        .accept_event("assistant", "s2", &event("e1", "hi"), false)
        .await
        .unwrap();
    runner.wake("assistant", "s2");
    runner.idle().await;

    assert_eq!(runner.deliver_turn_ends().await, 0);
    assert_eq!(turn_ends_left(&pool).await, 1, "a refused turn end is kept");
    runner
        .backend()
        .refuse_turn_ends
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        runner.deliver_turn_ends().await,
        0,
        "not due until its backoff passes"
    );

    make_turn_ends_due(&pool).await;
    assert_eq!(runner.deliver_turn_ends().await, 1);
    assert_eq!(turn_ends_left(&pool).await, 0);
    assert_eq!(runner.backend().turn_ends.lock().unwrap().len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_turn_end_refused_too_often_is_given_up(pool: PgPool) {
    let store = Store::new(pool.clone());
    let runner = remembering_runner(&pool, vec![reply_and_end("one")]);
    store
        .accept_event("assistant", "s3", &event("e1", "hi"), false)
        .await
        .unwrap();
    runner.wake("assistant", "s3");
    runner.idle().await;
    sqlx::query("update turn_ends set attempts = 30")
        .execute(&pool)
        .await
        .unwrap();
    runner
        .backend()
        .refuse_turn_ends
        .store(false, std::sync::atomic::Ordering::SeqCst);

    assert_eq!(runner.deliver_turn_ends().await, 0);

    assert_eq!(turn_ends_left(&pool).await, 0);
    assert!(
        runner.backend().turn_ends.lock().unwrap().is_empty(),
        "an abandoned turn end is not sent"
    );
}

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl newt_core::sink::EventSink for Recorder {
    fn emit(&self, event: newt_core::event::Event) {
        let kind = serde_json::to_value(&event.kind).unwrap();
        self.0
            .lock()
            .unwrap()
            .push(format!("turn {}", kind["type"].as_str().unwrap()));
    }
}

impl newt_server::events::Observer for Recorder {
    fn server(&self, event: &newt_server::events::ServerEvent) {
        let kind = serde_json::to_value(&event.kind).unwrap();
        self.0
            .lock()
            .unwrap()
            .push(format!("server {}", kind["type"].as_str().unwrap()));
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_observer_sees_a_turns_events_and_its_commit(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t9", &event("e1", "one"), false)
        .await
        .unwrap();
    let first = Arc::new(Recorder::default());
    let second = Arc::new(Recorder::default());
    let runner = Runner::builder(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        ScriptedModel {
            script: Mutex::new(vec![reply_and_end("a")].into()),
            steal_lease: None,
            delay: Duration::ZERO,
            started: Arc::new(tokio::sync::Notify::new()),
        },
    )
    .config(fast())
    .observe(Arc::clone(&first))
    .observe(Arc::clone(&second))
    .build();
    runner.wake("assistant", "t9");
    wait_for(&store, "t9", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;
    let seen = first.0.lock().unwrap().clone();
    for expected in [
        "server session_claimed",
        "turn turn_started",
        "turn turn_completed",
        "server turn_committed",
    ] {
        assert!(
            seen.iter().any(|kind| kind == expected),
            "{expected} missing from {seen:?}"
        );
    }
    assert_eq!(seen, *second.0.lock().unwrap());
}

struct GiveUpAtOnce;

impl FailurePolicy for GiveUpAtOnce {
    fn on_failure(&self, failure: &FailedTurn<'_>) -> AfterFailure {
        AfterFailure::DeadLetter {
            reason: format!("given up on {} after {}", failure.session, failure.attempts),
        }
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_failure_policy_decides_when_a_turn_is_dead_lettered(pool: PgPool) {
    let store = Store::new(pool.clone());
    store
        .accept_event("assistant", "t10", &event("e1", "hi"), false)
        .await
        .unwrap();
    let runner = Runner::builder(
        Store::new(pool.clone()),
        StaticBackend {
            system: "You are a helpful agent.".into(),
            model: "test".into(),
            workspace: Workspace::default(),
        },
        ScriptedModel {
            script: Mutex::new(VecDeque::new()),
            steal_lease: None,
            delay: Duration::ZERO,
            started: Arc::new(tokio::sync::Notify::new()),
        },
    )
    .config(fast())
    .failures(GiveUpAtOnce)
    .build();
    runner.wake("assistant", "t10");
    wait_for(&store, "t10", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;
    let (reason,): (Option<String>,) =
        sqlx::query_as("select dead_letter from inbox where key = 't10'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("given up on t10 after 1"));
}
