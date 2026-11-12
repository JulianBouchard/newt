#![expect(
    clippy::unwrap_used,
    clippy::unused_async_trait_impl,
    reason = "tests fail loudly on setup errors; the test server's handlers read like real async ones"
)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newt_core::journal::NoJournal;
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Lane, ModelRequest};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::NoSink;
use newt_core::steering::NoSteering;
use newt_core::tools::{CallContext, PageRead, Retention, ToolHost};
use newt_core::turn::{TurnDeps, TurnInput, TurnSettings, run_turn};
use newt_core::workspace::{Page, PageFuture, PageKey, PageSource, Workspace};
use newt_server::mcp::{CallLimits, McpAuth, McpServer, Toolset};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    ListToolsResult, MetaObject, PaginatedRequestParams, ProgressNotificationParam,
    ServerCapabilities, ServerConfig, SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext, SubscriptionSink};
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
    listeners: Mutex<Vec<SubscriptionSink>>,
    cancelled: AtomicUsize,
}

#[derive(Clone)]
struct Server {
    calls: Arc<Calls>,
    outward_idempotent: bool,
    announces_changes: bool,
}

impl Server {
    fn new(calls: &Arc<Calls>) -> Self {
        Self {
            calls: Arc::clone(calls),
            outward_idempotent: true,
            announces_changes: true,
        }
    }
}

async fn report_progress(context: &RequestContext<RoleServer>, every: Duration, times: u32) {
    let token = context.meta.get_progress_token().unwrap();
    for step in 0..times {
        tokio::time::sleep(every).await;
        context
            .peer
            .notify_progress(ProgressNotificationParam::new(
                token.clone(),
                f64::from(step),
            ))
            .await
            .unwrap();
    }
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
        let capabilities = ServerCapabilities::builder().enable_tools();
        ServerConfig::new(if self.announces_changes {
            capabilities.enable_tool_list_changed().build()
        } else {
            capabilities.build()
        })
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        self.announces_changes.then(|| requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        self.calls
            .listeners
            .lock()
            .unwrap()
            .push(context.sink().clone());
        context.cancelled().await;
        Ok(())
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
            Tool::new("bad_meta", "Returns broken newt metadata.", schema()),
            Tool::new("change_tools", "Announces a new tool list.", schema()),
            Tool::new("slow_search", "Reports progress, then answers.", schema()),
            Tool::new("silent_search", "Answers late without progress.", schema()),
            Tool::new("endless_search", "Reports progress forever.", schema()),
            Tool::new("wait_forever", "Answers only when cancelled.", schema()),
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
            "change_tools" => {
                let listeners = self.calls.listeners.lock().unwrap().clone();
                for listener in listeners {
                    listener.notify_tool_list_changed().await.unwrap();
                }
                CallToolResult::success(vec![ContentBlock::text("changed")])
            }
            "slow_search" => {
                report_progress(&context, Duration::from_millis(100), 6).await;
                CallToolResult::success(vec![ContentBlock::text("found 3")])
            }
            "wait_forever" => {
                context.ct.cancelled().await;
                self.calls.cancelled.fetch_add(1, Ordering::SeqCst);
                CallToolResult::success(vec![ContentBlock::text("cancelled")])
            }
            "silent_search" => {
                tokio::time::sleep(Duration::from_millis(600)).await;
                CallToolResult::success(vec![ContentBlock::text("too late")])
            }
            "endless_search" => {
                report_progress(&context, Duration::from_millis(50), 100).await;
                CallToolResult::success(vec![ContentBlock::text("never")])
            }
            "bad_meta" => CallToolResult::success(vec![ContentBlock::text("done")]).with_meta(
                Some(meta(json!({ "newt/pages": [{ "key": [], "path": "", "version": "1", "content": "" }] }))),
            ),
            _ => CallToolResult::success(vec![ContentBlock::text("plain")]),
        };
        Ok(result.into())
    }
}

async fn spawn_server(server: Server) -> String {
    spawn_stoppable(server).await.0
}

async fn spawn_stoppable(server: Server) -> (String, CancellationToken) {
    let stop = CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = stop.clone();
    let service: StreamableHttpService<Server, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Arc::default(), config);
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = stop.clone();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    (format!("http://{address}/mcp"), stop)
}

async fn connect(calls: &Arc<Calls>, outward_idempotent: bool) -> Result<Toolset, String> {
    connect_to(Server {
        outward_idempotent,
        ..Server::new(calls)
    })
    .await
}

async fn connect_to(server: Server) -> Result<Toolset, String> {
    let url = spawn_server(server).await;
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

fn key(idempotency_key: &str) -> CallContext {
    CallContext {
        idempotency_key: idempotency_key.into(),
        ..CallContext::default()
    }
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
            &CallContext {
                idempotency_key: "customer-7:t1:c1".into(),
                skills_loaded: vec!["writing".into()],
                pages_read: vec![PageRead {
                    path: "profile.md".into(),
                    version: "3".into(),
                }],
                traceparent: None,
            },
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
    assert_eq!(seen[0].1["newt/skills_loaded"], json!(["writing"]));
    assert_eq!(
        seen[0].1["newt/pages_read"],
        json!([{ "path": "profile.md", "version": "3" }])
    );

    let failed = host
        .call(&call("c2", "lookup_role", json!({})), &key("k2"))
        .await;
    assert!(failed.is_error);
    let not_an_object = host
        .call(&call("c3", "plain_tool", json!([1])), &key("k3"))
        .await;
    assert!(not_an_object.is_error);
    let unknown = host.call(&call("c4", "nope", json!({})), &key("k4")).await;
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
            cache_key: "customer-7",
            turn: "t1",
            system: "You help one customer.",
            external_tools: &toolset.tools,
            history: &[],
            context: None,
            skills: &[],
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
            compaction: None,
        },
    )
    .await
    .unwrap();

    let traceparent = calls.seen.lock().unwrap()[0].1["traceparent"].clone();
    assert!(
        traceparent
            .as_str()
            .is_some_and(|value| value.len() == 55 && value.starts_with("00-")),
        "{traceparent}"
    );
    assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    assert_eq!(
        workspace.read("profile.md").await.unwrap().as_deref(),
        Some("Remote or Berlin")
    );
    let committed = format!("{:?}", outcome.committed);
    assert!(committed.contains("Saved: Remote or Berlin"), "{committed}");
    assert!(!committed.contains("--- profile.md ---"), "{committed}");
}

#[tokio::test]
async fn invalid_result_metadata_is_loud_and_marks_every_page_stale() {
    let calls = Arc::new(Calls::default());
    let toolset = connect(&calls, true).await.unwrap();
    let output = toolset
        .host("customer-7")
        .call(&call("c1", "bad_meta", json!({})), &key("k1"))
        .await;
    assert_eq!(output.pages, Vec::<Page>::new());
    assert_eq!(output.invalidate, vec![Vec::<String>::new()]);
    assert!(
        output.content.contains("every page will be refetched"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn a_server_url_that_is_not_http_is_refused() {
    let error = Toolset::connect(
        &[McpServer {
            name: "app".into(),
            url: "ftp://example.test/mcp".into(),
            headers: BTreeMap::new(),
            auth: McpAuth::AgentToken,
        }],
        "agent-token",
    )
    .await
    .unwrap_err();
    assert!(error.contains("http"), "{error}");
}

#[test]
fn header_values_never_appear_in_debug_output() {
    let server = McpServer {
        name: "app".into(),
        url: "https://example.test/mcp".into(),
        headers: BTreeMap::from([("x-api-key".to_owned(), "super-secret-value".to_owned())]),
        auth: McpAuth::None,
    };
    let printed = format!("{server:?}");
    assert!(printed.contains("x-api-key"));
    assert!(!printed.contains("super-secret-value"));
}

#[tokio::test]
async fn a_lost_connection_marks_the_toolset_broken_and_says_the_outcome_is_unknown() {
    let calls = Arc::new(Calls::default());
    let (url, stop) = spawn_stoppable(Server::new(&calls)).await;
    let toolset = Toolset::connect(
        &[McpServer {
            name: "app".into(),
            url,
            headers: BTreeMap::new(),
            auth: McpAuth::None,
        }],
        "agent-token",
    )
    .await
    .unwrap();
    stop.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let output = toolset
        .host("customer-7")
        .call(
            &call("c1", "send_message", json!({ "text": "Hi" })),
            &key("k1"),
        )
        .await;
    assert!(output.is_error);
    assert!(
        output.content.contains("Do not repeat it"),
        "{}",
        output.content
    );
    assert!(toolset.is_stale());
}

#[tokio::test]
async fn tools_are_ordered_by_name_whatever_order_the_server_lists_them_in() {
    let calls = Arc::new(Calls::default());
    let toolset = connect(&calls, true).await.unwrap();
    let names: Vec<&str> = toolset
        .tools
        .iter()
        .map(|tool| tool.spec.name.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    assert_eq!(names.first(), Some(&"bad_meta"));
}

async fn eventually(check: impl Fn() -> bool) -> bool {
    for _ in 0..50 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check()
}

#[tokio::test]
async fn a_server_announcing_new_tools_marks_the_toolset_stale() {
    let calls = Arc::new(Calls::default());
    let toolset = connect(&calls, true).await.unwrap();
    assert_eq!(calls.listeners.lock().unwrap().len(), 1);
    assert!(!toolset.is_stale());
    let output = toolset
        .host("customer-7")
        .call(&call("c1", "change_tools", json!({})), &key("k1"))
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(eventually(|| toolset.is_stale()).await);
}

#[tokio::test]
async fn a_server_without_change_notifications_still_connects_and_stays_fresh() {
    let calls = Arc::new(Calls::default());
    let toolset = connect_to(Server {
        announces_changes: false,
        ..Server::new(&calls)
    })
    .await
    .unwrap();
    let output = toolset
        .host("customer-7")
        .call(&call("c1", "plain_tool", json!({})), &key("k1"))
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(calls.listeners.lock().unwrap().is_empty());
    assert!(!toolset.is_stale());
}

fn tight_limits(toolset: Toolset) -> Toolset {
    toolset.with_call_limits(CallLimits {
        quiet: Duration::from_millis(300),
        total: Duration::from_millis(1500),
    })
}

#[tokio::test]
async fn progress_keeps_a_long_call_alive_past_the_quiet_limit() {
    let calls = Arc::new(Calls::default());
    let toolset = tight_limits(connect(&calls, true).await.unwrap());
    let output = toolset
        .host("customer-7")
        .call(&call("c1", "slow_search", json!({})), &key("k1"))
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(output.content, "found 3");
}

#[tokio::test]
async fn a_call_that_goes_quiet_times_out_and_says_the_outcome_is_unknown() {
    let calls = Arc::new(Calls::default());
    let toolset = tight_limits(connect(&calls, true).await.unwrap());
    let output = toolset
        .host("customer-7")
        .call(&call("c1", "silent_search", json!({})), &key("k1"))
        .await;
    assert!(output.is_error);
    assert!(
        output.content.contains("no result or progress"),
        "{}",
        output.content
    );
    assert!(output.content.contains("Do not repeat it"));
}

#[tokio::test]
async fn progress_never_extends_a_call_past_the_total_limit() {
    let calls = Arc::new(Calls::default());
    let toolset = tight_limits(connect(&calls, true).await.unwrap());
    let started = std::time::Instant::now();
    let output = toolset
        .host("customer-7")
        .call(&call("c1", "endless_search", json!({})), &key("k1"))
        .await;
    assert!(output.is_error);
    assert!(
        output.content.contains("did not finish within"),
        "{}",
        output.content
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn many_sessions_call_one_server_at_once_without_queueing() {
    let calls = Arc::new(Calls::default());
    let toolset = connect(&calls, true).await.unwrap();
    let started = std::time::Instant::now();
    let outputs = futures_util::future::join_all((0..48).map(|index| {
        let host = toolset.host(&format!("customer-{index}"));
        async move {
            host.call(
                &call("c1", "silent_search", json!({})),
                &key(&format!("k{index}")),
            )
            .await
        }
    }))
    .await;
    assert!(outputs.iter().all(|output| !output.is_error));
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "48 calls of a 600 ms tool took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_abandoned_call_is_cancelled_on_the_server() {
    let calls = Arc::new(Calls::default());
    let toolset = connect(&calls, true).await.unwrap();
    let host = toolset.host("customer-7");
    let abandoned = tokio::time::timeout(
        Duration::from_millis(200),
        host.call(&call("c1", "wait_forever", json!({})), &key("k1")),
    )
    .await;
    assert!(abandoned.is_err());
    assert!(eventually(|| calls.cancelled.load(Ordering::SeqCst) == 1).await);
}

#[tokio::test]
async fn a_call_carries_its_traceparent_to_the_server() {
    let calls = Arc::new(Calls::default());
    let toolset = connect(&calls, true).await.unwrap();
    let traceparent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
    toolset
        .host("customer-7")
        .call(
            &call("c1", "plain_tool", json!({})),
            &CallContext {
                traceparent: Some(traceparent.into()),
                ..key("k1")
            },
        )
        .await;
    let seen = calls.seen.lock().unwrap().clone();
    assert_eq!(seen[0].1["traceparent"], traceparent);
}
