#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests fail loudly on setup errors"
)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use newt_core::journal::NoJournal;
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Effort, Lane, ModelRequest, ToolSpec};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::NoSink;
use newt_core::steering::NoSteering;
use newt_core::tools::{CallContext, Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::turn::{TurnDeps, TurnInput, TurnSettings, run_turn};
use newt_core::workspace::Workspace;
use proptest::prelude::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct ScriptedModel {
    script: Mutex<VecDeque<Message>>,
    scratches: Mutex<Vec<Vec<Message>>>,
}

impl Model for ScriptedModel {
    fn stream(
        &self,
        request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        self.scratches
            .lock()
            .unwrap()
            .push(request.prompt.scratch.to_vec());
        let next = self.script.lock().unwrap().pop_front();
        std::future::ready(
            next.ok_or_else(|| ProviderError::Protocol("script exhausted".into()))
                .map(|message| ModelResponse {
                    message,
                    usage: Usage::default(),
                    stop: StopReason::ToolUse,
                    timing: CallTiming::default(),
                    response_id: None,
                    request_ids: BTreeMap::new(),
                }),
        )
    }
}

struct Host;

impl ToolHost for Host {
    fn call(
        &self,
        call: &ToolCall,
        _context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        std::future::ready(match call.name.as_str() {
            "save_note" | "send_message" | "no_reply" => ToolOutput::ok("{}"),
            _ => ToolOutput::error("tool failed"),
        })
    }
}

fn external_tools() -> Vec<ToolDef> {
    let tool = |name: &str, retention, ends_turn| ToolDef {
        spec: ToolSpec {
            name: name.into(),
            description: format!("{name} tool"),
            input_schema: json!({ "type": "object" }),
        },
        retention,
        invalidates: Vec::new(),
        ends_turn,
        idempotent: retention == Retention::Outward,
    };
    vec![
        tool("save_note", Retention::Durable, false),
        tool("send_message", Retention::Outward, false),
        tool("fail_send", Retention::Outward, false),
        tool("no_reply", Retention::Ephemeral, true),
    ]
}

const NAMES: [&str; 20] = [
    "read_file",
    "glob",
    "grep",
    "end_turn",
    "remember",
    "batch",
    "checkpoint",
    "save_note",
    "send_message",
    "fail_send",
    "no_reply",
    "functions.save_note",
    "functions.end_turn",
    "functions.read_file",
    "unknown_tool",
    "Save_Note",
    " save_note",
    "save_note ",
    "",
    "batch",
];

fn arguments() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(json!({})),
        Just(json!({ "paths": ["profile.md"] })),
        Just(json!({ "pattern": "remote" })),
        Just(json!({ "note": "prefers remote" })),
        Just(json!({ "text": "hello" })),
        Just(json!({ "recorded": ["x"] })),
        Just(json!({ "nothing_new": "greeting" })),
        Just(json!({ "recorded": ["x"], "nothing_new": "y" })),
        Just(
            json!({ "calls": [{ "name": "functions.read_file", "arguments": { "paths": ["profile.md"] } }] })
        ),
        Just(json!({ "calls": [{ "name": "end_turn", "arguments": {} }] })),
        Just(
            json!({ "calls": [{ "name": "send_message", "arguments": {} }, { "name": "send_message", "arguments": {} }] })
        ),
        Just(json!({ "calls": "not a list" })),
        Just(json!({ "_newt_invalid_arguments": "{\"text\":" })),
        Just(json!("{\"text\":\"hello\"}")),
        Just(json!("")),
        Just(Value::Null),
        Just(json!([])),
        Just(json!(7)),
    ]
}

fn tool_call() -> impl Strategy<Value = Part> {
    (prop::sample::select(NAMES.to_vec()), arguments()).prop_map(|(name, arguments)| {
        Part::ToolCall(ToolCall {
            id: String::new(),
            name: name.to_owned(),
            arguments,
        })
    })
}

fn response() -> impl Strategy<Value = Message> {
    (
        prop::option::of(prop::sample::select(vec!["", "  ", "Sure.", "Done\n"])),
        prop::collection::vec(tool_call(), 0..4),
    )
        .prop_map(|(text, calls)| {
            let mut parts: Vec<Part> = text.into_iter().map(Part::text).collect();
            parts.extend(calls);
            Message {
                role: Role::Assistant,
                parts,
            }
        })
}

struct Ran {
    ok: bool,
    committed: Vec<Message>,
    scratches: Vec<Vec<Message>>,
}

fn run(script: Vec<Message>, require_end_turn: bool) -> Ran {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let model = ScriptedModel {
            script: Mutex::new(script.into()),
            scratches: Mutex::new(Vec::new()),
        };
        let tools = external_tools();
        let workspace = Workspace::new(BTreeMap::from([(
            "profile.md".to_owned(),
            "Prefers remote roles".to_owned(),
        )]));
        let history = vec![Message::user_text("earlier turn")];
        let turn = async {
            run_turn(
                &TurnDeps {
                    model: &model,
                    host: &Host,
                    steering: &NoSteering,
                    journal: &NoJournal,
                    sink: &NoSink,
                    cancel: &CancellationToken::new(),
                },
                TurnInput {
                    session: "s1",
                    cache_key: "s1",
                    turn: "t1",
                    system: "system",
                    external_tools: &tools,
                    history: &history,
                    context: None,
                    skills: &[],
                    events: vec![Message::user_text("Only remote roles please.")],
                    workspace: &workspace,
                    replay: Vec::new(),
                },
                &TurnSettings {
                    model: "scripted",
                    first_call_effort: None,
                    effort: Some(Effort::Low),
                    max_output_tokens: 1024,
                    max_model_calls: 6,
                    max_turn_tokens: None,
                    recite: true,
                    require_end_turn,
                    builtin_tools: newt_core::tools::BuiltinTools::ALL,
                    page_index_wait: std::time::Duration::from_secs(2),
                    lane: Lane::Interactive,
                    reset: false,
                    compaction: None,
                },
            )
            .await
        };
        let outcome = tokio::time::timeout(Duration::from_secs(10), turn)
            .await
            .expect("the turn hung");
        let (ok, committed) = match outcome {
            Ok(outcome) => (true, outcome.committed),
            Err(failure) => (false, failure.committed),
        };
        Ran {
            ok,
            committed,
            scratches: model.scratches.into_inner().unwrap(),
        }
    })
}

fn call_ids(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect()
}

fn result_ids(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect()
}

fn well_formed(messages: &[Message]) -> Result<(), String> {
    let calls = call_ids(messages);
    let results = result_ids(messages);
    if calls != results {
        return Err(format!("calls {calls:?} but results {results:?}"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in calls {
        if !seen.insert(id) {
            return Err(format!("call id {id:?} used twice"));
        }
    }
    Ok(())
}

fn settled(scratch: &[Message]) -> &[Message] {
    scratch
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn a_quirky_model_never_hangs_panics_or_breaks_history(
        script in prop::collection::vec(response(), 0..6),
        require_end_turn in any::<bool>(),
    ) {
        let mut script = script;
        let mut next_id = 0;
        for call in script.iter_mut().flat_map(|message| &mut message.parts) {
            if let Part::ToolCall(call) = call {
                next_id += 1;
                call.id = format!("call_{next_id}");
            }
        }
        let ran = run(script, require_end_turn);
        prop_assert!(ran.scratches.len() <= 6);
        for (index, scratch) in ran.scratches.iter().enumerate() {
            let checked = well_formed(settled(scratch));
            prop_assert!(checked.is_ok(), "request {index}: {checked:?}\n{scratch:#?}");
        }
        let committed = well_formed(&ran.committed);
        prop_assert!(committed.is_ok(), "committed ({}): {committed:?}\n{:#?}", ran.ok, ran.committed);
    }
}

#[test]
fn a_functions_prefixed_name_at_the_top_level_resolves_to_the_tool() {
    let script = vec![Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: "a".to_owned(),
            name: "functions.end_turn".to_owned(),
            arguments: json!({}),
        })],
    }];
    let ran = run(script, false);
    assert!(ran.ok);
    assert_eq!(
        ran.scratches.len(),
        1,
        "the prefixed end_turn ended the turn in one model call"
    );
}
