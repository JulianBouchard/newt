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
use newt_server::live::spawn_listener;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, SessionStatus, Store};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct ScriptedModel {
    script: Mutex<VecDeque<Message>>,
    steal_lease: Option<PgPool>,
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
        lease_ttl: Duration::from_secs(5),
        sweep_every: Duration::from_millis(50),
        shutdown_grace: Duration::from_secs(1),
    }
}

fn event(id: &str, text: &str) -> InboundEvent {
    serde_json::from_value(json!({ "id": id, "text": text })).unwrap()
}

fn runner(
    pool: &PgPool,
    script: Vec<Message>,
    steal: bool,
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
        },
        fast(),
        None,
    )
}

async fn wait_for(
    store: &Store,
    key: &str,
    done: impl Fn(&SessionStatus) -> bool,
) -> SessionStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = store.session_status("assistant", key).await.unwrap()
            && done(&status)
        {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "session {key} never reached the expected state"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_posted_event_runs_a_turn_and_commits_it(pool: PgPool) {
    let (live, _listener) = spawn_listener(pool.clone());
    let runner = runner(&pool, vec![reply_and_end("Hello!")], false);
    let _loops = runner.spawn_loops(live.subscribe());
    let state = AppState {
        store: Store::new(pool.clone()),
        tokens: Arc::new(HashMap::from([(
            "assistant".to_owned(),
            "secret".to_owned(),
        )])),
        live,
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
    let runner = runner(&pool, vec![reply_and_end("a"), reply_and_end("b")], false);

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
    let runner = runner(&pool, Vec::new(), false);

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
        "select failed_attempts, retry_after > now() from sessions where key = 't3'",
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
    let runner = runner(&pool, vec![reply_and_end("too late")], true);

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
    let runner = runner(&pool, vec![reply_and_end("recovered")], false);
    let (_never, quiet) = broadcast::channel(1);

    let _loops = runner.spawn_loops(quiet);
    let status = wait_for(&store, "t5", |s| {
        s.pending_events == 0 && s.lease_owner.is_none()
    })
    .await;

    assert_eq!(status.history_entries, 2);
    assert_eq!(status.lease_epoch, 8);
}
