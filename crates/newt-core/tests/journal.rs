#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use newt_core::event::Event;
use newt_core::journal::{Journal, JournalEntry, JournalError};
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Lane, ModelRequest, ToolSpec};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::EventSink;
use newt_core::steering::NoSteering;
use newt_core::tools::{Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::turn::{TurnDeps, TurnInput, TurnOutcome, TurnSettings, run_turn};
use newt_core::workspace::Workspace;
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Scripted {
    script: Mutex<VecDeque<Message>>,
    calls: AtomicUsize,
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

#[derive(Default)]
struct Host {
    calls: Mutex<Vec<(String, String)>>,
}

impl ToolHost for Host {
    fn call(&self, call: &ToolCall, key: &str) -> impl Future<Output = ToolOutput> + Send {
        self.calls
            .lock()
            .unwrap()
            .push((call.name.clone(), key.to_owned()));
        std::future::ready(ToolOutput {
            content: "{}".into(),
            is_error: false,
            summary: Some(format!("{} done", call.name)),
        })
    }
}

#[derive(Default)]
struct Recorder {
    entries: Mutex<Vec<JournalEntry>>,
    waits: AtomicUsize,
}

impl Journal for Recorder {
    fn append(&self, entry: JournalEntry) {
        self.entries.lock().unwrap().push(entry);
    }

    fn append_and_wait(
        &self,
        entry: JournalEntry,
    ) -> impl Future<Output = Result<(), JournalError>> + Send {
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().unwrap().push(entry);
        std::future::ready(Ok(()))
    }
}

struct Quiet;

impl EventSink for Quiet {
    fn emit(&self, _event: Event) {}
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    })
}

fn script() -> Vec<Message> {
    vec![
        Message {
            role: Role::Assistant,
            parts: vec![
                call("r1", "read_file", json!({ "paths": ["profile.md"] })),
                call("w1", "save_note", json!({ "text": "remote" })),
            ],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                call("s1", "send_message", json!({ "text": "Noted" })),
                call("e1", "end_turn", json!({ "recorded": ["remote"] })),
            ],
        },
    ]
}

fn tools() -> Vec<ToolDef> {
    let tool = |name: &str, retention| ToolDef {
        spec: ToolSpec {
            name: name.into(),
            description: name.into(),
            input_schema: json!({ "type": "object" }),
        },
        retention,
    };
    vec![
        tool("save_note", Retention::Durable),
        tool("send_message", Retention::Outward),
    ]
}

async fn run(
    model: &Scripted,
    host: &Host,
    journal: &Recorder,
    replay: Vec<JournalEntry>,
) -> TurnOutcome {
    run_turn(
        &TurnDeps {
            model,
            host,
            steering: &NoSteering,
            journal,
            sink: &Quiet,
            cancel: &CancellationToken::new(),
        },
        TurnInput {
            session: "customer-1",
            turn: "turn-9",
            system: "You are a helpful agent.",
            external_tools: &tools(),
            history: &[],
            events: vec![Message::user_text("Remote only.")],
            workspace: &Workspace::default(),
            replay,
        },
        &TurnSettings {
            model: "scripted",
            first_call_effort: None,
            effort: None,
            max_output_tokens: 1024,
            max_model_calls: 6,
            lane: Lane::Interactive,
        },
    )
    .await
    .unwrap()
}

fn kind(entry: &JournalEntry) -> String {
    match entry {
        JournalEntry::Steered { .. } => "steered".into(),
        JournalEntry::ModelResponse { call, .. } => format!("model:{call}"),
        JournalEntry::OutwardIntent { call_id } => format!("intent:{call_id}"),
        JournalEntry::ToolOutput { call_id, .. } => format!("output:{call_id}"),
    }
}

#[tokio::test]
async fn a_turn_records_responses_outputs_and_waits_only_on_outward_intents() {
    let model = Scripted {
        script: Mutex::new(script().into()),
        calls: AtomicUsize::new(0),
    };
    let journal = Recorder::default();

    run(&model, &Host::default(), &journal, Vec::new()).await;

    let kinds: Vec<String> = journal.entries.lock().unwrap().iter().map(kind).collect();
    assert_eq!(
        kinds,
        vec!["model:0", "output:w1", "model:1", "intent:s1", "output:s1"]
    );
    assert_eq!(journal.waits.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_resumed_turn_replays_and_resends_only_the_unfinished_outward_call() {
    let first_model = Scripted {
        script: Mutex::new(script().into()),
        calls: AtomicUsize::new(0),
    };
    let first_journal = Recorder::default();
    let completed = run(&first_model, &Host::default(), &first_journal, Vec::new()).await;
    let mut crashed: Vec<JournalEntry> = first_journal.entries.lock().unwrap().clone();
    crashed.pop();

    let model = Scripted {
        script: Mutex::new(VecDeque::new()),
        calls: AtomicUsize::new(0),
    };
    let host = Host::default();
    let journal = Recorder::default();
    let resumed = run(&model, &host, &journal, crashed).await;

    assert_eq!(model.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        *host.calls.lock().unwrap(),
        vec![("send_message".to_owned(), "customer-1:turn-9:s1".to_owned())]
    );
    assert_eq!(journal.waits.load(Ordering::Relaxed), 0);
    assert_eq!(resumed.committed, completed.committed);
}
