#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newt_core::journal::NoJournal;
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Lane, ModelRequest, ToolSpec};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::NoSink;
use newt_core::skills::Skill;
use newt_core::steering::NoSteering;
use newt_core::tools::{BuiltinTools, NoTools, Retention, ToolDef};
use newt_core::turn::{Compaction, TurnDeps, TurnInput, TurnOutcome, TurnSettings, run_turn};
use newt_core::workspace::{ListingFuture, PageFuture, PageKey, PageSource, Workspace};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Request {
    system: String,
    tools: Vec<String>,
    scratch: Vec<Message>,
}

struct Scripted {
    script: Mutex<VecDeque<Message>>,
    seen: Mutex<Vec<Request>>,
}

impl Model for Scripted {
    fn stream(
        &self,
        request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        self.seen.lock().unwrap().push(Request {
            system: request.prompt.system.to_owned(),
            tools: request
                .prompt
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            scratch: request.prompt.scratch.to_vec(),
        });
        let result = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Protocol("script exhausted".into()))
            .map(|message| ModelResponse {
                message,
                usage: Usage::default(),
                stop: StopReason::ToolUse,
                timing: CallTiming::default(),
                response_id: None,
                request_ids: BTreeMap::new(),
            });
        std::future::ready(result)
    }
}

fn says(parts: Vec<Part>) -> Message {
    Message {
        role: Role::Assistant,
        parts,
    }
}

fn call(id: &str, name: &str, arguments: Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
    })
}

fn add_task() -> Vec<ToolDef> {
    vec![ToolDef::new(
        ToolSpec {
            name: "add_task".into(),
            description: "Add a task.".into(),
            input_schema: json!({ "type": "object" }),
        },
        Retention::Durable,
    )]
}

fn skills() -> Vec<Skill> {
    vec![Skill {
        name: "writing".into(),
        description: "How to word a message.".into(),
        content: "Be brief.".into(),
        served_by: None,
    }]
}

async fn turn(
    script: Vec<Message>,
    builtin_tools: BuiltinTools,
    compaction: Option<&[String]>,
) -> (Option<TurnOutcome>, Vec<Request>) {
    let workspace = Workspace::new(BTreeMap::from([("notes.md".to_owned(), "x".to_owned())]));
    turn_on(
        script,
        builtin_tools,
        compaction,
        &workspace,
        Duration::from_secs(2),
    )
    .await
}

async fn turn_on(
    script: Vec<Message>,
    builtin_tools: BuiltinTools,
    compaction: Option<&[String]>,
    workspace: &Workspace,
    page_index_wait: Duration,
) -> (Option<TurnOutcome>, Vec<Request>) {
    let model = Scripted {
        script: Mutex::new(script.into()),
        seen: Mutex::new(Vec::new()),
    };
    let tools = add_task();
    let skills = skills();
    let outcome = run_turn(
        &TurnDeps {
            model: &model,
            host: &NoTools,
            steering: &NoSteering,
            journal: &NoJournal,
            sink: &NoSink,
            cancel: &CancellationToken::new(),
        },
        TurnInput {
            session: "s",
            cache_key: "s",
            turn: "t",
            system: "Be useful.",
            external_tools: &tools,
            history: &[],
            context: None,
            skills: &skills,
            events: vec![Message::user_text("hello")],
            workspace,
            replay: Vec::new(),
        },
        &TurnSettings {
            model: "scripted",
            first_call_effort: None,
            effort: None,
            max_output_tokens: 1024,
            max_model_calls: 4,
            max_turn_tokens: None,
            recite: false,
            require_end_turn: false,
            builtin_tools,
            page_index_wait,
            lane: Lane::Interactive,
            reset: false,
            compaction: compaction.map(|open_commitments| Compaction {
                open_commitments,
                max_checkpoint_chars: 1000,
            }),
        },
    )
    .await;
    (outcome.ok(), model.seen.into_inner().unwrap())
}

fn result_of<'a>(request: &'a Request, call_id: &str) -> &'a str {
    request
        .scratch
        .iter()
        .flat_map(|message| &message.parts)
        .find_map(|part| match part {
            Part::ToolResult(result) if result.call_id == call_id => Some(result.content.as_str()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn by_default_every_builtin_is_offered() {
    let (_, seen) = turn(vec![says(vec![Part::text("Hi.")])], BuiltinTools::ALL, None).await;
    assert_eq!(
        seen[0].tools,
        [
            "read_file",
            "glob",
            "grep",
            "end_turn",
            "remember",
            "batch",
            "checkpoint",
            "load_skill",
            "add_task"
        ]
    );
    assert!(seen[0].system.contains("<skills>"));
}

#[tokio::test]
async fn with_no_builtins_the_model_sees_only_the_agents_tools_and_no_skills_index() {
    let (outcome, seen) = turn(
        vec![says(vec![Part::text("Hi.")])],
        BuiltinTools::NONE,
        None,
    )
    .await;
    assert_eq!(seen[0].tools, ["add_task"]);
    assert!(!seen[0].system.contains("<skills>"), "{}", seen[0].system);
    assert_eq!(outcome.unwrap().reply.as_deref(), Some("Hi."));
}

#[tokio::test]
async fn the_page_index_opens_the_turn_only_when_the_agent_can_read_pages() {
    let opening = |seen: &[Request]| format!("{:?}", seen[0].scratch.first());
    let (_, seen) = turn(vec![says(vec![Part::text("Hi.")])], BuiltinTools::ALL, None).await;
    assert!(opening(&seen).contains("- notes.md"), "{}", opening(&seen));
    let (_, seen) = turn(
        vec![says(vec![Part::text("Hi.")])],
        BuiltinTools::NONE,
        None,
    )
    .await;
    assert!(
        !opening(&seen).contains("<workspace>"),
        "{}",
        opening(&seen)
    );
}

struct NeverListed;

impl PageSource for NeverListed {
    fn list(&self) -> ListingFuture<'_> {
        Box::pin(std::future::pending())
    }

    fn read(&self, _keys: Vec<PageKey>) -> PageFuture<'_> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }
}

#[tokio::test]
async fn a_listing_that_is_not_ready_in_time_starts_the_turn_without_the_index() {
    let workspace = Workspace::loading(Arc::new(NeverListed));
    let script = vec![says(vec![Part::text("Hi.")])];
    let started = std::time::Instant::now();
    let (outcome, seen) = turn_on(
        script,
        BuiltinTools::ALL,
        None,
        &workspace,
        Duration::from_millis(50),
    )
    .await;
    assert!(outcome.is_some());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!format!("{:?}", seen[0].scratch.first()).contains("<workspace>"));
}

#[tokio::test]
async fn a_zero_wait_leaves_the_index_out() {
    let workspace = Workspace::new(BTreeMap::from([("notes.md".to_owned(), "x".to_owned())]));
    let script = vec![says(vec![Part::text("Hi.")])];
    let (_, seen) = turn_on(script, BuiltinTools::ALL, None, &workspace, Duration::ZERO).await;
    assert!(!format!("{:?}", seen[0].scratch.first()).contains("<workspace>"));
}

#[tokio::test]
async fn a_list_keeps_only_the_named_builtins() {
    let only = BuiltinTools::only(&["read_file", "end_turn"]).unwrap();
    let (_, seen) = turn(vec![says(vec![Part::text("Hi.")])], only, None).await;
    assert_eq!(seen[0].tools, ["read_file", "end_turn", "add_task"]);
}

#[tokio::test]
async fn a_call_to_a_builtin_that_is_off_is_an_unknown_tool() {
    let script = vec![
        says(vec![
            call("r", "read_file", json!({ "paths": ["notes.md"] })),
            call("m", "remember", json!({ "note": "x" })),
            call(
                "b",
                "batch",
                json!({ "calls": [{ "name": "add_task", "arguments": {} }] }),
            ),
            call("e", "end_turn", json!({})),
        ]),
        says(vec![Part::text("Done.")]),
    ];
    let (outcome, seen) = turn(script, BuiltinTools::NONE, None).await;
    for id in ["r", "m", "b", "e"] {
        assert!(result_of(&seen[1], id).starts_with("unknown tool"), "{id}");
    }
    assert_eq!(outcome.unwrap().reply.as_deref(), Some("Done."));
}

#[tokio::test]
async fn a_compaction_turn_is_always_offered_checkpoint() {
    let script = vec![says(vec![call(
        "c",
        "checkpoint",
        json!({ "memory": [], "open_threads": [], "commitments": [], "decisions": [], "tone": "plain" }),
    )])];
    let (outcome, seen) = turn(script, BuiltinTools::NONE, Some(&[])).await;
    assert_eq!(seen[0].tools, ["checkpoint", "add_task"]);
    assert!(outcome.unwrap().checkpoint.is_some());
}
