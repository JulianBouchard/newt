#![expect(
    clippy::unwrap_used,
    clippy::unused_async_trait_impl,
    reason = "tests fail loudly on setup errors; the test server's handlers read like real async ones"
)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newt_server::backend::{Backend, TurnRequest};
use newt_server::context::{ContextProviderBackend, McpAgent};
use rmcp::model::{
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, RequestMetaObject, Resource, ResourceContents,
    ServerCapabilities, ServerConfig, SubscriptionFilter,
};
use rmcp::service::{RequestContext, SubscriptionContext, SubscriptionSink};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};

const INSTRUCTIONS: &str = "You help one person keep their tasks.";

#[derive(Clone, Default)]
struct App {
    pages: Arc<Mutex<HashMap<String, String>>>,
    listeners: Arc<Mutex<Vec<SubscriptionSink>>>,
    ended: Arc<AtomicUsize>,
}

impl App {
    fn new() -> Self {
        let app = Self::default();
        app.set("app://ana/tasks.md", "- [ ] Call the bank\n");
        app.set("app://ana/notes.md", "Lives in Lisbon\n");
        app.set("app://ben/tasks.md", "- [ ] Book flights\n");
        app
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
}

fn session(meta: &RequestMetaObject) -> String {
    meta.get("newt/session")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned()
}

impl ServerHandler for App {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
        .with_instructions(INSTRUCTIONS)
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
        Ok(ListToolsResult::with_all_items(Vec::new()))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let prefix = format!("app://{}/", session(&context.meta));
        let mut uris: Vec<String> = self
            .pages
            .lock()
            .unwrap()
            .keys()
            .filter(|uri| uri.starts_with(&prefix))
            .cloned()
            .collect();
        uris.sort();
        let mut resources: Vec<Resource> = uris
            .iter()
            .map(|uri| Resource::new(uri, uri.trim_start_matches(&prefix)))
            .collect();
        resources.push(Resource::new("skill://writing/SKILL.md", "SKILL.md"));
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let owner = format!("app://{}/", session(&context.meta));
        if !request.uri.starts_with(&owner) {
            return Err(ErrorData::invalid_params("not this session's page", None));
        }
        let text = self
            .pages
            .lock()
            .unwrap()
            .get(&request.uri)
            .cloned()
            .ok_or_else(|| ErrorData::invalid_params("no such page", None))?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri)]).into())
    }
}

async fn backend(app: App) -> ContextProviderBackend {
    let service: StreamableHttpService<App, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(app.clone()),
        Arc::default(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    ContextProviderBackend::new(BTreeMap::new(), ["default".to_owned()])
        .unwrap()
        .with_mcp_agents(BTreeMap::from([(
            "assistant".to_owned(),
            McpAgent {
                url: format!("http://{address}/mcp"),
                model: "default".into(),
                token: "agent-token".into(),
            },
        )]))
}

#[tokio::test]
async fn an_mcp_server_alone_is_an_agent_with_its_instructions_and_the_sessions_resources() {
    let backend = backend(App::new()).await;
    let ana = backend
        .prepare("assistant", "ana", &TurnRequest::default())
        .await
        .unwrap();
    assert_eq!(ana.system, INSTRUCTIONS);
    assert_eq!(ana.model, "default");
    assert_eq!(
        ana.workspace.read("tasks.md").await.unwrap().as_deref(),
        Some("- [ ] Call the bank\n")
    );
    assert_eq!(ana.workspace.paths(), ["notes.md", "tasks.md"]);
    let ben = backend
        .prepare("assistant", "ben", &TurnRequest::default())
        .await
        .unwrap();
    assert_eq!(
        ben.workspace.read("tasks.md").await.unwrap().as_deref(),
        Some("- [ ] Book flights\n")
    );
    assert_eq!(ben.workspace.paths(), ["tasks.md"]);
}

#[tokio::test]
async fn a_resource_update_from_the_server_makes_the_next_read_fetch_it_again() {
    let app = App::new();
    let backend = backend(app.clone()).await;
    let ana = backend
        .prepare("assistant", "ana", &TurnRequest::default())
        .await
        .unwrap();
    assert_eq!(
        ana.workspace.read("tasks.md").await.unwrap().as_deref(),
        Some("- [ ] Call the bank\n")
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.listeners.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    app.set("app://ana/tasks.md", "- [x] Call the bank\n");
    app.announce("app://ana/tasks.md").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ana.workspace.is_stale("tasks.md") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!ana.workspace.is_stale("notes.md"));
    assert_eq!(
        ana.workspace.read("tasks.md").await.unwrap().as_deref(),
        Some("- [x] Call the bank\n")
    );
}

#[tokio::test]
async fn the_subscription_ends_with_the_turns_workspace() {
    let app = App::new();
    let backend = backend(app.clone()).await;
    let ana = backend
        .prepare("assistant", "ana", &TurnRequest::default())
        .await
        .unwrap();
    ana.workspace.read("tasks.md").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.listeners.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(ana);
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.ended.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
