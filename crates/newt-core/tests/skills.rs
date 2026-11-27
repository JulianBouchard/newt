#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;

use newt_core::journal::NoJournal;
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Lane, ModelRequest, ToolSpec};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::NoSink;
use newt_core::skills::Skill;
use newt_core::steering::NoSteering;
use newt_core::tools::{CallContext, PageRead, Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::turn::{TurnDeps, TurnInput, TurnOutcome, TurnSettings, run_turn};
use newt_core::workspace::Workspace;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Scripted {
    script: Mutex<VecDeque<Vec<Part>>>,
    systems: Mutex<Vec<String>>,
    tools: Mutex<Vec<Vec<String>>>,
    errors: Mutex<Vec<String>>,
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
        for part in request
            .prompt
            .scratch
            .iter()
            .flat_map(|message| &message.parts)
        {
            if let Part::ToolResult(result) = part
                && result.is_error
            {
                self.errors.lock().unwrap().push(result.content.clone());
            }
        }
        self.tools.lock().unwrap().push(
            request
                .prompt
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
        );
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

#[derive(Default)]
struct Seeing(Mutex<Vec<CallContext>>, Mutex<Vec<String>>);

impl ToolHost for Seeing {
    fn call(
        &self,
        _call: &ToolCall,
        context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        self.0.lock().unwrap().push(context.clone());
        std::future::ready(ToolOutput::ok("sent"))
    }

    fn load_skill(
        &self,
        skill: &Skill,
        file: Option<&str>,
    ) -> impl Future<Output = ToolOutput> + Send {
        self.1
            .lock()
            .unwrap()
            .push(format!("{} {}", skill.name, file.unwrap_or("SKILL.md")));
        std::future::ready(ToolOutput::ok("Served instructions."))
    }
}

fn call(id: &str, name: &str, arguments: Value) -> Part {
    Part::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    })
}

fn writing() -> Vec<Skill> {
    vec![Skill::inline(
        "writing",
        "How to word a message to the person.",
        "Write short sentences. Never use a dash.",
    )]
}

async fn run(
    script: Vec<Vec<Part>>,
    skills: &[Skill],
) -> (
    Result<TurnOutcome, newt_core::turn::TurnFailure>,
    Scripted,
    Seeing,
) {
    let model = Scripted {
        script: Mutex::new(script.into()),
        ..Scripted::default()
    };
    let host = Seeing::default();
    let send = ToolDef::new(
        ToolSpec {
            name: "send_message".into(),
            description: "Send.".into(),
            input_schema: json!({ "type": "object" }),
        },
        Retention::Outward,
    );
    let workspace = Workspace::new(BTreeMap::from([(
        "profile.md".to_owned(),
        "Remote only".to_owned(),
    )]));
    let outcome = run_turn(
        &TurnDeps {
            model: &model,
            host: &host,
            steering: &NoSteering,
            journal: &NoJournal,
            history: &newt_core::commit::Retained,
            sink: &NoSink,
            cancel: &CancellationToken::new(),
        },
        TurnInput {
            session: "customer-1",
            cache_key: "customer-1",
            turn: "t1",
            system: "You help one customer.",
            external_tools: &[send],
            history: &[],
            context: None,
            skills,
            events: vec![Message::user_text("Any news?")],
            workspace: &workspace,
            replay: Vec::new(),
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
    .await;
    (outcome, model, host)
}

#[tokio::test]
async fn tools_see_which_skills_were_loaded_and_which_pages_were_read() {
    let (outcome, model, host) = run(
        vec![
            vec![
                call("s1", "load_skill", json!({ "name": "writing" })),
                call("r1", "read_file", json!({ "paths": ["profile.md"] })),
            ],
            vec![call(
                "m1",
                "send_message",
                json!({ "text": "Nothing new yet." }),
            )],
            vec![call(
                "e1",
                "end_turn",
                json!({ "nothing_new": "status check" }),
            )],
        ],
        &writing(),
    )
    .await;
    let committed = format!("{:?}", outcome.unwrap().committed);
    let seen = host.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].skills_loaded, vec!["writing"]);
    assert_eq!(
        seen[0].pages_read,
        vec![PageRead {
            path: "profile.md".into(),
            version: "0".into(),
        }]
    );
    assert_eq!(seen[0].idempotency_key, "customer-1:t1:m1");
    assert!(model.systems.lock().unwrap()[0].contains("- writing: How to word a message"));
    assert!(model.tools.lock().unwrap()[0].contains(&"load_skill".to_owned()));
    assert!(
        !committed.contains("Never use a dash"),
        "skill text never reaches history"
    );
}

#[tokio::test]
async fn an_unknown_skill_is_an_error_and_no_skills_means_no_tool() {
    let (outcome, model, _) = run(
        vec![
            vec![call("s1", "load_skill", json!({ "name": "nope" }))],
            vec![call("e1", "end_turn", json!({ "nothing_new": "x" }))],
        ],
        &writing(),
    )
    .await;
    assert!(outcome.is_ok());
    assert_eq!(
        *model.errors.lock().unwrap(),
        vec!["There is no skill named `nope`."]
    );
    let (_, model, _) = run(
        vec![vec![call("e1", "end_turn", json!({ "nothing_new": "x" }))]],
        &[],
    )
    .await;
    assert!(!model.tools.lock().unwrap()[0].contains(&"load_skill".to_owned()));
}

#[tokio::test]
async fn a_served_skill_and_its_files_load_through_the_host() {
    let mut skills = writing();
    skills.push(Skill {
        served_by: Some("app".into()),
        ..Skill::inline("app/refunds", "How to handle a refund.", "")
    });
    let (outcome, model, host) = run(
        vec![
            vec![
                call("s1", "load_skill", json!({ "name": "app/refunds" })),
                call(
                    "s2",
                    "load_skill",
                    json!({ "name": "app/refunds", "file": "references/policy.md" }),
                ),
                call(
                    "s3",
                    "load_skill",
                    json!({ "name": "writing", "file": "references/x.md" }),
                ),
            ],
            vec![call("m1", "send_message", json!({ "text": "Refunded." }))],
            vec![call("e1", "end_turn", json!({ "nothing_new": "x" }))],
        ],
        &skills,
    )
    .await;
    assert!(outcome.is_ok());
    assert_eq!(
        *host.1.lock().unwrap(),
        vec!["app/refunds SKILL.md", "app/refunds references/policy.md"]
    );
    assert_eq!(host.0.lock().unwrap()[0].skills_loaded, vec!["app/refunds"]);
    let mut errors = model.errors.lock().unwrap().clone();
    errors.dedup();
    assert_eq!(
        errors,
        vec!["The skill `writing` has no files besides itself."]
    );
    assert!(model.systems.lock().unwrap()[0].contains("- app/refunds: How to handle a refund."));
}
