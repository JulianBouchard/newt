#![expect(
    clippy::unwrap_used,
    reason = "tests fail loudly, and simulate crashes by panicking"
)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use newt_core::journal::TurnChoice;
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Effort, ModelRequest, ToolSpec};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::tools::{CallContext, Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::workspace::Workspace;
use newt_server::backend::{Backend, BackendError, Prepared, SessionState, TurnRequest, TurnState};
use newt_server::fork::{Fork, fork, history};
use newt_server::live::Topics;
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::sessions::{Lease, TurnCommit};
use newt_server::store::{InboundEvent, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

fn strong() -> TurnChoice {
    TurnChoice {
        model: "strong".into(),
        first_call_effort: Some(Effort::High),
        effort: Some(Effort::High),
        reset: false,
    }
}

fn reset() -> TurnChoice {
    TurnChoice {
        model: "base".into(),
        first_call_effort: None,
        effort: None,
        reset: true,
    }
}

#[derive(Clone, Default)]
struct Provider {
    choices: Arc<Mutex<VecDeque<Option<TurnChoice>>>>,
    crash: Arc<AtomicBool>,
    sends: Arc<Mutex<usize>>,
}

struct Host(Provider);

impl ToolHost for Host {
    fn call(
        &self,
        _call: &ToolCall,
        _context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        assert!(
            !self.0.crash.swap(false, Ordering::SeqCst),
            "simulated crash before the send"
        );
        *self.0.sends.lock().unwrap() += 1;
        std::future::ready(ToolOutput {
            content: "sent".into(),
            is_error: false,
            summary: Some("Sent".into()),
            ..ToolOutput::default()
        })
    }
}

impl Backend for Provider {
    type Host = Host;

    fn prepare(
        &self,
        _agent: &str,
        _key: &str,
        request: &TurnRequest,
    ) -> impl Future<Output = Result<Prepared<Host>, BackendError>> + Send {
        let choice = self.choices.lock().unwrap().pop_front().flatten();
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
                ends_turn: false,
            }],
            context: None,
            skills: Vec::new(),
            workspace: Workspace::default(),
            model: "base".into(),
            first_call_effort: None,
            effort: None,
            max_output_tokens: 1024,
            max_model_calls: 6,
            max_turn_tokens: None,
            recite: false,
            require_end_turn: true,
            history_budget: 60_000,
            state: SessionState::starting_from(request.state.clone()),
            choice,
            turn_end: false,
            turn_state: TurnState::default(),
            host: Host(self.clone()),
        }))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Call {
    model: String,
    effort: Option<Effort>,
    history: Vec<String>,
    opens_turn: bool,
}

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<Call>>,
    fail_next: AtomicBool,
}

fn text_of(message: &Message) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_call(id: &str, name: &str, arguments: Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    })
}

impl Model for Recorder {
    fn stream(
        &self,
        request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        let scratch = request.prompt.scratch;
        let first_call = scratch.len() == 1;
        self.calls.lock().unwrap().push(Call {
            model: request.model.to_owned(),
            effort: request.effort,
            history: request.prompt.history.iter().map(text_of).collect(),
            opens_turn: first_call,
        });
        let crash = scratch
            .first()
            .is_some_and(|first| text_of(first).starts_with("crash"));
        let parts = match (first_call, crash) {
            (true, _) if self.fail_next.swap(false, Ordering::SeqCst) => {
                return std::future::ready(Err(ProviderError::Protocol("boom".into())));
            }
            (true, true) => vec![tool_call("s1", "send_message", json!({ "text": "hi" }))],
            (true, false) => vec![Part::text("On it.")],
            (false, _) => vec![tool_call("e1", "end_turn", json!({ "nothing_new": "ok" }))],
        };
        std::future::ready(Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                parts,
            },
            usage: Usage::default(),
            stop: StopReason::ToolUse,
            timing: CallTiming::default(),
            response_id: None,
            request_ids: BTreeMap::new(),
        }))
    }
}

fn runner(pool: &PgPool, provider: &Provider) -> Runner<Provider, Recorder> {
    Runner::new(
        Store::new(pool.clone()),
        provider.clone(),
        Recorder::default(),
        RunnerConfig {
            lease_ttl: Duration::from_secs(1),
            sweep_every: Duration::from_millis(50),
            shutdown_grace: Duration::from_secs(1),
            backoff_base: Duration::from_millis(10),
            ..RunnerConfig::default()
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

async fn is_idle(store: &Store) -> bool {
    let status = store.session_status("assistant", "k").await.unwrap();
    status.is_some_and(|status| status.pending_events == 0 && status.lease_owner.is_none())
}

async fn send(store: &Store, id: &str, text: &str) {
    let event: InboundEvent = serde_json::from_value(json!({ "id": id, "text": text })).unwrap();
    store
        .accept_event("assistant", "k", &event, false)
        .await
        .unwrap();
}

async fn say(
    store: &Store,
    runner: &Runner<Provider, Recorder>,
    provider: &Provider,
    text: &str,
    choice: Option<TurnChoice>,
) {
    provider.choices.lock().unwrap().push_back(choice);
    send(store, text, text).await;
    runner.wake("assistant", "k");
    until("the turn", || async { is_idle(store).await }).await;
}

fn first_calls(runner: &Runner<Provider, Recorder>) -> Vec<Call> {
    runner
        .model()
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.opens_turn)
        .cloned()
        .collect()
}

async fn checkpoint_count(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar("select count(*) from checkpoints where key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_turns_model_and_efforts_apply_to_every_call_of_that_turn_only(pool: PgPool) {
    let store = Store::new(pool.clone());
    let provider = Provider::default();
    let runner = runner(&pool, &provider);
    say(&store, &runner, &provider, "one", Some(strong())).await;
    say(&store, &runner, &provider, "two", None).await;

    let calls = runner.model().calls.lock().unwrap().clone();
    let used: Vec<(String, Option<Effort>)> = calls
        .iter()
        .map(|call| (call.model.clone(), call.effort))
        .collect();
    assert_eq!(
        used,
        vec![
            ("strong".into(), Some(Effort::High)),
            ("strong".into(), Some(Effort::High)),
            ("base".into(), None),
            ("base".into(), None),
        ]
    );

    let frames: Vec<Value> = sqlx::query_scalar(
        "select event from live_events where event->>'type' = 'turn_started' order by seq",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let shown: Vec<(&str, bool)> = frames
        .iter()
        .map(|frame| {
            (
                frame["model"].as_str().unwrap(),
                frame["reset"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(shown, vec![("strong", false), ("base", false)]);
    let journal_rows: i64 = sqlx::query_scalar("select count(*) from turn_journal")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(journal_rows, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_reset_hides_earlier_history_from_the_model_but_keeps_it_stored(pool: PgPool) {
    let store = Store::new(pool.clone());
    let provider = Provider::default();
    let runner = runner(&pool, &provider);
    say(&store, &runner, &provider, "one", None).await;
    say(&store, &runner, &provider, "two", None).await;
    say(&store, &runner, &provider, "three", Some(reset())).await;
    say(&store, &runner, &provider, "four", None).await;

    let calls = first_calls(&runner);
    assert!(calls[1].history.contains(&"one".to_owned()));
    assert!(
        calls[2].history.is_empty(),
        "the reset turn sees no history"
    );
    assert_eq!(calls[3].history[0], "three");
    assert!(
        !calls[3]
            .history
            .iter()
            .any(|text| text == "one" || text == "two"),
        "{:?}",
        calls[3].history
    );

    let entries = history(&pool, "assistant", "k").await.unwrap().unwrap();
    let texts: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry.entry["parts"][0]["text"].as_str().map(str::to_owned))
        .collect();
    for text in ["one", "two", "three", "four"] {
        assert!(texts.contains(&text.to_owned()), "{text} stays stored");
    }

    let frames: Vec<Value> = sqlx::query_scalar(
        "select event from live_events where event->>'type' = 'turn_started' order by seq",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let flags: Vec<bool> = frames
        .iter()
        .map(|frame| frame["reset"].as_bool().unwrap())
        .collect();
    assert_eq!(flags, vec![false, false, true, false]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_reset_turn_that_does_not_commit_resets_nothing(pool: PgPool) {
    let store = Store::new(pool.clone());
    let provider = Provider::default();
    let runner = runner(&pool, &provider);
    say(&store, &runner, &provider, "one", None).await;

    let _loops = runner.spawn_loops();
    runner.model().fail_next.store(true, Ordering::SeqCst);
    provider.choices.lock().unwrap().push_back(Some(reset()));
    provider.choices.lock().unwrap().push_back(None);
    send(&store, "two", "two").await;
    runner.wake("assistant", "k");
    until("the retried turn", || async { is_idle(&store).await }).await;

    let calls = first_calls(&runner);
    assert!(
        calls[1].history.is_empty(),
        "the failed attempt asked for a reset"
    );
    assert!(
        calls[2].history.contains(&"one".to_owned()),
        "the retry saw the whole history: the failed attempt reset nothing"
    );
    assert_eq!(checkpoint_count(&pool, "k").await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_resumed_turn_keeps_the_model_and_reset_it_started_with(pool: PgPool) {
    let store = Store::new(pool.clone());
    let provider = Provider::default();
    let first = runner(&pool, &provider);
    say(&store, &first, &provider, "one", None).await;

    provider.crash.store(true, Ordering::SeqCst);
    let mut chosen = reset();
    chosen.model = "strong".into();
    provider.choices.lock().unwrap().push_back(Some(chosen));
    provider.choices.lock().unwrap().push_back(None);
    send(&store, "crash now", "crash now").await;
    first.wake("assistant", "k");
    until("the crash", || async {
        !provider.crash.load(Ordering::SeqCst)
    })
    .await;

    let second = runner(&pool, &provider);
    let _loops = second.spawn_loops();
    until("the recovered commit", || async { is_idle(&store).await }).await;

    let resumed = second.model().calls.lock().unwrap().clone();
    assert_eq!(
        resumed.len(),
        1,
        "the first call is replayed from the journal"
    );
    assert_eq!(resumed[0].model, "strong");
    assert!(
        resumed[0].history.is_empty(),
        "the resumed turn keeps its reset: {:?}",
        resumed[0].history
    );
    assert_eq!(checkpoint_count(&pool, "k").await, 1);
    assert_eq!(*provider.sends.lock().unwrap(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn compaction_after_a_reset_only_sees_what_came_after_it(pool: PgPool) {
    let store = Store::new(pool.clone());
    let provider = Provider::default();
    let runner = runner(&pool, &provider);
    say(&store, &runner, &provider, "one", None).await;
    say(&store, &runner, &provider, "two", None).await;
    sqlx::query(
        "insert into checkpoints (agent, key, through_seq, snapshot, summary) \
         values ('assistant', 'k', 1, '{\"commitments\": [\"call the bank\"]}', 'old summary')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let lease = Lease {
        agent: "assistant".into(),
        key: "k".into(),
        owner: "test".into(),
        epoch: 0,
    };
    let before = store.compaction_state(&lease).await.unwrap();
    assert_eq!(before.open_commitments, vec!["call the bank"]);

    say(&store, &runner, &provider, "three", Some(reset())).await;
    say(&store, &runner, &provider, "four", None).await;

    let calls = first_calls(&runner);
    assert_eq!(calls[2].history, Vec::<String>::new());
    assert!(
        !calls[3]
            .history
            .iter()
            .any(|text| text.contains("old summary")),
        "{:?}",
        calls[3].history
    );

    let after = store.compaction_state(&lease).await.unwrap();
    assert_eq!(after.checkpoint_tokens, 0);
    assert_eq!(after.open_commitments, Vec::<String>::new());
    let turns: std::collections::BTreeSet<String> =
        after.rows.iter().map(|row| row.turn_id.clone()).collect();
    assert_eq!(turns.len(), 2, "only the reset turn and the one after it");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_fork_keeps_a_reset_only_when_it_includes_the_reset_turn(pool: PgPool) {
    let store = Store::new(pool.clone());
    let provider = Provider::default();
    let runner = runner(&pool, &provider);
    say(&store, &runner, &provider, "one", None).await;
    say(&store, &runner, &provider, "two", Some(reset())).await;

    let entries = history(&pool, "assistant", "k").await.unwrap().unwrap();
    let first_turn = entries[0].turn.clone();
    let second_turn = entries.last().unwrap().turn.clone();
    for (into, through) in [("before", &first_turn), ("through", &second_turn)] {
        fork(
            &pool,
            Fork {
                agent: "assistant",
                key: "k",
                into_agent: "assistant",
                into_key: into,
                through_turn: Some(through),
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(checkpoint_count(&pool, "before").await, 0);
    assert_eq!(checkpoint_count(&pool, "through").await, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_resume_without_a_journaled_choice_ignores_what_turn_answers_now(pool: PgPool) {
    let store = Store::new(pool.clone());
    let provider = Provider::default();
    let first = runner(&pool, &provider);
    say(&store, &first, &provider, "one", None).await;

    provider.crash.store(true, Ordering::SeqCst);
    provider.choices.lock().unwrap().push_back(None);
    let mut asked = reset();
    asked.model = "strong".into();
    provider.choices.lock().unwrap().push_back(Some(asked));
    send(&store, "crash now", "crash now").await;
    first.wake("assistant", "k");
    until("the crash", || async {
        !provider.crash.load(Ordering::SeqCst)
    })
    .await;

    let second = runner(&pool, &provider);
    let _loops = second.spawn_loops();
    until("the recovered commit", || async { is_idle(&store).await }).await;

    let resumed = second.model().calls.lock().unwrap().clone();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].model, "base", "no mixing models inside a turn");
    assert!(
        resumed[0].history.contains(&"one".to_owned()),
        "the earlier calls saw the history, so the turn keeps it: {:?}",
        resumed[0].history
    );
    assert_eq!(checkpoint_count(&pool, "k").await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_commit_without_history_rows_does_not_reset(pool: PgPool) {
    let store = Store::new(pool.clone());
    send(&store, "e1", "hello").await;
    let (lease, _) = store
        .claim("assistant", "k", "owner", Duration::from_secs(30))
        .await
        .unwrap();
    let lease = lease.unwrap();
    store
        .commit_turn(
            &lease,
            TurnCommit {
                turn_id: "t1".into(),
                reset: true,
                ..TurnCommit::default()
            },
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!(
        checkpoint_count(&pool, "k").await,
        0,
        "no history rows, so nothing to start over from"
    );
}
