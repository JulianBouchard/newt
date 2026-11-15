//! Connecting to MCP servers and listing their tools. One call's path lives in
//! `call.rs`.

mod call;
mod mrtr;
mod pages;
mod skills;

pub use pages::{META_KEY, McpPages};

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use newt_core::prompt::ToolSpec;
use newt_core::skills::Skill;
use newt_core::tools::{Retention, ToolDef};
use newt_core::workspace::PageKey;
use rmcp::handler::client::progress::ProgressDispatcher;
use rmcp::model::{
    ClientConfig, ElicitationCapability, FormElicitationCapability, JsonObject,
    ProgressNotificationParam, ProtocolVersion, ServerNotification, SubscriptionFilter,
    TASKS_EXTENSION_ID, Tool, UrlElicitationCapability,
};
use rmcp::service::NotificationContext;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::backend::{SessionState, TurnState};

pub const META_RETENTION: &str = "newt/retention";
pub const META_INVALIDATES: &str = "newt/invalidates";
pub const META_IDEMPOTENT: &str = "newt/idempotent";
pub const META_SESSION: &str = "newt/session";
pub const META_IDEMPOTENCY_KEY: &str = "newt/idempotency_key";
pub const META_SKILLS_LOADED: &str = "newt/skills_loaded";
pub const META_PAGES_READ: &str = "newt/pages_read";
pub const META_SUMMARY: &str = "newt/summary";
pub const META_INVALIDATE: &str = "newt/invalidate";
pub const META_PAGES: &str = "newt/pages";
pub const META_TURN_STATE: &str = "newt/turn_state";
pub const META_STATE_PATCH: &str = "newt/state_patch";
pub const META_STATE: &str = "newt/state";

const CALL_QUIET_LIMIT: Duration = Duration::from_secs(90);
const CALL_LIMIT: Duration = Duration::from_mins(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// One connection per server carries the calls of every session of an agent,
/// so it must not cap them at rmcp's default of 16 requests in flight.
const MAX_CALLS_IN_FLIGHT: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    #[default]
    None,
    AgentToken,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub auth: McpAuth,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServer")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("auth", &self.auth)
            .finish()
    }
}

type Client = RunningService<RoleClient, Handler>;

/// Newt's side of each MCP connection: hands progress notifications to the
/// call that is waiting for them.
#[derive(Debug, Clone, Default)]
struct Handler {
    progress: ProgressDispatcher,
}

impl ClientHandler for Handler {
    /// Newt can pass a form or a link to the person and come back with the
    /// answer (2.5), and polls tools that run as tasks.
    fn get_info(&self) -> ClientConfig {
        let mut info = ClientConfig::default();
        info.capabilities.elicitation = Some(
            ElicitationCapability::new()
                .with_form(FormElicitationCapability::new())
                .with_url(UrlElicitationCapability::new()),
        );
        info.capabilities
            .extensions
            .get_or_insert_with(Default::default)
            .insert(TASKS_EXTENSION_ID.to_owned(), JsonObject::new());
        info
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.progress.handle_notification(params).await;
    }
}

struct Remote {
    client: Arc<Client>,
    server: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallLimits {
    /// How long a call may go without a result or a progress notification.
    pub quiet: Duration,
    /// How long a call may run in total, however much progress it reports.
    pub total: Duration,
}

impl Default for CallLimits {
    fn default() -> Self {
        Self {
            quiet: CALL_QUIET_LIMIT,
            total: CALL_LIMIT,
        }
    }
}

#[derive(Clone)]
pub struct McpHost {
    tools: Arc<HashMap<String, Remote>>,
    skills: Arc<HashMap<String, skills::Served>>,
    verified: skills::Verified,
    stale: Arc<AtomicBool>,
    limits: CallLimits,
    session: String,
    turn_state: TurnState,
    session_state: SessionState,
}

impl std::fmt::Debug for McpHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpHost")
            .field("tools", &self.tools.len())
            .field("skills", &self.skills.len())
            .field("stale", &self.stale)
            .field("limits", &self.limits)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

pub struct Toolset {
    pub tools: Vec<ToolDef>,
    /// Skills the servers serve, sorted by name, named `{server}/{skill path}`.
    pub skills: Vec<Skill>,
    /// What the servers tell the model about themselves, in server order.
    pub instructions: Vec<String>,
    remotes: Arc<HashMap<String, Remote>>,
    resources: Option<(String, Arc<Client>)>,
    served: Arc<HashMap<String, skills::Served>>,
    verified: skills::Verified,
    stale: Arc<AtomicBool>,
    limits: CallLimits,
}

impl std::fmt::Debug for Toolset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Toolset")
            .field("tools", &self.tools.len())
            .field("skills", &self.skills.len())
            .finish_non_exhaustive()
    }
}

impl Toolset {
    pub async fn connect(servers: &[McpServer], agent_token: &str) -> Result<Self, String> {
        tokio::time::timeout(CONNECT_TIMEOUT, Self::connect_all(servers, agent_token))
            .await
            .map_err(|_| {
                format!(
                    "MCP servers did not connect and list their tools within {} seconds",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
    }

    /// True once a connection broke, a server said its tools changed, or a
    /// server's change notifications stopped: the next turn reconnects and
    /// lists the tools again.
    pub fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Relaxed)
    }

    /// The bootstrap's own skills followed by the served ones. A served skill
    /// never silently shadows a bootstrap skill of the same name.
    pub fn skills_after(&self, bootstrap: &[Skill]) -> Result<Vec<Skill>, String> {
        skills::merge(bootstrap, &self.skills)
    }

    /// One session's workspace pages, from the first server that offers
    /// resources, or `None` when no server does.
    pub fn pages(&self, session: &str) -> Option<Arc<McpPages>> {
        self.resources
            .as_ref()
            .map(|(server, client)| Arc::new(McpPages::new(Arc::clone(client), server, session)))
    }

    #[must_use]
    pub fn with_call_limits(mut self, limits: CallLimits) -> Self {
        self.limits = limits;
        self
    }

    async fn connect_all(servers: &[McpServer], agent_token: &str) -> Result<Self, String> {
        let mut tools = Vec::new();
        let mut remotes = HashMap::new();
        let mut offered = Vec::new();
        let mut served = HashMap::new();
        let mut instructions = Vec::new();
        let mut resources = None;
        let stale = Arc::new(AtomicBool::new(false));
        for server in servers {
            let client = Arc::new(connect(server, agent_token).await?);
            if let Some(info) = client.peer_info() {
                if let Some(text) = info.instructions.as_deref().map(str::trim)
                    && !text.is_empty()
                {
                    instructions.push(text.to_owned());
                }
                if resources.is_none() && info.capabilities.resources.is_some() {
                    resources = Some((server.name.clone(), Arc::clone(&client)));
                }
            }
            let mut listed = client
                .list_all_tools()
                .await
                .map_err(|error| format!("MCP server `{}`: listing tools: {error}", server.name))?;
            listed.sort_by(|left, right| left.name.cmp(&right.name));
            watch_tool_list(&server.name, &client, &stale).await;
            for tool in listed {
                let name = tool.name.to_string();
                let def = tool_def(tool).map_err(|error| {
                    format!("MCP server `{}`, tool `{name}`: {error}", server.name)
                })?;
                let remote = Remote {
                    client: Arc::clone(&client),
                    server: server.name.clone(),
                };
                if remotes.insert(name.clone(), remote).is_some() {
                    return Err(format!("two MCP servers both offer a tool named `{name}`"));
                }
                tools.push(def);
            }
            for (skill, entry) in skills::list(&server.name, &client).await? {
                served.insert(skill.name.clone(), entry);
                offered.push(skill);
            }
            tracing::info!(server = %server.name, tools = tools.len(), skills = offered.len(), "MCP server connected");
        }
        offered.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self {
            tools,
            skills: offered,
            instructions,
            remotes: Arc::new(remotes),
            resources,
            served: Arc::new(served),
            verified: skills::Verified::default(),
            stale,
            limits: CallLimits::default(),
        })
    }

    pub fn host(
        &self,
        session: &str,
        turn_state: TurnState,
        session_state: SessionState,
    ) -> McpHost {
        McpHost {
            tools: Arc::clone(&self.remotes),
            skills: Arc::clone(&self.served),
            verified: Arc::clone(&self.verified),
            stale: Arc::clone(&self.stale),
            limits: self.limits,
            session: session.to_owned(),
            turn_state,
            session_state,
        }
    }
}

async fn watch_tool_list(server: &str, client: &Client, stale: &Arc<AtomicBool>) {
    let filter = SubscriptionFilter::builder().tools_list_changed().build();
    let mut subscription = match client.peer().listen(filter).await {
        Ok(subscription) if subscription.acknowledged().tools_list_changed == Some(true) => {
            subscription
        }
        Ok(_) => {
            tracing::debug!(server, "MCP server does not announce tool changes");
            return;
        }
        Err(error) => {
            tracing::debug!(server, %error, "MCP server offers no change notifications");
            return;
        }
    };
    let server = server.to_owned();
    let stale = Arc::clone(stale);
    tokio::spawn(async move {
        loop {
            match subscription.next().await {
                Ok(Some(ServerNotification::ToolListChangedNotification(_))) => {
                    tracing::info!(
                        server,
                        "MCP server changed its tools; relisting before the next turn"
                    );
                    stale.store(true, Ordering::Relaxed);
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    tracing::info!(
                        server,
                        "MCP change notifications stopped; relisting before the next turn"
                    );
                    stale.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
    });
}

async fn connect(server: &McpServer, agent_token: &str) -> Result<Client, String> {
    if !(server.url.starts_with("https://") || server.url.starts_with("http://")) {
        return Err(format!(
            "MCP server `{}`: url must start with http:// or https://",
            server.name
        ));
    }
    let mut headers = HashMap::new();
    for (name, value) in &server.headers {
        let header_name = name
            .parse()
            .map_err(|_| format!("MCP server `{}`: invalid header name `{name}`", server.name))?;
        let header_value = value.parse().map_err(|_| {
            format!(
                "MCP server `{}`: invalid value for header `{name}`",
                server.name
            )
        })?;
        headers.insert(header_name, header_value);
    }
    let mut config = StreamableHttpClientTransportConfig::with_uri(server.url.as_str())
        .custom_headers(headers)
        .max_concurrent_requests(MAX_CALLS_IN_FLIGHT);
    if server.auth == McpAuth::AgentToken {
        config = config.auth_header(agent_token);
    }
    Handler::default()
        .serve_with_lifecycle(
            StreamableHttpClientTransport::from_config(config),
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::LATEST],
                legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
            },
        )
        .await
        .map_err(|error| format!("MCP server `{}`: connecting: {error}", server.name))
}

fn meta_field<T: DeserializeOwned>(
    meta: Option<&JsonObject>,
    key: &str,
) -> Result<Option<T>, String> {
    meta.and_then(|meta| meta.get(key))
        .map(|value| {
            T::deserialize(value).map_err(|error| format!("`_meta.{key}` is invalid: {error}"))
        })
        .transpose()
}

fn tool_def(tool: Tool) -> Result<ToolDef, String> {
    let meta = tool.meta.as_deref();
    let retention: Retention = meta_field(meta, META_RETENTION)?.unwrap_or(Retention::Durable);
    let invalidates: Vec<PageKey> = meta_field(meta, META_INVALIDATES)?.unwrap_or_default();
    let idempotent: bool = meta_field(meta, META_IDEMPOTENT)?.unwrap_or(false);
    if retention == Retention::Outward && !idempotent {
        return Err(format!(
            "outward tools must set `_meta.{META_IDEMPOTENT}: true` and honour `_meta.{META_IDEMPOTENCY_KEY}`, or a crash could send twice"
        ));
    }
    let mut def = ToolDef::new(
        ToolSpec {
            name: tool.name.to_string(),
            description: tool
                .description
                .map(|text| text.to_string())
                .unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
        },
        retention,
    );
    def.invalidates = invalidates;
    Ok(def)
}
