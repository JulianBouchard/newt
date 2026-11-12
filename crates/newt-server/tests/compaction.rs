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
    largest_history: Mutex<usize>,
    compactions: Mutex<usize>,
    histories_starting_with_checkpoint: Mutex<usize>,
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

fn call(id: &str, name: &str, arguments: serde_json::Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    })
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
            *self.compactions.lock().unwrap() += 1;
            vec![call(
                "c",
                "checkpoint",
                json!({
                    "memory": ["Prefers remote roles"],
                    "open_threads": [],
                    "commitments": [COMMITMENT],
                    "decisions": [],
                    "tone": "short",
                }),
            )]
        } else {
            let bytes: usize = request
                .prompt
                .history
                .iter()
                .map(|message| serde_json::to_string(message).unwrap().len())
                .sum();
            let mut largest = self.largest_history.lock().unwrap();
            *largest = (*largest).max(bytes);
            if request
                .prompt
                .history
                .first()
                .is_some_and(|first| text_of(first).starts_with("<checkpoint>"))
            {
                *self.histories_starting_with_checkpoint.lock().unwrap() += 1;
            }
            vec![
                Part::text("Here is a long and careful answer. ".repeat(20)),
                call("e", "end_turn", json!({ "nothing_new": "small talk" })),
            ]
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

#[sqlx::test(migrations = "../../migrations")]
async fn a_long_session_stays_under_its_history_budget(pool: PgPool) {
    let store = Store::new(pool.clone());
    let runner = Runner::new(
        Store::new(pool.clone()),
        Budgeted,
        Chatty::default(),
        RunnerConfig {
            sweep_every: Duration::from_millis(50),
            ..RunnerConfig::default()
        },
        None,
        broadcast::channel(16).0,
    );
    for turn in 0..30 {
        let event: InboundEvent = serde_json::from_value(json!({
            "id": format!("e{turn}"),
            "text": format!("Message {turn}: {}", "tell me more about remote roles. ".repeat(10)),
        }))
        .unwrap();
        store.accept_event("assistant", "k", &event).await.unwrap();
        runner.wake("assistant", "k");
        until_idle(&store).await;
    }

    let checkpoints: i64 = sqlx::query_scalar("select count(*) from checkpoints")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        checkpoints >= 3,
        "expected several compactions, saw {checkpoints}"
    );
    let raw: i64 = sqlx::query_scalar("select count(*) from history where kind = 'message'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(raw, 60, "raw history is never deleted");

    let model = runner_model(&runner);
    let largest = *model.largest_history.lock().unwrap();
    assert!(
        largest / 4 <= usize::try_from(BUDGET).unwrap() + 400,
        "the prompt history grew to about {} tokens",
        largest / 4
    );
    assert!(*model.histories_starting_with_checkpoint.lock().unwrap() > 0);

    let summary: String =
        sqlx::query_scalar("select summary from checkpoints order by through_seq desc limit 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(summary.contains(COMMITMENT), "{summary}");
}

fn runner_model(runner: &Runner<Budgeted, Chatty>) -> &Chatty {
    runner.model()
}
