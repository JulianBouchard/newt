#![expect(
    clippy::unwrap_used,
    reason = "tests fail loudly, and simulate crashes by panicking"
)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{ModelRequest, ToolSpec};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::tools::{Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::workspace::Workspace;
use newt_server::backend::{Backend, BackendError, Prepared};
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, Store};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crash {
    BeforeEffect,
    AfterEffect,
}

#[derive(Default)]
struct World {
    effects: Mutex<HashMap<String, usize>>,
    keys: Mutex<Vec<String>>,
    crash: Mutex<Option<Crash>>,
}

impl World {
    fn send(&self, key: &str) -> ToolOutput {
        self.keys.lock().unwrap().push(key.to_owned());
        if self.effects.lock().unwrap().contains_key(key) {
            return ToolOutput {
                content: "already sent".into(),
                is_error: false,
                summary: Some("Sent".into()),
                ..ToolOutput::default()
            };
        }
        let crash = self.crash.lock().unwrap().take();
        assert!(
            crash != Some(Crash::BeforeEffect),
            "simulated crash before the send"
        );
        *self
            .effects
            .lock()
            .unwrap()
            .entry(key.to_owned())
            .or_default() += 1;
        assert!(
            crash != Some(Crash::AfterEffect),
            "simulated crash after the send"
        );
        ToolOutput {
            content: "sent".into(),
            is_error: false,
            summary: Some("Sent".into()),
            ..ToolOutput::default()
        }
    }
}

struct Host(Arc<World>);

impl ToolHost for Host {
    fn call(&self, _call: &ToolCall, key: &str) -> impl Future<Output = ToolOutput> + Send {
        std::future::ready(self.0.send(key))
    }
}

struct WorldBackend(Arc<World>);

impl Backend for WorldBackend {
    type Host = Host;

    fn prepare(
        &self,
        _agent: &str,
        _key: &str,
    ) -> impl Future<Output = Result<Prepared<Host>, BackendError>> + Send {
        std::future::ready(Ok(Prepared {
            system: "You are a helpful agent.".into(),
            tools: vec![ToolDef {
                spec: ToolSpec {
                    name: "send_message".into(),
                    description: "Send a message.".into(),
                    input_schema: json!({ "type": "object" }),
                },
                retention: Retention::Outward,
                invalidates: Vec::new(),
            }],
            context: None,
            workspace: Workspace::default(),
            model: "test".into(),
            first_call_effort: None,
            effort: None,
            max_output_tokens: 1024,
            max_model_calls: 6,
            host: Host(Arc::clone(&self.0)),
        }))
    }
}

struct Scripted {
    script: Mutex<VecDeque<Message>>,
    calls: Arc<AtomicUsize>,
}

impl Model for Scripted {
    fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let next = self.script.lock().unwrap().pop_front();
        std::future::ready(
            next.map(|message| ModelResponse {
                message,
                usage: Usage::default(),
                stop: StopReason::ToolUse,
                timing: CallTiming::default(),
                response_id: None,
                request_ids: BTreeMap::new(),
            })
            .ok_or_else(|| ProviderError::Protocol("script exhausted".into())),
        )
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        })],
    }
}

fn runner(
    pool: &PgPool,
    world: &Arc<World>,
    script: Vec<Message>,
    calls: &Arc<AtomicUsize>,
) -> Runner<WorldBackend, Scripted> {
    Runner::new(
        Store::new(pool.clone()),
        WorldBackend(Arc::clone(world)),
        Scripted {
            script: Mutex::new(script.into()),
            calls: Arc::clone(calls),
        },
        RunnerConfig {
            lease_ttl: Duration::from_secs(1),
            sweep_every: Duration::from_millis(50),
            shutdown_grace: Duration::from_secs(1),
            backoff_base: Duration::from_millis(10),
            max_attempts: 5,
            max_sessions: 8,
        },
        None,
        broadcast::channel(16).0,
    )
}

async fn until(what: &str, check: impl AsyncFn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn crash_then_recover(pool: PgPool, crash: Crash) {
    let store = Store::new(pool.clone());
    let event: InboundEvent =
        serde_json::from_value(json!({ "id": "e1", "text": "Tell them yes." })).unwrap();
    store.accept_event("assistant", "k", &event).await.unwrap();
    let world = Arc::new(World::default());
    *world.crash.lock().unwrap() = Some(crash);
    let calls_a = Arc::new(AtomicUsize::new(0));
    let calls_b = Arc::new(AtomicUsize::new(0));

    let crashing = runner(
        &pool,
        &world,
        vec![
            tool_call("s1", "send_message", json!({ "text": "Yes!" })),
            tool_call("e1", "end_turn", json!({ "nothing_new": "sent" })),
        ],
        &calls_a,
    );
    crashing.wake("assistant", "k");
    until("the crash", || async {
        world.crash.lock().unwrap().is_none()
    })
    .await;

    let recovering = runner(
        &pool,
        &world,
        vec![tool_call(
            "e2",
            "end_turn",
            json!({ "nothing_new": "sent" }),
        )],
        &calls_b,
    );
    let _loops = recovering.spawn_loops();
    until("the recovered commit", || async {
        let status = store
            .session_status("assistant", "k")
            .await
            .unwrap()
            .unwrap();
        status.pending_events == 0 && status.lease_owner.is_none()
    })
    .await;

    let effects: usize = world.effects.lock().unwrap().values().sum();
    assert_eq!(effects, 1, "the message must be sent exactly once");
    let keys = world.keys.lock().unwrap().clone();
    assert_eq!(keys.len(), 2);
    assert_eq!(
        keys[0], keys[1],
        "the resend must reuse the idempotency key"
    );
    assert_eq!(calls_a.load(Ordering::Relaxed), 1);
    assert_eq!(
        calls_b.load(Ordering::Relaxed),
        1,
        "the recorded model call must be replayed, not re-paid"
    );
    let status = store
        .session_status("assistant", "k")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.history_entries, 3);
    let journal_rows: i64 = sqlx::query_scalar("select count(*) from turn_journal")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(journal_rows, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_crash_before_the_send_resends_once_with_the_same_key(pool: PgPool) {
    crash_then_recover(pool, Crash::BeforeEffect).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_crash_after_the_send_never_sends_twice(pool: PgPool) {
    crash_then_recover(pool, Crash::AfterEffect).await;
}
