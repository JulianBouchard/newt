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
use newt_core::tools::{CallContext, Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::workspace::Workspace;
use newt_server::backend::{
    AgentSettings, Backend, BackendError, Prepared, SessionState, TurnRequest, TurnState,
};
use newt_server::live::Topics;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, Store};
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crash {
    BeforeEffect,
    AfterEffect,
}

/// The tool the turn calls: an outward send, or a durable write whose handler
/// does or doesn't dedupe on the idempotency key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Tool {
    #[default]
    Send,
    Write {
        idempotent: bool,
    },
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Send => "send_message",
            Self::Write { .. } => "save_note",
        }
    }

    fn dedupes(self) -> bool {
        self != Self::Write { idempotent: false }
    }
}

#[derive(Default)]
struct World {
    tool: Tool,
    effects: Mutex<HashMap<String, usize>>,
    keys: Mutex<Vec<String>>,
    crash: Mutex<Option<Crash>>,
}

impl World {
    fn send(&self, key: &str) -> ToolOutput {
        self.keys.lock().unwrap().push(key.to_owned());
        if self.tool.dedupes() && self.effects.lock().unwrap().contains_key(key) {
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
    fn call(
        &self,
        _call: &ToolCall,
        context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        std::future::ready(self.0.send(&context.idempotency_key))
    }
}

struct WorldBackend(Arc<World>);

impl Backend for WorldBackend {
    type Host = Host;

    fn prepare(
        &self,
        _agent: &str,
        _key: &str,
        _request: &TurnRequest,
    ) -> impl Future<Output = Result<Prepared<Host>, BackendError>> + Send {
        std::future::ready(Ok(Prepared {
            system: "You are a helpful agent.".into(),
            tools: vec![ToolDef {
                spec: ToolSpec {
                    name: self.0.tool.name().into(),
                    description: "Do it.".into(),
                    input_schema: json!({ "type": "object" }),
                },
                retention: match self.0.tool {
                    Tool::Send => Retention::Outward,
                    Tool::Write { .. } => Retention::Durable,
                },
                invalidates: Vec::new(),
                ends_turn: false,
                idempotent: self.0.tool.dedupes(),
            }],
            context: None,
            skills: Vec::new(),
            workspace: Workspace::default(),
            settings: AgentSettings {
                model: "test".into(),
                first_call_effort: None,
                effort: None,
                max_output_tokens: 1024,
                max_model_calls: 6,
                max_turn_tokens: None,
                recite: false,
                require_end_turn: true,
                builtin_tools: newt_core::tools::BuiltinTools::ALL,
                page_index_wait: std::time::Duration::from_secs(2),
                history_budget: 60_000,
            },
            state: SessionState::default(),
            choice: None,
            turn_end: false,
            turn_state: TurnState::default(),
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
            reserved_interactive: 2,
            max_waiting: 64,
            cache_key_buckets: newt_server::cache_keys::Buckets::Fixed(1),
            retention: newt_server::retention::Retention::default(),
        },
        None,
        Topics::default(),
    )
}

async fn until(what: &str, check: impl AsyncFn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Recovered {
    effects: usize,
    keys: Vec<String>,
    model_calls_before: usize,
    model_calls_after: usize,
    history_entries: i64,
    journal_rows: i64,
    results: Vec<String>,
}

/// Runs one turn that calls `tool` once, crashes it at `crash`, and lets a
/// second runner recover the session.
async fn crash_then_recover(pool: PgPool, tool: Tool, crash: Crash) -> Recovered {
    let store = Store::new(pool.clone());
    let event: InboundEvent =
        serde_json::from_value(json!({ "id": "e1", "text": "Tell them yes." })).unwrap();
    store
        .accept_event("assistant", "k", &event, false)
        .await
        .unwrap();
    let world = Arc::new(World {
        tool,
        ..World::default()
    });
    *world.crash.lock().unwrap() = Some(crash);
    let calls_a = Arc::new(AtomicUsize::new(0));
    let calls_b = Arc::new(AtomicUsize::new(0));

    let crashing = runner(
        &pool,
        &world,
        vec![
            tool_call("s1", tool.name(), json!({ "text": "Yes!" })),
            tool_call("e1", "end_turn", json!({ "nothing_new": "done" })),
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
            json!({ "nothing_new": "done" }),
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

    let status = store
        .session_status("assistant", "k")
        .await
        .unwrap()
        .unwrap();
    let journal_rows: i64 = sqlx::query_scalar("select count(*) from turn_journal")
        .fetch_one(&pool)
        .await
        .unwrap();
    let results: Vec<String> = sqlx::query_scalar(
        "select part->>'content' from history, jsonb_array_elements(entry->'parts') part \
         where part->>'type' = 'tool_result' order by seq",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let effects = world.effects.lock().unwrap().values().sum();
    let keys = world.keys.lock().unwrap().clone();
    Recovered {
        effects,
        keys,
        model_calls_before: calls_a.load(Ordering::Relaxed),
        model_calls_after: calls_b.load(Ordering::Relaxed),
        history_entries: status.history_entries,
        journal_rows,
        results,
    }
}

fn assert_resent_once(recovered: &Recovered) {
    assert_eq!(recovered.effects, 1, "the effect must happen exactly once");
    assert_eq!(recovered.keys.len(), 2);
    assert_eq!(
        recovered.keys[0], recovered.keys[1],
        "the retry must reuse the idempotency key"
    );
    assert_eq!(recovered.model_calls_before, 1);
    assert_eq!(
        recovered.model_calls_after, 1,
        "the recorded model call must be replayed, not re-paid"
    );
    assert_eq!(recovered.history_entries, 3);
    assert_eq!(recovered.journal_rows, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_crash_before_the_send_resends_once_with_the_same_key(pool: PgPool) {
    assert_resent_once(&crash_then_recover(pool, Tool::Send, Crash::BeforeEffect).await);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_crash_after_the_send_never_sends_twice(pool: PgPool) {
    assert_resent_once(&crash_then_recover(pool, Tool::Send, Crash::AfterEffect).await);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_crash_after_an_idempotent_write_retries_it_with_the_same_key(pool: PgPool) {
    let tool = Tool::Write { idempotent: true };
    assert_resent_once(&crash_then_recover(pool, tool, Crash::AfterEffect).await);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_crash_after_a_write_reports_it_interrupted_instead_of_writing_twice(pool: PgPool) {
    let tool = Tool::Write { idempotent: false };
    let recovered = crash_then_recover(pool, tool, Crash::AfterEffect).await;

    assert_eq!(recovered.effects, 1, "the write must not run a second time");
    assert_eq!(recovered.keys.len(), 1);
    assert_eq!(recovered.model_calls_after, 1);
    assert!(
        recovered.results[0].starts_with("Interrupted:"),
        "the model must learn the write may have happened: {:?}",
        recovered.results
    );
    assert_eq!(recovered.journal_rows, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_crash_while_a_write_starts_reports_it_interrupted_too(pool: PgPool) {
    let tool = Tool::Write { idempotent: false };
    let recovered = crash_then_recover(pool, tool, Crash::BeforeEffect).await;

    assert_eq!(recovered.effects, 0);
    assert_eq!(recovered.keys.len(), 1);
    assert!(recovered.results[0].starts_with("Interrupted:"));
}
