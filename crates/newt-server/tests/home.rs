#![expect(
    clippy::unwrap_used,
    clippy::unused_async_trait_impl,
    reason = "tests fail loudly on setup errors; the test server's handlers read like real async ones"
)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newt_core::journal::TurnChoice;
use newt_core::message::ToolCall;
use newt_core::prompt::Effort;
use newt_core::tools::{CallContext, Retention, ToolHost};
use newt_server::backend::{Backend, Prepared, TurnEvent, TurnRequest};
use newt_server::home::{HomeBackend, HomeServer};
use newt_server::mcp::McpHost;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CustomRequest,
    CustomResult, JsonObject, ListResourcesResult, ListToolsResult, MetaObject,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    RequestMetaObject, Resource, ResourceContents, ServerCapabilities, ServerConfig,
    SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext, SubscriptionSink};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};

const INSTRUCTIONS: &str = "You help one person keep their tasks.";

/// What the server saw, and the `_meta` it came with.
type Seen = Arc<Mutex<Vec<(String, Option<RequestMetaObject>)>>>;

/// A test MCP server: the home when it declares settings in the newt
/// extension, an added server otherwise.
#[derive(Clone, Default)]
struct App {
    instructions: Option<String>,
    /// `None`: no newt extension. `Some({})`: speaks newt, no settings.
    newt: Arc<Mutex<Option<Value>>>,
    pages: Arc<Mutex<HashMap<String, String>>>,
    tools: Vec<(&'static str, Value)>,
    listeners: Arc<Mutex<Vec<SubscriptionSink>>>,
    ended: Arc<AtomicUsize>,
    turn_answer: Arc<Mutex<Value>>,
    turn_calls: Arc<Mutex<Vec<Value>>>,
    turn_ends: Arc<Mutex<Vec<Value>>>,
    refuse_turn_end: bool,
    seen_meta: Seen,
    read_patch: Option<Value>,
    result_meta: Option<Value>,
    listings: Arc<AtomicUsize>,
}

impl App {
    fn home() -> Self {
        let app = Self {
            instructions: Some(INSTRUCTIONS.into()),
            newt: Arc::new(Mutex::new(Some(json!({
                "model": "default",
                "max_model_calls": 6,
            })))),
            tools: vec![(
                "complete_task",
                json!({ "newt/retention": "durable", "newt/invalidates": [["tasks"]] }),
            )],
            ..Self::default()
        };
        app.set("app://ana/tasks.md", "- [ ] Call the bank\n");
        app.set("app://ana/notes.md", "Lives in Lisbon\n");
        app.set("app://ben/tasks.md", "- [ ] Book flights\n");
        app
    }

    /// A server that knows nothing about newt, with a tool whose `newt/*`
    /// metadata newt must ignore and one page for everyone.
    fn plain() -> Self {
        let app = Self {
            tools: vec![("search_jobs", json!({ "newt/retention": "ephemeral" }))],
            result_meta: Some(json!({
                "newt/summary": "should be ignored",
                "newt/pages": [{ "key": ["x"], "path": "tasks.md", "version": "1", "content": "hijacked" }],
            })),
            ..Self::default()
        };
        app.set("jobs://acme-sre", "# Acme SRE\nRemote.\n");
        app
    }

    fn settings(&self, settings: Value) {
        *self.newt.lock().unwrap() = Some(settings);
    }

    fn set(&self, uri: &str, text: &str) {
        self.pages
            .lock()
            .unwrap()
            .insert(uri.to_owned(), text.to_owned());
    }

    async fn announce(&self, uri: &str) {
        let listeners = self.listeners.lock().unwrap().clone();
        let watching = listeners.iter().filter(|listener| {
            listener
                .accepted()
                .resource_subscriptions
                .as_ref()
                .is_some_and(|uris| uris.iter().any(|accepted| accepted == uri))
        });
        for listener in watching {
            listener.notify_resource_updated(uri).await.unwrap();
        }
    }

    fn seen(&self, what: &str, meta: &RequestMetaObject) {
        let meta = (!meta.is_empty()).then(|| meta.clone());
        self.seen_meta.lock().unwrap().push((what.to_owned(), meta));
    }
}

fn session(meta: &RequestMetaObject) -> String {
    meta.get("newt/session")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned()
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(fields) => fields,
        _ => JsonObject::new(),
    }
}

impl ServerHandler for App {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .enable_resources()
            .enable_resources_subscribe()
            .build();
        if let Some(newt) = self.newt.lock().unwrap().clone() {
            capabilities.extensions = Some(BTreeMap::from([("newt".to_owned(), object(newt))]));
        }
        let config = ServerConfig::new(capabilities);
        match &self.instructions {
            Some(text) => config.with_instructions(text),
            None => config,
        }
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        let watches_pages = context
            .sink()
            .accepted()
            .resource_subscriptions
            .as_ref()
            .is_some_and(|uris| !uris.is_empty());
        self.listeners.lock().unwrap().push(context.sink().clone());
        context.cancelled().await;
        if watches_pages {
            self.ended.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.listings.fetch_add(1, Ordering::SeqCst);
        let tools = self
            .tools
            .iter()
            .map(|(name, meta)| {
                let mut tool = Tool::new(
                    *name,
                    "A tool.",
                    Arc::new(object(json!({ "type": "object" }))),
                );
                let mut fields = MetaObject::new();
                for (key, value) in object(meta.clone()) {
                    fields.insert(key, value);
                }
                tool.meta = Some(fields);
                tool
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.seen(&format!("call {}", request.name), &context.meta);
        let mut result = CallToolResult::success(vec![ContentBlock::text("done")]);
        if let Some(meta) = &self.result_meta {
            let mut fields = MetaObject::new();
            for (key, value) in object(meta.clone()) {
                fields.insert(key, value);
            }
            result.meta = Some(fields);
        }
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        self.seen("list", &context.meta);
        let session = session(&context.meta);
        let prefix = if session.is_empty() {
            String::new()
        } else {
            format!("app://{session}/")
        };
        let mut uris: Vec<String> = self
            .pages
            .lock()
            .unwrap()
            .keys()
            .filter(|uri| {
                if prefix.is_empty() {
                    !uri.starts_with("app://")
                } else {
                    uri.starts_with(&prefix)
                }
            })
            .cloned()
            .collect();
        uris.sort();
        let resources: Vec<Resource> = uris
            .iter()
            .map(|uri| {
                let name = uri.trim_start_matches(&prefix).replace("jobs://", "jobs/")
                    + if prefix.is_empty() { ".md" } else { "" };
                Resource::new(uri, name)
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.seen("read", &context.meta);
        let text = self
            .pages
            .lock()
            .unwrap()
            .get(&request.uri)
            .cloned()
            .ok_or_else(|| ErrorData::invalid_params("no such page", None))?;
        let mut result = ReadResourceResult::new(vec![ResourceContents::text(text, request.uri)]);
        if let Some(patch) = &self.read_patch {
            let mut meta = MetaObject::new();
            meta.insert("newt/state_patch".to_owned(), patch.clone());
            result.meta = Some(meta);
        }
        Ok(result.into())
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        let params = request.params.clone().unwrap_or(Value::Null);
        match request.method.as_str() {
            "newt/turn" => {
                self.turn_calls.lock().unwrap().push(params);
                Ok(CustomResult::new(self.turn_answer.lock().unwrap().clone()))
            }
            "newt/turn_end" => {
                if self.refuse_turn_end {
                    return Err(ErrorData::internal_error("turn end store is down", None));
                }
                self.turn_ends.lock().unwrap().push(params);
                Ok(CustomResult::new(json!({})))
            }
            other => Err(ErrorData::invalid_params(other.to_owned(), None)),
        }
    }
}

async fn serve(app: App) -> String {
    serve_stoppable(app).await.0
}

async fn serve_stoppable(app: App) -> (String, tokio_util::sync::CancellationToken) {
    let stop = tokio_util::sync::CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = stop.clone();
    let service: StreamableHttpService<App, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(app.clone()), Arc::default(), config);
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

fn backend_at(url: String) -> HomeBackend {
    HomeBackend::new(
        BTreeMap::from([(
            "assistant".to_owned(),
            HomeServer {
                url,
                token: "agent-token".into(),
            },
        )]),
        ["default".to_owned(), "strong".to_owned()],
    )
}

async fn backend(app: App) -> HomeBackend {
    backend_at(serve(app).await)
}

async fn prepare(backend: &HomeBackend, key: &str) -> Prepared<McpHost> {
    backend
        .prepare("assistant", key, &TurnRequest::default())
        .await
        .unwrap()
}

async fn prepare_error(app: App) -> String {
    backend(app)
        .await
        .prepare("assistant", "ana", &TurnRequest::default())
        .await
        .unwrap_err()
        .to_string()
}

#[tokio::test]
async fn the_home_server_is_the_agent_with_its_instructions_settings_and_pages() {
    let backend = backend(App::home()).await;
    let ana = prepare(&backend, "ana").await;
    assert_eq!(ana.system, INSTRUCTIONS);
    assert_eq!(ana.model, "default");
    assert_eq!(ana.max_model_calls, 6);
    assert_eq!(ana.max_output_tokens, 4096);
    assert!(!ana.turn_end);
    assert_eq!(ana.context, None);
    assert_eq!(
        ana.workspace.read("tasks.md").await.unwrap().as_deref(),
        Some("- [ ] Call the bank\n")
    );
    assert_eq!(ana.workspace.paths(), ["notes.md", "tasks.md"]);
    let ben = prepare(&backend, "ben").await;
    ben.workspace.read("tasks.md").await.unwrap();
    assert_eq!(ben.workspace.paths(), ["tasks.md"]);
}

#[tokio::test]
async fn a_home_that_does_not_declare_the_agent_fails_loudly() {
    let mut no_extension = App::home();
    no_extension.newt = Arc::new(Mutex::new(None));
    let error = prepare_error(no_extension).await;
    assert!(
        error.contains("does not declare the `newt` extension"),
        "{error}"
    );

    let unknown_model = App::home();
    unknown_model.settings(json!({ "model": "other" }));
    let error = prepare_error(unknown_model).await;
    assert!(error.contains("model `other`"), "{error}");

    let unknown_field = App::home();
    unknown_field.settings(json!({ "model": "default", "version": "1" }));
    let error = prepare_error(unknown_field).await;
    assert!(error.contains("unknown field `version`"), "{error}");

    let no_instructions = App {
        instructions: None,
        ..App::home()
    };
    let error = prepare_error(no_instructions).await;
    assert!(error.contains("no instructions"), "{error}");

    let error = backend_at("stdio:true".into())
        .prepare("assistant", "ana", &TurnRequest::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("MCP server `assistant`"), "{error}");
}

#[tokio::test]
async fn an_unknown_agent_fails_loudly() {
    let error = backend(App::home())
        .await
        .prepare("nobody", "ana", &TurnRequest::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("agent `nobody` is not in server.toml"),
        "{error}"
    );
}

#[tokio::test]
async fn newt_turn_shapes_the_turn_and_hears_its_events_and_state() {
    let app = App::home();
    app.settings(json!({ "model": "default", "turn": true }));
    *app.turn_answer.lock().unwrap() = json!({
        "pinned": "Write for WhatsApp.",
        "instructions": "It is Friday.",
        "constraints": ["  No messages before Monday.  ", ""],
        "state": { "seen": 2 },
        "turn_state": { "channel": "whatsapp" },
        "model": "strong",
        "effort": "high",
    });
    let backend = backend(app.clone()).await;
    let request = TurnRequest {
        events: vec![TurnEvent {
            id: "m1".into(),
            meta: Some(json!({ "channel": "whatsapp" })),
        }],
        state: Some(json!({ "seen": 1 })),
    };
    let prepared = backend.prepare("assistant", "ana", &request).await.unwrap();
    assert_eq!(
        *app.turn_calls.lock().unwrap(),
        vec![json!({
            "session": "ana",
            "events": [{ "id": "m1", "meta": { "channel": "whatsapp" } }],
            "state": { "seen": 1 },
        })]
    );
    assert_eq!(
        prepared.system,
        format!("{INSTRUCTIONS}\n\n<pinned>\nWrite for WhatsApp.\n</pinned>")
    );
    assert_eq!(
        prepared.context.as_deref(),
        Some(
            "<turn_context>\n<instructions>\nIt is Friday.\n</instructions>\n<active_constraints>\n- No messages before Monday.\n</active_constraints>\n</turn_context>"
        )
    );
    assert_eq!(prepared.state.changed(), Some(json!({ "seen": 2 })));
    assert_eq!(prepared.turn_state.get(), json!({ "channel": "whatsapp" }));
    assert_eq!(
        prepared.model, "default",
        "the settings' model stays the fallback"
    );
    assert_eq!(
        prepared.choice,
        Some(TurnChoice {
            model: "strong".into(),
            first_call_effort: None,
            effort: Some(Effort::High),
            reset: false,
        })
    );
}

#[tokio::test]
async fn a_home_that_does_not_declare_turn_is_never_asked() {
    let app = App::home();
    *app.turn_answer.lock().unwrap() = json!({ "instructions": "never seen" });
    let prepared = prepare(&backend(app.clone()).await, "ana").await;
    assert!(app.turn_calls.lock().unwrap().is_empty());
    assert_eq!(prepared.context, None);
    assert_eq!(prepared.choice, None);
}

#[tokio::test]
async fn newt_turn_naming_an_unknown_model_or_field_fails_the_turn() {
    let app = App::home();
    app.settings(json!({ "model": "default", "turn": true }));
    *app.turn_answer.lock().unwrap() = json!({ "model": "other" });
    assert!(
        prepare_error(app)
            .await
            .contains("newt/turn names model `other`")
    );

    let app = App::home();
    app.settings(json!({ "model": "default", "turn": true }));
    *app.turn_answer.lock().unwrap() = json!({ "version": "1" });
    let error = prepare_error(app).await;
    assert!(error.contains("newt/turn: invalid result"), "{error}");
}

#[tokio::test]
async fn a_turn_end_reaches_the_home_and_a_refusal_is_an_error() {
    let app = App::home();
    app.settings(json!({ "model": "default", "turn_end": true }));
    let backend = backend(app.clone()).await;
    assert!(prepare(&backend, "ana").await.turn_end);
    let payload = json!({ "session": "ana", "turn": "t1", "outcome": "completed" });
    backend.turn_end("assistant", &payload).await.unwrap();
    assert_eq!(*app.turn_ends.lock().unwrap(), vec![payload.clone()]);

    let refusing = App {
        refuse_turn_end: true,
        ..App::home()
    };
    let error = backend_at(serve(refusing).await)
        .turn_end("assistant", &payload)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("turn end store is down"), "{error}");
}

#[tokio::test]
async fn reading_a_page_sends_the_state_and_can_patch_it() {
    let app = App {
        read_patch: Some(json!({ "rendered": "tasks" })),
        ..App::home()
    };
    let backend = backend(app.clone()).await;
    let request = TurnRequest {
        events: Vec::new(),
        state: Some(json!({ "seen": 1 })),
    };
    let prepared = backend.prepare("assistant", "ana", &request).await.unwrap();
    prepared.workspace.read("tasks.md").await.unwrap();
    assert_eq!(
        prepared.state.changed(),
        Some(json!({ "seen": 1, "rendered": "tasks" }))
    );
    let seen = app.seen_meta.lock().unwrap();
    let (_, read) = seen.iter().find(|(what, _)| what == "read").unwrap();
    let read = read.as_ref().unwrap();
    assert_eq!(read["newt/session"], "ana");
    assert_eq!(read["newt/state"], json!({ "seen": 1 }));
}

#[tokio::test]
async fn a_resource_update_from_the_server_makes_the_next_read_fetch_it_again() {
    let app = App::home();
    let backend = backend(app.clone()).await;
    let ana = prepare(&backend, "ana").await;
    assert_eq!(
        ana.workspace.read("tasks.md").await.unwrap().as_deref(),
        Some("- [ ] Call the bank\n")
    );
    eventually(|| app.listeners.lock().unwrap().len() >= 2).await;

    app.set("app://ana/tasks.md", "- [x] Call the bank\n");
    app.announce("app://ana/tasks.md").await;
    eventually(|| ana.workspace.is_stale("tasks.md")).await;
    assert!(!ana.workspace.is_stale("notes.md"));
    assert_eq!(
        ana.workspace.read("tasks.md").await.unwrap().as_deref(),
        Some("- [x] Call the bank\n")
    );
}

#[tokio::test]
async fn the_subscription_ends_with_the_turns_workspace() {
    let app = App::home();
    let backend = backend(app.clone()).await;
    let ana = prepare(&backend, "ana").await;
    ana.workspace.read("tasks.md").await.unwrap();
    eventually(|| app.listeners.lock().unwrap().len() >= 2).await;
    drop(ana);
    eventually(|| app.ended.load(Ordering::SeqCst) > 0).await;
}

async fn eventually(check: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

/// A home that adds `paraform`, a plain server, and `notes`, a server that
/// speaks newt.
async fn home_with_servers() -> (HomeBackend, App, App) {
    let plain = App::plain();
    let speaking = App {
        newt: Arc::new(Mutex::new(Some(json!({})))),
        tools: vec![("add_note", json!({ "newt/retention": "ephemeral" }))],
        ..App::default()
    };
    let home = App::home();
    home.settings(json!({
        "model": "default",
        "servers": [
            { "name": "paraform", "url": serve(plain.clone()).await },
            { "name": "notes", "url": serve(speaking.clone()).await },
        ],
    }));
    (backend(home).await, plain, speaking)
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: json!({}),
    }
}

fn context() -> CallContext {
    CallContext {
        idempotency_key: "assistant/ana:t1:c1".into(),
        ..CallContext::default()
    }
}

#[tokio::test]
async fn added_servers_tools_are_prefixed_and_only_a_newt_server_decides_retention() {
    let (backend, _, _) = home_with_servers().await;
    let prepared = prepare(&backend, "ana").await;
    let tools: BTreeMap<&str, Retention> = prepared
        .tools
        .iter()
        .map(|tool| (tool.spec.name.as_str(), tool.retention))
        .collect();
    assert_eq!(
        tools,
        BTreeMap::from([
            ("complete_task", Retention::Durable),
            ("notes__add_note", Retention::Ephemeral),
            ("paraform__search_jobs", Retention::Durable),
        ])
    );
}

#[tokio::test]
async fn a_plain_server_gets_no_session_data_and_its_newt_fields_are_ignored() {
    let (backend, plain, speaking) = home_with_servers().await;
    let prepared = prepare(&backend, "ana").await;
    prepared.turn_state.set(json!({ "channel": "whatsapp" }));

    let output = prepared
        .host
        .call(&call("paraform__search_jobs"), &context())
        .await;
    assert_eq!(output.content, "done");
    assert_eq!(output.summary, None);
    assert_eq!(output.pages.len(), 0, "a plain server cannot write pages");
    let seen = plain.seen_meta.lock().unwrap().clone();
    let (what, meta) = seen
        .iter()
        .find(|(what, _)| what.starts_with("call"))
        .unwrap();
    assert_eq!(
        what, "call search_jobs",
        "the server sees its own tool name"
    );
    let meta = meta.as_ref().unwrap();
    assert_eq!(meta["newt/idempotency_key"], "assistant/ana:t1:c1");
    for private in [
        "newt/session",
        "newt/turn_state",
        "newt/state",
        "newt/pages_read",
    ] {
        assert!(
            !meta.contains_key(private),
            "{private} reached a plain server"
        );
    }

    prepared
        .host
        .call(&call("notes__add_note"), &context())
        .await;
    let seen = speaking.seen_meta.lock().unwrap().clone();
    let (_, meta) = seen
        .iter()
        .find(|(what, _)| what.starts_with("call"))
        .unwrap();
    let meta = meta.as_ref().unwrap();
    assert_eq!(meta["newt/session"], "ana");
    assert_eq!(meta["newt/turn_state"], json!({ "channel": "whatsapp" }));
}

#[tokio::test]
async fn an_added_servers_pages_live_under_its_folder() {
    let (backend, plain, _) = home_with_servers().await;
    let prepared = prepare(&backend, "ana").await;
    assert_eq!(
        prepared
            .workspace
            .read("paraform/jobs/acme-sre.md")
            .await
            .unwrap()
            .as_deref(),
        Some("# Acme SRE\nRemote.\n")
    );
    assert_eq!(
        prepared.workspace.paths(),
        ["notes.md", "paraform/jobs/acme-sre.md", "tasks.md"]
    );
    let seen = plain.seen_meta.lock().unwrap().clone();
    assert!(
        seen.iter()
            .filter(|(what, _)| what == "list" || what == "read")
            .all(|(_, meta)| meta
                .as_ref()
                .is_none_or(|meta| !meta.keys().any(|key| key.starts_with("newt/")))),
        "a plain server's resource requests carry nothing of newt's: {seen:?}"
    );
}

#[tokio::test]
async fn an_unreachable_added_server_leaves_the_agent_running_without_it() {
    let home = App::home();
    home.settings(json!({
        "model": "default",
        "servers": [{ "name": "paraform", "url": "http://127.0.0.1:9/mcp" }],
    }));
    let prepared = prepare(&backend(home).await, "ana").await;
    assert!(
        prepared
            .tools
            .iter()
            .all(|tool| !tool.spec.name.starts_with("paraform__"))
    );
    assert!(
        prepared
            .context
            .as_deref()
            .unwrap()
            .contains("<unavailable_servers>\nparaform: unreachable"),
    );
    assert_eq!(
        prepared
            .workspace
            .read("tasks.md")
            .await
            .unwrap()
            .as_deref(),
        Some("- [ ] Call the bank\n")
    );
}

#[tokio::test]
async fn an_added_server_with_a_bad_name_or_a_missing_token_fails_loudly() {
    let home = App::home();
    home.settings(json!({
        "model": "default",
        "servers": [{ "name": "Para Form", "url": "http://127.0.0.1:9/mcp" }],
    }));
    assert!(
        prepare_error(home)
            .await
            .contains("server name `Para Form`")
    );

    let home = App::home();
    home.settings(json!({
        "model": "default",
        "servers": [{
            "name": "paraform",
            "url": "http://127.0.0.1:9/mcp",
            "auth_env": "NEWT_TEST_SURELY_UNSET_SERVER_TOKEN",
        }],
    }));
    let error = prepare_error(home).await;
    assert!(
        error.contains("NEWT_TEST_SURELY_UNSET_SERVER_TOKEN is not set"),
        "{error}"
    );
}

#[test]
fn the_agent_token_never_appears_in_debug_output() {
    let home = HomeServer {
        url: "https://agent.example/mcp".into(),
        token: "super-secret-value".into(),
    };
    assert!(!format!("{home:?}").contains("super-secret-value"));
}

#[tokio::test]
async fn an_added_server_with_a_tool_newt_cannot_offer_is_left_out_not_fatal() {
    let broken = App {
        tools: vec![(
            "synchronize_contacts_with_the_external_directory_service_now",
            json!({}),
        )],
        ..App::default()
    };
    let home = App::home();
    home.settings(json!({
        "model": "default",
        "servers": [{ "name": "crm", "url": serve(broken).await }],
    }));
    let prepared = prepare(&backend(home).await, "ana").await;
    assert!(
        prepared
            .context
            .as_deref()
            .unwrap()
            .contains("<unavailable_servers>\ncrm: unreachable")
    );
    assert!(
        prepared
            .tools
            .iter()
            .any(|tool| tool.spec.name == "complete_task")
    );
}

#[tokio::test]
async fn an_added_server_going_down_leaves_the_homes_pages_readable() {
    let (url, stop) = serve_stoppable(App::plain()).await;
    let home = App::home();
    home.settings(json!({
        "model": "default",
        "servers": [{ "name": "paraform", "url": url }],
    }));
    let prepared = prepare(&backend(home).await, "ana").await;
    prepared.workspace.read("tasks.md").await.unwrap();
    assert_eq!(
        prepared.workspace.paths(),
        ["notes.md", "paraform/jobs/acme-sre.md", "tasks.md"]
    );
    stop.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    prepared.workspace.invalidate(&[Vec::new()]);
    assert_eq!(
        prepared
            .workspace
            .read("tasks.md")
            .await
            .unwrap()
            .as_deref(),
        Some("- [ ] Call the bank\n")
    );
    assert_eq!(
        prepared
            .workspace
            .read("paraform/jobs/acme-sre.md")
            .await
            .unwrap(),
        None,
        "the down server's page is gone for this read, not an error"
    );
}

#[tokio::test]
async fn a_home_page_inside_an_added_servers_folder_fails_loudly() {
    let home = App::home();
    home.set("app://ana/paraform/jobs.md", "mine");
    home.settings(json!({
        "model": "default",
        "servers": [{ "name": "paraform", "url": serve(App::plain()).await }],
    }));
    let prepared = prepare(&backend(home).await, "ana").await;
    let error = prepared
        .workspace
        .read("tasks.md")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("inside the folder of added server `paraform`"),
        "{error}"
    );
}

#[tokio::test]
async fn an_added_server_cannot_share_the_agents_name() {
    let home = App::home();
    home.settings(json!({
        "model": "default",
        "servers": [{ "name": "assistant", "url": "http://127.0.0.1:9/mcp" }],
    }));
    let error = prepare_error(home).await;
    assert!(
        error.contains("cannot be named `assistant`, like the agent"),
        "{error}"
    );
}

#[tokio::test]
async fn a_server_that_changes_its_tools_is_listed_again_without_touching_the_others() {
    let plain = App::plain();
    let home = App::home();
    home.settings(json!({
        "model": "default",
        "servers": [{ "name": "paraform", "url": serve(plain.clone()).await }],
    }));
    let backend = backend(home.clone()).await;
    prepare(&backend, "ana").await;
    assert_eq!(home.listings.load(Ordering::SeqCst), 1);
    assert_eq!(plain.listings.load(Ordering::SeqCst), 1);

    eventually(|| !plain.listeners.lock().unwrap().is_empty()).await;
    let listeners = plain.listeners.lock().unwrap().clone();
    for listener in listeners {
        let _ = listener.notify_tool_list_changed().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    prepare(&backend, "ana").await;
    assert_eq!(
        home.listings.load(Ordering::SeqCst),
        1,
        "the home is kept as it was"
    );
    assert_eq!(
        plain.listings.load(Ordering::SeqCst),
        2,
        "the server that changed is listed again"
    );
}
