use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::State;
use axum::routing::{get, post};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::profile::{Kind, Profile};
use crate::sample::{Rng, Spread};

const VERSION: &str = "loadtest-1";
const PREFIX_BYTES: usize = 30_000;
const PAGE_BYTES: usize = 1_500;

#[derive(Debug)]
pub struct MockApp {
    rng: Rng,
    speed: f64,
    render: Spread,
    lazy_pages: bool,
    public_url: String,
    instructions: String,
    profile: Profile,
    pub tool_calls: AtomicU64,
    pub sends: AtomicU64,
}

impl MockApp {
    pub fn new(
        public_url: &str,
        speed: f64,
        rng_seed: u64,
        render_ms: f64,
        lazy_pages: bool,
        profile: Profile,
    ) -> Arc<Self> {
        let rng = Rng::new(rng_seed);
        let mut instructions = String::from(
            "You are a customer assistant. Read the workspace, act with your tools, then end the turn.\n",
        );
        while instructions.len() < PREFIX_BYTES {
            instructions.push_str(&rng.words(12));
            instructions.push('\n');
        }
        Arc::new(Self {
            rng,
            speed,
            render: Spread::new(render_ms, render_ms * 2.0),
            lazy_pages,
            public_url: public_url.trim_end_matches('/').to_owned(),
            instructions,
            profile,
            tool_calls: AtomicU64::new(0),
            sends: AtomicU64::new(0),
        })
    }

    fn page(&self, path: &str) -> Value {
        let mut content = format!("# {path}\n");
        while content.len() < PAGE_BYTES {
            content.push_str(&self.rng.words(10));
            content.push('\n');
        }
        let key: Vec<&str> = path.trim_end_matches(".md").split('/').collect();
        json!({ "key": key, "path": path, "version": self.rng.next_u64().to_string(), "content": content })
    }

    fn pages(&self) -> Vec<Value> {
        self.profile
            .pages
            .iter()
            .map(|path| self.page(path))
            .collect()
    }
}

#[derive(Deserialize)]
struct PagesRequest {
    keys: Vec<Vec<String>>,
}

pub fn router(app: Arc<MockApp>) -> axum::Router {
    let tools_app = Arc::clone(&app);
    let mcp: StreamableHttpService<Tools, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(Tools(Arc::clone(&tools_app))),
        Arc::default(),
        StreamableHttpServerConfig::default(),
    );
    axum::Router::new()
        .route("/agent/bootstrap", get(bootstrap))
        .route("/agent/turn", post(turn))
        .route("/agent/pages", post(pages))
        .route("/stats", get(stats))
        .nest_service("/mcp", mcp)
        .with_state(app)
}

async fn bootstrap(State(app): State<Arc<MockApp>>) -> Json<Value> {
    Json(json!({
        "version": VERSION,
        "instructions": app.instructions,
        "model": "default",
        "first_call_effort": "low",
        "effort": "medium",
        "max_model_calls": 30,
        "skills": app.profile.skills.iter().map(|name| json!({
            "name": name,
            "description": format!("How to handle {name} work."),
            "content": app.rng.words(400),
        })).collect::<Vec<_>>(),
        "mcp_servers": [{ "name": "tools", "url": format!("{}/mcp", app.public_url), "auth": "agent_token" }],
    }))
}

async fn turn(State(app): State<Arc<MockApp>>) -> Json<Value> {
    if app.lazy_pages {
        return Json(json!({ "version": VERSION }));
    }
    tokio::time::sleep(app.render.millis(&app.rng, app.speed)).await;
    Json(json!({ "version": VERSION, "pages": app.pages() }))
}

async fn pages(State(app): State<Arc<MockApp>>, Json(request): Json<PagesRequest>) -> Json<Value> {
    tokio::time::sleep(app.render.millis(&app.rng, app.speed)).await;
    let wanted: Vec<Value> = app
        .pages()
        .into_iter()
        .filter(|page| {
            request.keys.iter().any(|prefix| {
                page["key"].as_array().is_some_and(|key| {
                    key.len() >= prefix.len() && key.iter().zip(prefix).all(|(a, b)| a == b)
                })
            })
        })
        .collect();
    Json(json!({ "pages": wanted }))
}

async fn stats(State(app): State<Arc<MockApp>>) -> Json<Value> {
    Json(json!({
        "tool_calls": app.tool_calls.load(Ordering::Relaxed),
        "sends": app.sends.load(Ordering::Relaxed),
    }))
}

#[derive(Clone)]
struct Tools(Arc<MockApp>);

fn meta(value: Value) -> MetaObject {
    let mut meta = MetaObject::new();
    if let Value::Object(fields) = value {
        for (key, field) in fields {
            meta.insert(key, field);
        }
    }
    meta
}

fn catalogue(profile: &Profile) -> Vec<Tool> {
    let mut schema = JsonObject::new();
    schema.insert("type".to_owned(), json!("object"));
    schema.insert(
        "properties".to_owned(),
        json!({ "text": { "type": "string" } }),
    );
    schema.insert("required".to_owned(), json!(["text"]));
    let schema = Arc::new(schema);
    profile
        .tools
        .iter()
        .filter_map(|tool| {
            let newt = match tool.kind {
                Kind::Send => json!({ "newt/retention": "outward", "newt/idempotent": true }),
                Kind::Read => json!({ "newt/retention": "ephemeral" }),
                Kind::Write => json!({ "newt/retention": "durable", "newt/invalidates": [[]] }),
                Kind::Workspace | Kind::Skill | Kind::End => return None,
            };
            Some(
                Tool::new(
                    tool.name.clone(),
                    "A tool mocked for load tests.",
                    Arc::clone(&schema),
                )
                .with_meta(meta(newt)),
            )
        })
        .collect()
}

impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(catalogue(
            &self.0.profile,
        ))))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send + '_ {
        let app = Arc::clone(&self.0);
        async move {
            app.tool_calls.fetch_add(1, Ordering::Relaxed);
            let Some(profile) = app.profile.find(&request.name).cloned() else {
                return Ok(CallToolResult::error(vec![ContentBlock::text("unknown tool")]).into());
            };
            tokio::time::sleep(profile.latency.millis(&app.rng, app.speed)).await;
            if app.rng.chance(profile.error_rate) {
                return Ok(CallToolResult::error(vec![ContentBlock::text("mock refusal")]).into());
            }
            if profile.kind == Kind::Send {
                app.sends.fetch_add(1, Ordering::Relaxed);
            }
            let result = CallToolResult::success(vec![ContentBlock::text(format!(
                "{{\"ok\":true,\"note\":\"{}\"}}",
                app.rng.words(8)
            ))])
            .with_meta(Some(meta(
                json!({ "newt/summary": format!("{} done", profile.name) }),
            )));
            Ok(result.into())
        }
    }
}
