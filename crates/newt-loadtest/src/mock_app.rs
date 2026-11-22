//! The load test's agent: one home MCP server with a production-sized prompt
//! and workspace, skills served over MCP, and tools with a real assistant's
//! latencies and error rates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::State;
use axum::routing::get;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    ListResourcesResult, ListSkillsResult, ListToolsResult, MetaObject, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig, Skill, SkillFrontmatter, SkillResource,
    SkillResources, SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};

use crate::profile::{Kind, Profile};
use crate::sample::{Rng, Spread};

const PREFIX_BYTES: usize = 30_000;
const PAGE_BYTES: usize = 1_500;
const PAGE_SCHEME: &str = "mock://";

#[derive(Debug)]
pub struct MockApp {
    rng: Rng,
    speed: f64,
    render: Spread,
    instructions: String,
    skills: Vec<(String, String)>,
    profile: Profile,
    pub tool_calls: AtomicU64,
    pub sends: AtomicU64,
    pub resource_lists: AtomicU64,
    pub resource_reads: AtomicU64,
}

impl MockApp {
    pub fn new(speed: f64, rng_seed: u64, render_ms: f64, profile: Profile) -> Arc<Self> {
        let rng = Rng::new(rng_seed);
        let mut instructions = String::from(
            "You are a customer assistant. Read the workspace, act with your tools, then end the turn.\n",
        );
        while instructions.len() < PREFIX_BYTES {
            instructions.push_str(&rng.words(12));
            instructions.push('\n');
        }
        let skills = profile
            .skills
            .iter()
            .map(|name| {
                let file = format!(
                    "---\nname: {name}\ndescription: How to handle {name} work.\n---\n\n{}\n",
                    rng.words(400)
                );
                (name.clone(), file)
            })
            .collect();
        Arc::new(Self {
            rng,
            speed,
            render: Spread::new(render_ms, render_ms * 2.0),
            instructions,
            skills,
            profile,
            tool_calls: AtomicU64::new(0),
            sends: AtomicU64::new(0),
            resource_lists: AtomicU64::new(0),
            resource_reads: AtomicU64::new(0),
        })
    }

    fn page(&self) -> String {
        let mut content = String::from("# page\n");
        while content.len() < PAGE_BYTES {
            content.push_str(&self.rng.words(10));
            content.push('\n');
        }
        content
    }

    /// The agent's settings, declared as the `newt` extension.
    fn settings() -> JsonObject {
        let Value::Object(settings) = json!({
            "model": "default",
            "first_call_effort": "low",
            "effort": "medium",
            "max_model_calls": 30,
        }) else {
            unreachable!("a JSON object literal")
        };
        settings
    }
}

impl MockApp {
    /// A page, freshly rendered, or a skill's file.
    fn read(&self, request: ReadResourceRequestParams) -> Result<ReadResourceResponse, ErrorData> {
        self.resource_reads.fetch_add(1, Ordering::Relaxed);
        let text = if request.uri.starts_with(PAGE_SCHEME) {
            self.page()
        } else {
            self.skills
                .iter()
                .find(|(name, _)| skill_uri(name) == request.uri)
                .map(|(_, file)| file.clone())
                .ok_or_else(|| ErrorData::invalid_params("no such resource", None))?
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri)]).into())
    }
}

fn skill_uri(name: &str) -> String {
    format!("skill://{name}/SKILL.md")
}

/// The agent at `/mcp`, and the counters at `/stats`.
pub fn router(app: Arc<MockApp>) -> axum::Router {
    let home_app = Arc::clone(&app);
    let mcp: StreamableHttpService<Home, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(Home(Arc::clone(&home_app))),
        Arc::default(),
        StreamableHttpServerConfig::default(),
    );
    axum::Router::new()
        .route("/stats", get(stats))
        .nest_service("/mcp", mcp)
        .with_state(app)
}

/// Serves the agent over stdin and stdout, for newt to run it with
/// `mcp = "stdio:newt-loadtest mock-app --stdio"`.
pub async fn serve_stdio(app: Arc<MockApp>) -> anyhow::Result<()> {
    use rmcp::ServiceExt as _;
    let running = Home(app)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    running.waiting().await?;
    Ok(())
}

async fn stats(State(app): State<Arc<MockApp>>) -> Json<Value> {
    Json(json!({
        "tool_calls": app.tool_calls.load(Ordering::Relaxed),
        "sends": app.sends.load(Ordering::Relaxed),
        "resource_lists": app.resource_lists.load(Ordering::Relaxed),
        "resource_reads": app.resource_reads.load(Ordering::Relaxed),
    }))
}

#[derive(Clone)]
struct Home(Arc<MockApp>);

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

impl ServerHandler for Home {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .enable_skills()
            .build();
        capabilities
            .extensions
            .get_or_insert_with(Default::default)
            .insert("newt".to_owned(), MockApp::settings());
        ServerConfig::new(capabilities).with_instructions(self.0.instructions.clone())
    }

    /// Accepts newt's subscriptions so it does not log a refusal every turn.
    /// The mock app never changes a page or a tool, so it sends nothing on them.
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        context.cancelled().await;
        Ok(())
    }

    /// Every session sees the same pages. Listing them costs the render time
    /// once per turn, as rendering a real workspace would.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        self.0.resource_lists.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(self.0.render.millis(&self.0.rng, self.0.speed)).await;
        let resources = self
            .0
            .profile
            .pages
            .iter()
            .map(|path| {
                let key: Vec<&str> = path.trim_end_matches(".md").split('/').collect();
                let mut meta = MetaObject::new();
                meta.insert("newt/key".to_owned(), json!(key));
                Resource::new(format!("{PAGE_SCHEME}{path}"), path.clone()).with_meta(meta)
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, ErrorData>> + Send + '_ {
        std::future::ready(self.0.read(request))
    }

    fn list_skills(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListSkillsResult, ErrorData>> + Send + '_ {
        let skills = self
            .0
            .skills
            .iter()
            .map(|(name, file)| {
                let uri = skill_uri(name);
                Skill::new(
                    uri.clone(),
                    SkillFrontmatter::new(name.clone(), format!("How to handle {name} work.")),
                    SkillResources::Files(vec![SkillResource::from_bytes(uri, file.as_bytes())]),
                )
            })
            .collect();
        std::future::ready(Ok(ListSkillsResult::with_all_items(skills)))
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
