#![expect(
    clippy::unwrap_used,
    clippy::unused_async_trait_impl,
    reason = "tests fail loudly on setup errors; the test server's handlers read like real async ones"
)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use newt_core::journal::NoJournal;
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Lane, ModelRequest};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::NoSink;
use newt_core::steering::NoSteering;
use newt_core::tools::{Retention, ToolHost};
use newt_core::turn::{TurnDeps, TurnInput, TurnSettings, run_turn};
use newt_core::workspace::{Page, PageFuture, PageKey, PageSource, Workspace};
use newt_server::mcp::{McpAuth, McpServer, Toolset};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Calls {
    seen: Mutex<Vec<(String, Value)>>,
    authorization: Mutex<Vec<Option<String>>>,
    profile: Mutex<String>,
}

#[derive(Clone)]
struct Server {
    calls: Arc<Calls>,
    outward_idempotent: bool,
}

fn meta(value: Value) -> MetaObject {
    let mut meta = MetaObject::new();
    if let Value::Object(fields) = value {
        for (key, field) in fields {
            meta.insert(key, field);
        }
    }
    meta
}

fn schema() -> Arc<JsonObject> {
    let Value::Object(schema) = json!({ "type": "object" }) else {
        unreachable!()
    };
    Arc::new(schema)
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let authorization = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.headers.get("authorization"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.calls.authorization.lock().unwrap().push(authorization);
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new("lookup_role", "Look up a role.", schema())
                .with_meta(meta(json!({ "newt/retention": "ephemeral" }))),
            Tool::new("save_preference", "Save a preference.", schema()).with_meta(meta(
                json!({ "newt/retention": "durable", "newt/invalidates": [["customer"]] }),
            )),
            Tool::new("send_message", "Send a message.", schema()).with_meta(meta(json!({
                "newt/retention": "outward",
                "newt/idempotent": self.outward_idempotent,
            }))),
            Tool::new(
                "plain_tool",
                "A tool that knows nothing about newt.",
                schema(),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let request_meta = Value::Object((**context.meta).clone());
        self.calls
            .seen
            .lock()
            .unwrap()
            .push((request.name.to_string(), request_meta));
        let text = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let result = match request.name.as_ref() {
            "save_preference" => {
                text.clone_into(&mut self.calls.profile.lock().unwrap());
                CallToolResult::success(vec![ContentBlock::text("{\"saved\":true}")]).with_meta(
                    Some(meta(json!({ "newt/summary": format!("Saved: {text}") }))),
                )
            }
            "send_message" => CallToolResult::success(vec![ContentBlock::text("sent")]).with_meta(
                Some(meta(json!({
                    "newt/summary": "Sent",
                    "newt/pages": [{
                        "key": ["conversation"],
                        "path": "conversation.md",
                        "version": "2",
                        "content": format!("You: {text}"),
                    }],
                }))),
            ),
            "lookup_role" => CallToolResult::error(vec![ContentBlock::text("no such role")]),
            _ => CallToolResult::success(vec![ContentBlock::text("plain")]),
        };
        Ok(result.into())
    }
}

async fn spawn_server(server: Server) -> String {
    let service: StreamableHttpService<Server, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::default(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}/mcp")
}

async fn connect(calls: &Arc<Calls>, outward_idempotent: bool) -> Result<Toolset, String> {
    let url = spawn_server(Server {
        calls: Arc::clone(calls),
        outward_idempotent,
    })
    .await;
    Toolset::connect(
        &[McpServer {
            name: "app".into(),
            url,
            headers: BTreeMap::new(),
            auth: McpAuth::AgentToken,
        }],
        "agent-token",
    )
    .await
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

#[tokio::test]
async fn tools_carry_their_retention_and_invalidations_from_meta() {
    let calls = Arc::new(Calls::default());
    let toolset = connect(&calls, true).await.unwrap();
    let retention: BTreeMap<&str, (Retention, Vec<PageKey>)> = toolset
        .tools
        .iter()
        .map(|tool| {
            (
                tool.spec.name.as_str(),
                (tool.retention, tool.invalidates.clone()),
            )
        })
        .collect();
    assert_eq!(retention["lookup_role"].0, Retention::Ephemeral);
    assert_eq!(
        retention["save_preference"],
        (Retention::Durable, vec![vec!["customer".to_owned()]])
    );
    assert_eq!(retention["send_message"].0, Retention::Outward);
    assert_eq!(retention["plain_tool"], (Retention::Durable, Vec::new()));
    assert_eq!(
        *calls.authorization.lock().unwrap(),
        vec![Some("Bearer agent-token".to_owned())]
    );
}

#[tokio::test]
async fn an_outward_tool_without_idempotency_is_refused() {
    let calls = Arc::new(Calls::default());
    let error = connect(&calls, false).await.unwrap_err();
    assert!(error.contains("send_message"), "{error}");
    assert!(error.contains("newt/idempotent"), "{error}");
}

#[tokio::test]
async fn a_call_sends_the_session_and_idempotency_key_and_reads_result_meta() {
    let calls = Arc::new(Calls::default());
    let toolset = connect(&calls, true).await.unwrap();
    let host = toolset.host("customer-7");

    let output = host
        .call(
            &call("c1", "send_message", json!({ "text": "Hi Maria" })),
            "customer-7:t1:c1",
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(output.content, "sent");
    assert_eq!(output.summary.as_deref(), Some("Sent"));
    assert_eq!(
        output.pages,
        vec![Page {
            key: vec!["conversation".into()],
            path: "conversation.md".into(),
            version: "2".into(),
            content: "You: Hi Maria".into(),
        }]
    );
    let seen = calls.seen.lock().unwrap().clone();
    assert_eq!(seen[0].1["newt/session"], "customer-7");
    assert_eq!(seen[0].1["newt/idempotency_key"], "customer-7:t1:c1");

    let failed = host.call(&call("c2", "lookup_role", json!({})), "k2").await;
    assert!(failed.is_error);
    let not_an_object = host.call(&call("c3", "plain_tool", json!([1])), "k3").await;
    assert!(not_an_object.is_error);
    let unknown = host.call(&call("c4", "nope", json!({})), "k4").await;
    assert!(unknown.is_error);
}

struct ProfileSource {
    calls: Arc<Calls>,
    fetches: AtomicUsize,
}

impl PageSource for ProfileSource {
    fn fetch(&self, _keys: Vec<PageKey>) -> PageFuture<'_> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let content = self.calls.profile.lock().unwrap().clone();
        Box::pin(async move {
            Ok(vec![Page {
                key: vec!["customer".into(), "profile".into()],
                path: "profile.md".into(),
                version: "2".into(),
                content,
            }])
        })
    }
}

struct Scripted(Mutex<VecDeque<Message>>);

impl Model for Scripted {
    fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        let next = self.0.lock().unwrap().pop_front();
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

fn assistant(id: &str, name: &str, arguments: Value) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(call(id, name, arguments))],
    }
}

#[tokio::test]
async fn an_mcp_write_invalidates_a_page_and_the_next_read_refetches_it() {
    let calls = Arc::new(Calls::default());
    "Remote only".clone_into(&mut calls.profile.lock().unwrap());
    let toolset = connect(&calls, true).await.unwrap();
    let source = Arc::new(ProfileSource {
        calls: Arc::clone(&calls),
        fetches: AtomicUsize::new(0),
    });
    let workspace = Workspace::with_source(
        vec![Page {
            key: vec!["customer".into(), "profile".into()],
            path: "profile.md".into(),
            version: "1".into(),
            content: "Remote only".into(),
        }],
        Arc::clone(&source) as Arc<dyn PageSource>,
    );
    let model = Scripted(Mutex::new(
        vec![
            assistant(
                "w1",
                "save_preference",
                json!({ "text": "Remote or Berlin" }),
            ),
            assistant("r1", "read_file", json!({ "paths": ["profile.md"] })),
            assistant("e1", "end_turn", json!({ "recorded": ["Berlin is fine"] })),
        ]
        .into(),
    ));
    let host = toolset.host("customer-7");
    let outcome = run_turn(
        &TurnDeps {
            model: &model,
            host: &host,
            steering: &NoSteering,
            journal: &NoJournal,
            sink: &NoSink,
            cancel: &CancellationToken::new(),
        },
        TurnInput {
            session: "customer-7",
            turn: "t1",
            system: "You help one customer.",
            external_tools: &toolset.tools,
            history: &[],
            events: vec![Message::user_text("Berlin works too.")],
            workspace: &workspace,
            replay: Vec::new(),
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
    .unwrap();

    assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    assert_eq!(
        workspace.read("profile.md").await.unwrap().as_deref(),
        Some("Remote or Berlin")
    );
    let committed = format!("{:?}", outcome.committed);
    assert!(committed.contains("Saved: Remote or Berlin"), "{committed}");
    assert!(!committed.contains("--- profile.md ---"), "{committed}");
}
