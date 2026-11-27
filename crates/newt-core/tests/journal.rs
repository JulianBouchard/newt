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
use newt_core::tools::{CallContext, Retention, ToolDef, ToolHost, ToolOutput};
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
    fn call(
        &self,
        call: &ToolCall,
        context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        let key = context.idempotency_key.as_str();
        self.calls
            .lock()
            .unwrap()
            .push((call.name.clone(), key.to_owned()));
        std::future::ready(ToolOutput {
            content: "{}".into(),
            is_error: false,
            summary: Some(format!("{} done", call.name)),
            ..ToolOutput::default()
        })
    }
}

#[derive(Default)]
struct Recorder {
    entries: Mutex<Vec<JournalEntry>>,
    waits: AtomicUsize,
    flushes: AtomicUsize,
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

    fn flush_relaxed(&self) -> impl Future<Output = Result<(), JournalError>> + Send {
        std::future::poll_fn(|_| {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            std::task::Poll::Ready(Ok(()))
        })
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
        invalidates: Vec::new(),
        ends_turn: false,
        idempotent: retention == Retention::Outward,
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
            cache_key: "customer-1",
            turn: "turn-9",
            system: "You are a helpful agent.",
            external_tools: &tools(),
            history: &[],
            context: None,
            skills: &[],
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
            max_turn_tokens: None,
            recite: true,
            require_end_turn: true,
            builtin_tools: newt_core::tools::BuiltinTools::ALL,
            page_index_wait: std::time::Duration::from_secs(2),
            lane: Lane::Interactive,
            reset: false,
            compaction: None,
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
        JournalEntry::Host(_) => "host".to_owned(),
    }
}

#[tokio::test]
async fn a_turn_waits_for_the_journal_once_per_step_that_writes_and_before_each_outward_call() {
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
    assert_eq!(journal.flushes.load(Ordering::Relaxed), 1);
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
    assert_eq!(
        journal.waits.load(Ordering::Relaxed),
        1,
        "the resend re-checks the lease with one fenced journal write"
    );
    assert_eq!(resumed.committed, completed.committed);
}

#[tokio::test]
async fn a_resumed_turn_reports_a_started_write_as_interrupted_instead_of_running_it() {
    let first_model = Scripted {
        script: Mutex::new(script().into()),
        calls: AtomicUsize::new(0),
    };
    let first_journal = Recorder::default();
    run(&first_model, &Host::default(), &first_journal, Vec::new()).await;
    let crashed: Vec<JournalEntry> = first_journal.entries.lock().unwrap()[..1].to_vec();

    let model = Scripted {
        script: Mutex::new(script()[1..].to_vec().into()),
        calls: AtomicUsize::new(0),
    };
    let host = Host::default();
    let resumed = run(&model, &host, &Recorder::default(), crashed).await;

    let names: Vec<String> = host
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(names, vec!["send_message".to_owned()]);
    let interrupted = resumed.committed.iter().flat_map(|message| &message.parts).any(|part| {
        matches!(part, Part::ToolResult(result) if result.call_id == "w1" && result.is_error && result.content.starts_with("Interrupted:"))
    });
    assert!(interrupted, "the model is told the write may have happened");
}

#[tokio::test]
async fn a_resumed_turn_runs_a_write_whose_response_was_never_journaled() {
    let model = Scripted {
        script: Mutex::new(script().into()),
        calls: AtomicUsize::new(0),
    };
    let host = Host::default();
    run(&model, &host, &Recorder::default(), Vec::new()).await;

    assert_eq!(
        model.calls.load(Ordering::Relaxed),
        2,
        "the model is asked again"
    );

    assert_eq!(
        *host.calls.lock().unwrap(),
        vec![
            ("save_note".to_owned(), "customer-1:turn-9:w1".to_owned()),
            ("send_message".to_owned(), "customer-1:turn-9:s1".to_owned())
        ]
    );
}
