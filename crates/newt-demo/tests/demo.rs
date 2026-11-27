#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::ModelRequest;
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_demo::{Demo, app};
use newt_server::home::{HomeBackend, HomeServer};
use newt_server::runner::{Runner, RunnerConfig};
use newt_server::store::{InboundEvent, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "demo-test-token";

#[derive(Default)]
struct Scripted {
    script: Mutex<VecDeque<Vec<Part>>>,
    reads: Mutex<Vec<String>>,
    systems: Mutex<Vec<String>>,
    tools: Mutex<Vec<String>>,
}

fn call(id: &str, name: &str, arguments: Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    })
}

impl Model for Scripted {
    fn stream(
        &self,
        request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        self.systems
            .lock()
            .unwrap()
            .push(request.prompt.system.to_owned());
        *self.tools.lock().unwrap() = request
            .prompt
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        if let Some(last) = request.prompt.scratch.last() {
            for part in &last.parts {
                if let Part::ToolResult(result) = part
                    && result.content.contains("--- ")
                {
                    self.reads.lock().unwrap().push(result.content.clone());
                }
            }
        }
        let next = self.script.lock().unwrap().pop_front();
        std::future::ready(
            next.map(|parts| ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    parts,
                },
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

async fn until_idle(store: &Store) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = store
            .session_status("assistant", "ana")
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

async fn say(store: &Store, runner: &Runner<HomeBackend, Scripted>, id: &str, text: &str) {
    let event: InboundEvent = serde_json::from_value(json!({ "id": id, "text": text })).unwrap();
    store
        .accept_event("assistant", "ana", &event, false)
        .await
        .unwrap();
    runner.wake("assistant", "ana");
    until_idle(store).await;
}

fn backend(url: String) -> HomeBackend {
    HomeBackend::new(
        BTreeMap::from([(
            "assistant".to_owned(),
            HomeServer {
                url,
                token: TOKEN.into(),
            },
        )]),
        ["default".to_owned()],
    )
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_demo_assistant_keeps_tasks_and_notes_and_replies(pool: PgPool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let demo = Demo::new(TOKEN.into(), &format!("http://{address}"));
    tokio::spawn({
        let router = app(Arc::clone(&demo));
        async move { axum::serve(listener, router).await.unwrap() }
    });
    let model = converse(pool, backend(format!("http://{address}/mcp"))).await;
    assert_eq!(
        demo.sent("ana"),
        vec!["Added it to your list.", "Marked it done."]
    );
    let system = &model.0[0];
    assert!(
        system.starts_with("You are a personal assistant"),
        "{system}"
    );
    assert!(
        system.contains("- assistant/writing: How to word a message to the person."),
        "{system}"
    );
    assert!(model.1.contains(&"load_skill".to_owned()));
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_same_assistant_runs_as_a_child_process_over_stdio(pool: PgPool) {
    let command = format!("stdio:{} --stdio", env!("CARGO_BIN_EXE_newt-demo"));
    let model = converse(pool, backend(command)).await;
    assert!(model.0[0].starts_with("You are a personal assistant"));
}

/// Two turns with the demo app: add a task and a note and reply, then
/// complete the task and reply. Checks what the model read, and returns the
/// system prompts and tool names it saw.
async fn converse(pool: PgPool, backend: HomeBackend) -> (Vec<String>, Vec<String>) {
    let script = vec![
        vec![
            call("t1", "add_task", json!({ "text": "Book the dentist" })),
            call("n1", "add_note", json!({ "text": "Lives in Lisbon" })),
        ],
        vec![call(
            "r1",
            "read_file",
            json!({ "paths": ["tasks.md", "notes.md"] }),
        )],
        vec![call(
            "s1",
            "send_message",
            json!({ "text": "Added it to your list." }),
        )],
        vec![call(
            "e1",
            "end_turn",
            json!({ "recorded": ["dentist task", "Lisbon"] }),
        )],
        vec![call("c1", "complete_task", json!({ "id": 1 }))],
        vec![call("r2", "read_file", json!({ "paths": ["tasks.md"] }))],
        vec![call(
            "s2",
            "send_message",
            json!({ "text": "Marked it done." }),
        )],
        vec![call(
            "e2",
            "end_turn",
            json!({ "nothing_new": "task closed" }),
        )],
    ];
    let store = Store::new(pool.clone());
    let runner = Runner::builder(
        Store::new(pool),
        backend,
        Scripted {
            script: Mutex::new(script.into()),
            ..Scripted::default()
        },
    )
    .config(RunnerConfig::default())
    .build();

    say(
        &store,
        &runner,
        "e1",
        "Remind me to book the dentist. I live in Lisbon now.",
    )
    .await;
    say(&store, &runner, "e2", "Booked it!").await;

    let reads = runner.model().reads.lock().unwrap().clone();
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert!(
        reads[0].contains("- [ ] #1 Book the dentist"),
        "{}",
        reads[0]
    );
    assert!(reads[0].contains("- Lives in Lisbon"), "{}", reads[0]);
    assert!(
        reads[1].contains("- [x] #1 Book the dentist"),
        "{}",
        reads[1]
    );
    let model = runner.model();
    let systems = model.systems.lock().unwrap().clone();
    let tools = model.tools.lock().unwrap().clone();
    (systems, tools)
}
