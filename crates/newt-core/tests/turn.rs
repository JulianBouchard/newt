#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests fail loudly on setup errors"
)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;

use newt_core::event::{Event, EventKind};
use newt_core::journal::NoJournal;
use newt_core::message::{Message, Part, Role, ToolCall, ToolResult};
use newt_core::prompt::{Effort, Lane, ModelRequest, ToolSpec};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::EventSink;
use newt_core::steering::NoSteering;
use newt_core::tools::{Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::turn::{
    TurnDeps, TurnError, TurnFailure, TurnInput, TurnOutcome, TurnSettings, run_turn,
};
use newt_core::workspace::Workspace;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct Seen {
    effort: Option<Effort>,
    scratch: Vec<Message>,
}

struct ScriptedModel {
    script: Mutex<VecDeque<Message>>,
    seen: Mutex<Vec<Seen>>,
}

impl ScriptedModel {
    fn new(script: Vec<Message>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Model for ScriptedModel {
    fn stream(
        &self,
        request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        self.seen.lock().unwrap().push(Seen {
            effort: request.effort,
            scratch: request.prompt.scratch.to_vec(),
        });
        let next = self.script.lock().unwrap().pop_front();
        let result = next
            .ok_or_else(|| ProviderError::Protocol("script exhausted".into()))
            .map(|message| {
                for call in message.tool_calls() {
                    on_event(StreamEvent::ToolCallStarted {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    });
                }
                ModelResponse {
                    message,
                    usage: Usage {
                        input_tokens: 1000,
                        cache_read_tokens: 800,
                        ..Usage::default()
                    },
                    stop: StopReason::ToolUse,
                    timing: CallTiming::default(),
                    response_id: None,
                    request_ids: std::collections::BTreeMap::new(),
                }
            });
        std::future::ready(result)
    }
}

#[derive(Default)]
struct RecordingHost {
    calls: Mutex<Vec<String>>,
}

impl ToolHost for RecordingHost {
    fn call(&self, call: &ToolCall, _key: &str) -> impl Future<Output = ToolOutput> + Send {
        self.calls.lock().unwrap().push(call.name.clone());
        let output = match call.name.as_str() {
            "save_note" => ToolOutput {
                content: "{\"note\":{\"id\":7}}".into(),
                is_error: false,
                summary: Some(format!(
                    "Saved: {}",
                    call.arguments["text"].as_str().unwrap()
                )),
            },
            "send_message" => ToolOutput {
                content: "{\"delivered\":true}".into(),
                is_error: false,
                summary: Some("Sent message".into()),
            },
            _ => ToolOutput {
                content: "provider down".into(),
                is_error: true,
                summary: None,
            },
        };
        std::future::ready(output)
    }
}

#[derive(Default)]
struct CollectingSink {
    events: Mutex<Vec<Event>>,
}

impl EventSink for CollectingSink {
    fn emit(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

fn external_tools() -> Vec<ToolDef> {
    let tool = |name: &str, retention| ToolDef {
        spec: ToolSpec {
            name: name.into(),
            description: format!("{name} tool"),
            input_schema: json!({ "type": "object" }),
        },
        retention,
    };
    vec![
        tool("save_note", Retention::Durable),
        tool("send_message", Retention::Outward),
        tool("fail_send", Retention::Outward),
    ]
}

fn call(id: &str, name: &str, arguments: Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    })
}

fn assistant(parts: Vec<Part>) -> Message {
    Message {
        role: Role::Assistant,
        parts,
    }
}

fn result(id: &str, content: &str) -> Part {
    Part::ToolResult(ToolResult {
        call_id: id.into(),
        content: content.into(),
        is_error: false,
    })
}

struct Run {
    outcome: Result<TurnOutcome, TurnFailure>,
    model: ScriptedModel,
    host: RecordingHost,
    sink: CollectingSink,
}

async fn run(script: Vec<Message>, max_model_calls: u32, tools: &[ToolDef]) -> Run {
    let model = ScriptedModel::new(script);
    let host = RecordingHost::default();
    let sink = CollectingSink::default();
    let workspace = Workspace::new(BTreeMap::from([(
        "profile.md".to_owned(),
        "Prefers remote roles".to_owned(),
    )]));
    let history = vec![Message::user_text("earlier turn")];
    let outcome = run_turn(
        &TurnDeps {
            model: &model,
            host: &host,
            steering: &NoSteering,
            journal: &NoJournal,
            sink: &sink,
            cancel: &CancellationToken::new(),
        },
        TurnInput {
            session: "customer-1",
            turn: "turn-9",
            system: "You are a helpful agent.",
            external_tools: tools,
            history: &history,
            events: vec![Message::user_text("Only remote roles please.")],
            workspace: &workspace,
            replay: Vec::new(),
        },
        &TurnSettings {
            model: "scripted",
            first_call_effort: Some(Effort::Low),
            effort: Some(Effort::Medium),
            max_output_tokens: 4096,
            max_model_calls,
            lane: Lane::Interactive,
        },
    )
    .await;
    Run {
        outcome,
        model,
        host,
        sink,
    }
}

#[tokio::test]
async fn a_full_turn_commits_writes_and_sends_and_drops_reads() {
    let script = vec![
        assistant(vec![
            call("r1", "read_file", json!({ "paths": ["profile.md"] })),
            call("w1", "save_note", json!({ "text": "remote only" })),
        ]),
        assistant(vec![
            Part::text("Got it, remote only."),
            call("s1", "send_message", json!({ "text": "Noted!" })),
            call("e1", "end_turn", json!({ "recorded": ["remote only"] })),
        ]),
    ];
    let run = run(script, 8, &external_tools()).await;
    let outcome = run.outcome.unwrap();

    assert_eq!(outcome.end.recorded, vec!["remote only"]);
    assert_eq!(
        outcome.committed,
        vec![
            Message::user_text("Only remote roles please."),
            assistant(vec![call(
                "w1",
                "save_note",
                json!({ "text": "remote only" })
            )]),
            Message {
                role: Role::User,
                parts: vec![result("w1", "Saved: remote only")],
            },
            assistant(vec![
                Part::text("Got it, remote only."),
                call("s1", "send_message", json!({ "text": "Noted!" })),
            ]),
            Message {
                role: Role::User,
                parts: vec![result("s1", "Sent message")],
            },
        ]
    );
    assert_eq!(
        *run.host.calls.lock().unwrap(),
        vec!["save_note", "send_message"]
    );

    let seen = run.model.seen.lock().unwrap();
    assert_eq!(seen[0].effort, Some(Effort::Low));
    assert_eq!(seen[1].effort, Some(Effort::Medium));
    let Part::ToolResult(read) = &seen[1].scratch[2].parts[0] else {
        unreachable!("the third scratch message holds the first batch's results");
    };
    assert!(read.content.contains("Prefers remote roles"));

    let events = run.sink.events.lock().unwrap();
    let started: Vec<&str> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec!["read_file", "save_note", "send_message", "end_turn"]
    );
    assert!(events.iter().any(|event| matches!(
        event.kind,
        EventKind::TurnCompleted {
            model_calls: 2,
            committed_messages: 5,
            ..
        }
    )));
}

#[tokio::test]
async fn stopping_without_end_turn_gets_one_reminder_that_is_never_committed() {
    let script = vec![
        assistant(vec![Part::text("Sure.")]),
        assistant(vec![call(
            "e1",
            "end_turn",
            json!({ "nothing_new": "just a greeting" }),
        )]),
    ];
    let run = run(script, 8, &external_tools()).await;
    let outcome = run.outcome.unwrap();

    assert_eq!(
        outcome.committed,
        vec![
            Message::user_text("Only remote roles please."),
            assistant(vec![Part::text("Sure.")]),
        ]
    );
    assert_eq!(run.model.seen.lock().unwrap()[1].scratch.len(), 3);
}

#[tokio::test]
async fn stopping_twice_without_end_turn_is_an_error() {
    let script = vec![
        assistant(vec![Part::text("Sure.")]),
        assistant(vec![Part::text("Done.")]),
    ];
    let run = run(script, 8, &external_tools()).await;
    assert!(
        matches!(
            &run.outcome,
            Err(TurnFailure {
                error: TurnError::NoEndTurn,
                ..
            })
        ),
        "{:?}",
        run.outcome
    );
}

#[tokio::test]
async fn a_failed_outward_call_skips_later_outward_calls_and_blocks_end_turn() {
    let script = vec![
        assistant(vec![
            call("f1", "fail_send", json!({})),
            call("s1", "send_message", json!({ "text": "hi" })),
            call("e1", "end_turn", json!({ "recorded": ["x"] })),
        ]),
        assistant(vec![call(
            "e2",
            "end_turn",
            json!({ "nothing_new": "send failed, will retry later" }),
        )]),
    ];
    let run = run(script, 8, &external_tools()).await;
    let outcome = run.outcome.unwrap();

    assert_eq!(*run.host.calls.lock().unwrap(), vec!["fail_send"]);
    assert_eq!(
        outcome.end.nothing_new.as_deref(),
        Some("send failed, will retry later")
    );
    let seen = run.model.seen.lock().unwrap();
    let results: Vec<(&str, bool)> = seen[1].scratch[2]
        .parts
        .iter()
        .map(|part| match part {
            Part::ToolResult(result) => (result.call_id.as_str(), result.is_error),
            other => unreachable!("expected only tool results, got {other:?}"),
        })
        .collect();
    assert_eq!(results, vec![("f1", true), ("s1", true), ("e1", true)]);
}

#[tokio::test]
async fn running_out_of_model_calls_is_an_error() {
    let script = vec![assistant(vec![call(
        "r1",
        "read_file",
        json!({ "paths": ["profile.md"] }),
    )])];
    let run = run(script, 1, &external_tools()).await;
    assert!(
        matches!(
            &run.outcome,
            Err(TurnFailure {
                error: TurnError::TooManyModelCalls(1),
                ..
            })
        ),
        "{:?}",
        run.outcome
    );
}

#[tokio::test]
async fn an_external_tool_shadowing_a_builtin_is_rejected() {
    let mut tools = external_tools();
    tools[0].spec.name = "read_file".into();
    let run = run(Vec::new(), 8, &tools).await;
    assert!(
        matches!(&run.outcome, Err(TurnFailure { error: TurnError::DuplicateTool(name), .. }) if name == "read_file"),
        "{:?}",
        run.outcome
    );
}

#[tokio::test]
async fn a_turn_that_fails_after_sending_still_commits_the_send() {
    let script = vec![assistant(vec![call(
        "s1",
        "send_message",
        json!({ "text": "hi" }),
    )])];
    let run = run(script, 1, &external_tools()).await;

    let Err(failure) = run.outcome else {
        panic!("expected the turn to fail, got {:?}", run.outcome);
    };
    assert!(matches!(failure.error, TurnError::TooManyModelCalls(1)));
    assert_eq!(
        failure.committed,
        vec![
            Message::user_text("Only remote roles please."),
            assistant(vec![call("s1", "send_message", json!({ "text": "hi" }))]),
            Message {
                role: Role::User,
                parts: vec![result("s1", "Sent message")],
            },
        ]
    );
}

#[tokio::test]
async fn invalid_arguments_are_refused_and_block_end_turn() {
    let script = vec![
        assistant(vec![
            call(
                "w1",
                "save_note",
                json!({ newt_core::message::INVALID_ARGUMENTS_KEY: "{\"text\": \"cut" }),
            ),
            call("e1", "end_turn", json!({ "recorded": ["note"] })),
        ]),
        assistant(vec![call(
            "e2",
            "end_turn",
            json!({ "nothing_new": "retry next turn" }),
        )]),
    ];
    let run = run(script, 8, &external_tools()).await;
    run.outcome.unwrap();

    assert!(run.host.calls.lock().unwrap().is_empty());
    let seen = run.model.seen.lock().unwrap();
    let errors: Vec<bool> = seen[1].scratch[2]
        .parts
        .iter()
        .map(|part| matches!(part, Part::ToolResult(result) if result.is_error))
        .collect();
    assert_eq!(errors, vec![true, true]);
}

#[tokio::test]
async fn a_cancelled_turn_starts_no_outward_calls_and_reports_cancellation() {
    let model = ScriptedModel::new(vec![assistant(vec![call(
        "s1",
        "send_message",
        json!({ "text": "hi" }),
    )])]);
    let host = RecordingHost::default();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let workspace = Workspace::default();

    let outcome = run_turn(
        &TurnDeps {
            model: &model,
            host: &host,
            steering: &NoSteering,
            journal: &NoJournal,
            sink: &CollectingSink::default(),
            cancel: &cancel,
        },
        TurnInput {
            session: "customer-1",
            turn: "turn-10",
            system: "You are a helpful agent.",
            external_tools: &external_tools(),
            history: &[],
            events: vec![Message::user_text("Stop.")],
            workspace: &workspace,
            replay: Vec::new(),
        },
        &TurnSettings {
            model: "scripted",
            first_call_effort: None,
            effort: None,
            max_output_tokens: 1024,
            max_model_calls: 1,
            lane: Lane::Interactive,
        },
    )
    .await;

    assert!(host.calls.lock().unwrap().is_empty());
    assert!(outcome.is_err());
}
