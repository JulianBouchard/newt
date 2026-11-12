#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::ModelRequest;
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::tools::NoTools;
use newt_core::workspace::Workspace;
use newt_server::backend::{Backend, BackendError, Prepared};
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::sessions::Lease;
use newt_server::store::{InboundEvent, Store};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const BUDGET: u32 = 2_000;
const COMMITMENT: &str = "Send the Acme brief by Friday";

struct Budgeted;

impl Backend for Budgeted {
    type Host = NoTools;

    fn prepare(
        &self,
        _agent: &str,
        _key: &str,
    ) -> impl Future<Output = Result<Prepared<NoTools>, BackendError>> + Send {
        std::future::ready(Ok(Prepared {
            system: "You are a helpful agent.".into(),
            tools: Vec::new(),
            context: None,
            workspace: Workspace::default(),
            model: "test".into(),
            first_call_effort: None,
            effort: None,
            max_output_tokens: 1024,
            max_model_calls: 4,
            history_budget: BUDGET,
            host: NoTools,
        }))
    }
}

#[derive(Default)]
struct Chatty {
    fail_compaction: bool,
    largest_history: Mutex<usize>,
    compactions: Mutex<usize>,
    refusals: Mutex<usize>,
    histories_starting_with_checkpoint: Mutex<usize>,
    split_turns: Mutex<usize>,
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

fn results(scratch: &[Message]) -> Vec<String> {
    scratch
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .collect()
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    })
}

fn checkpoint(commitments: &[&str]) -> Vec<Part> {
    vec![call(
        "c",
        "checkpoint",
        json!({
            "memory": ["Prefers remote roles"],
            "open_threads": [],
            "commitments": commitments,
            "decisions": [],
            "tone": "short",
        }),
    )]
}

impl Chatty {
    fn compact(&self, request: &ModelRequest<'_>) -> Vec<Part> {
        let scratch = request.prompt.scratch;
        let seen = results(scratch);
        if scratch.len() == 1 {
            *self.compactions.lock().unwrap() += 1;
        }
        if seen.is_empty() {
            if self.fail_compaction {
                return vec![Part::text("Not now.")];
            }
            let compactions = self.compactions.lock().unwrap();
            return vec![call(
                "m",
                "remember",
                json!({ "note": format!("Flushed fact {}", *compactions) }),
            )];
        }
        if self.fail_compaction {
            return vec![Part::text("Still not now.")];
        }
        let prior = request
            .prompt
            .history
            .first()
            .is_some_and(|first| text_of(first).starts_with("<checkpoint>"));
        let refused = seen
            .iter()
            .any(|result| result.contains("Checkpoint refused"));
        if refused {
            *self.refusals.lock().unwrap() += 1;
        }
        if prior && !refused {
            return checkpoint(&[]);
        }
        checkpoint(&[COMMITMENT])
    }

    fn answer(&self, request: &ModelRequest<'_>) -> Vec<Part> {
        let history = request.prompt.history;
        let bytes: usize = history
            .iter()
            .map(|message| serde_json::to_string(message).unwrap().len())
            .sum();
        let mut largest = self.largest_history.lock().unwrap();
        *largest = (*largest).max(bytes);
        if history
            .first()
            .is_some_and(|first| text_of(first).starts_with("<checkpoint>"))
        {
            *self.histories_starting_with_checkpoint.lock().unwrap() += 1;
            let next = history.get(1);
            let starts_a_turn = next.is_none_or(|message| {
                message.role == Role::User && text_of(message).starts_with("Message ")
                    || message
                        .parts
                        .iter()
                        .any(|part| matches!(part, Part::ToolCall(call) if call.name == "remember"))
            });
            if !starts_a_turn {
                *self.split_turns.lock().unwrap() += 1;
            }
        }
        vec![
            Part::text("Here is a long and careful answer. ".repeat(20)),
            call("e", "end_turn", json!({ "nothing_new": "small talk" })),
        ]
    }
}

impl Model for Chatty {
    fn stream(
        &self,
        request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        let compacting = request
            .prompt
            .scratch
            .first()
            .is_some_and(|first| text_of(first).starts_with("Newt is compacting"));
        let parts = if compacting {
            self.compact(request)
        } else {
            self.answer(request)
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

async fn until_idle(store: &Store) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = store
            .session_status("assistant", "k")
            .await
            .unwrap()
            .unwrap();
        if status.pending_events == 0 && status.lease_owner.is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "session never went idle: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn runner(pool: &PgPool, model: Chatty) -> Runner<Budgeted, Chatty> {
    Runner::new(
        Store::new(pool.clone()),
        Budgeted,
        model,
        RunnerConfig {
            sweep_every: Duration::from_millis(50),
            ..RunnerConfig::default()
        },
        None,
        broadcast::channel(16).0,
    )
}

async fn converse(store: &Store, runner: &Runner<Budgeted, Chatty>, turns: usize) {
    for turn in 0..turns {
        let event: InboundEvent = serde_json::from_value(json!({
            "id": format!("e{turn}"),
            "text": format!("Message {turn}: {}", "tell me more about remote roles. ".repeat(10)),
        }))
        .unwrap();
        store.accept_event("assistant", "k", &event).await.unwrap();
        runner.wake("assistant", "k");
        until_idle(store).await;
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_long_session_stays_under_its_history_budget(pool: PgPool) {
    let store = Store::new(pool.clone());
    let runner = runner(&pool, Chatty::default());
    converse(&store, &runner, 30).await;

    let checkpoints: i64 = sqlx::query_scalar("select count(*) from checkpoints")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        checkpoints >= 3,
        "expected several compactions, saw {checkpoints}"
    );
    let events: i64 = sqlx::query_scalar(
        "select count(*) from history where entry->'parts'->0->>'text' like 'Message %'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 30, "raw history is never deleted");

    let model = runner.model();
    let largest = *model.largest_history.lock().unwrap();
    assert!(
        largest / 4 <= usize::try_from(BUDGET).unwrap() + 400,
        "the prompt history grew to about {} tokens",
        largest / 4
    );
    assert!(*model.histories_starting_with_checkpoint.lock().unwrap() > 0);
    assert_eq!(
        *model.split_turns.lock().unwrap(),
        0,
        "the cut never splits a turn"
    );
    assert!(
        *model.refusals.lock().unwrap() > 0,
        "a checkpoint dropping the open commitment must be refused"
    );

    let summary: String =
        sqlx::query_scalar("select summary from checkpoints order by through_seq desc limit 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(summary.contains(COMMITMENT), "{summary}");

    let latest_fact = format!("Flushed fact {}", *model.compactions.lock().unwrap());
    let lease = Lease {
        agent: "assistant".into(),
        key: "k".into(),
        owner: "test".into(),
        epoch: 0,
    };
    let history = format!("{:?}", store.history(&lease).await.unwrap());
    assert!(
        history.contains(&latest_fact),
        "a fact saved during the flush stays in history"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_failing_compaction_backs_off_instead_of_retrying_every_turn(pool: PgPool) {
    let store = Store::new(pool.clone());
    let runner = runner(
        &pool,
        Chatty {
            fail_compaction: true,
            ..Chatty::default()
        },
    );
    converse(&store, &runner, 14).await;

    assert_eq!(*runner.model().compactions.lock().unwrap(), 1);
    let (failures, waiting): (i32, bool) =
        sqlx::query_as("select compaction_failures, compaction_retry_after > now() from sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(failures, 1);
    assert!(waiting);
    let checkpoints: i64 = sqlx::query_scalar("select count(*) from checkpoints")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(checkpoints, 0);
}
