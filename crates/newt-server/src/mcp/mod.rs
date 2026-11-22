//! Connecting to an agent's MCP servers and listing their tools: the home
//! server, which is the agent, and the servers it adds. One call's path lives
//! in `call.rs`, workspace pages in `pages.rs`.

mod call;
mod mrtr;
mod pages;
mod skills;

pub use pages::{META_KEY, ServerPages, check_pages};

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::join_all;
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
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient};
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

/// The MCP extension a server declares to speak newt's protocol: the session
/// data newt sends and the `newt/*` fields it reads back. The home server
/// declares it with the agent's settings; an added server declares it empty.
pub const NEWT_EXTENSION: &str = "newt";
/// Between an added server's name and its tool's name: `paraform__search_jobs`.
pub const TOOL_SEPARATOR: &str = "__";
/// The longest tool name the model APIs accept.
pub const MAX_TOOL_NAME: usize = 64;
/// URL scheme of a server newt runs itself and talks to over stdin and stdout.
pub const STDIO_SCHEME: &str = "stdio:";

const CALL_QUIET_LIMIT: Duration = Duration::from_secs(90);
const CALL_LIMIT: Duration = Duration::from_mins(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How often newt tries again to reach an added server that was down.
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// One connection per server carries the calls of every session of an agent,
/// so it must not cap them at rmcp's default of 16 requests in flight.
const MAX_CALLS_IN_FLIGHT: usize = 1024;

#[derive(Clone, PartialEq, Eq, Default)]
pub enum McpAuth {
    #[default]
    None,
    /// Sent as `Authorization: Bearer <token>` over HTTP.
    Bearer(String),
}

impl std::fmt::Debug for McpAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Bearer(_) => formatter.write_str("Bearer(<redacted>)"),
        }
    }
}

/// Where a server is: `https://…`, `http://…`, or `stdio:<command>` for one
/// newt starts itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub url: String,
    pub auth: McpAuth,
}

impl McpServer {
    /// Checks that the URL is one newt can reach.
    pub fn check_url(url: &str) -> Result<(), String> {
        if url.starts_with("https://") || url.starts_with("http://") {
            return Ok(());
        }
        match url.strip_prefix(STDIO_SCHEME) {
            Some(command) if !command.trim().is_empty() => Ok(()),
            Some(_) => Err(format!("`{url}` names no command to run")),
            None => Err(format!(
                "`{url}` must start with https://, http:// or {STDIO_SCHEME}"
            )),
        }
    }
}

/// Checks an added server's name, which becomes its folder in the workspace
/// and the prefix of its tools.
pub fn check_server_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name.starts_with(|first: char| first.is_ascii_lowercase() || first.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "server name `{name}` must be 1 to 32 lowercase letters, digits or `-`, starting with a letter or digit"
        ))
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
        let extensions = info
            .capabilities
            .extensions
            .get_or_insert_with(Default::default);
        extensions.insert(TASKS_EXTENSION_ID.to_owned(), JsonObject::new());
        extensions.insert(NEWT_EXTENSION.to_owned(), JsonObject::new());
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

/// Which server a page or a tool belongs to. The home's pages sit at the root
/// of the workspace; an added server's sit under its name, and their keys are
/// scoped to it, so its tools can only make its own pages stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Owner(Option<String>);

impl Owner {
    pub(crate) const HOME: Self = Self(None);

    pub(crate) fn added(name: &str) -> Self {
        Self(Some(name.to_owned()))
    }

    /// The first element of every key this server's pages carry: a NUL and
    /// the server's name, which no home key can collide with by accident.
    fn tag(&self) -> Option<String> {
        self.0.as_ref().map(|name| format!("\u{0}{name}"))
    }

    pub(crate) fn key(&self, key: PageKey) -> PageKey {
        match self.tag() {
            Some(tag) => std::iter::once(tag).chain(key).collect(),
            None => key,
        }
    }

    pub(crate) fn path(&self, path: &str) -> String {
        match &self.0 {
            Some(name) => format!("{name}/{path}"),
            None => path.to_owned(),
        }
    }

    /// Whether a key, as the workspace holds it, may be one of this server's.
    fn may_own(&self, key: &[String]) -> bool {
        match (self.tag(), key.first()) {
            (_, None) => true,
            (Some(tag), Some(first)) => *first == tag,
            (None, Some(first)) => !first.starts_with('\u{0}'),
        }
    }
}

#[derive(Clone)]
struct Remote {
    client: Arc<Client>,
    server: String,
    /// The tool's name on its server, without newt's prefix.
    tool: String,
    /// Whether the server declared the newt extension (the home always does).
    speaks_newt: bool,
    owner: Owner,
    /// Its server's stale flag: a call that finds the connection broken sets it.
    stale: Arc<AtomicBool>,
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
            .field("limits", &self.limits)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// A connection to the home server, with what it declared when newt
/// connected: its instructions and its `newt` extension.
#[derive(Clone)]
pub struct HomeConnection {
    server: McpServer,
    client: Arc<Client>,
    /// The home's `instructions`: the agent's system prompt.
    pub instructions: String,
    /// The home's `newt` extension object: the agent's settings.
    pub extension: Option<JsonObject>,
}

impl std::fmt::Debug for HomeConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HomeConnection")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl HomeConnection {
    pub async fn connect(server: &McpServer) -> Result<Self, String> {
        let client = within_connect_timeout("connect", connect(server))
            .await
            .map_err(|error| format!("MCP server `{}` {error}", server.name))??;
        let info = client.peer_info();
        let instructions = info
            .as_ref()
            .and_then(|info| info.instructions.as_deref())
            .map(str::trim)
            .unwrap_or_default()
            .to_owned();
        let extension = info
            .as_ref()
            .and_then(|info| info.capabilities.extensions.as_ref())
            .and_then(|extensions| extensions.get(NEWT_EXTENSION))
            .cloned();
        Ok(Self {
            server: server.clone(),
            client: Arc::new(client),
            instructions,
            extension,
        })
    }

    /// Sends one of the newt extension's own requests (`newt/turn`,
    /// `newt/turn_end`) and returns its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        call::custom(&self.client, &self.server.name, method, params).await
    }

    /// Lists the home's tools and connects the servers it adds, all at once.
    /// An added server that cannot be reached is left out and listed in
    /// `unavailable`; newt keeps trying it in the background.
    pub async fn with_servers(self, added: &[McpServer]) -> Result<Toolset, String> {
        let folders = check_added(&self.server.name, added)?;
        let home = list_home(&self.server, &self.client).await?;
        let attempts = join_all(
            added
                .iter()
                .map(|server| async move { (server.clone(), connect_added(server).await) }),
        )
        .await;
        assemble(self, home, attempts, folders)
    }
}

/// Why an added server is left out of a toolset.
enum Left {
    /// It did not connect or list in time: newt keeps trying it.
    Unreachable(String),
    /// It connected but offers something newt cannot load, such as a tool
    /// name over the limit: newt does not retry it until the home reconnects.
    Invalid(String),
}

impl std::fmt::Display for Left {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) | Self::Invalid(why) => formatter.write_str(why),
        }
    }
}

/// The servers of one agent, connected and listed: the tools and skills the
/// model sees, and where each call goes.
pub struct Toolset {
    pub tools: Vec<ToolDef>,
    /// Skills the servers serve, sorted by name, named `{server}/{skill path}`.
    pub skills: Vec<Skill>,
    /// Added servers left out this time, by name.
    pub unavailable: Vec<String>,
    /// The left-out servers newt does not retry until the home reconnects.
    invalid: HashSet<String>,
    /// The left-out servers a probe found answering again.
    answered: Arc<Mutex<HashSet<String>>>,
    home: HomeConnection,
    /// The home first, then each added server that is connected.
    servers: Vec<Listed>,
    remotes: Arc<HashMap<String, Remote>>,
    /// The added servers' names: folders the home's pages may not use.
    folders: Arc<HashSet<String>>,
    served: Arc<HashMap<String, skills::Served>>,
    verified: skills::Verified,
    /// Set when a server that was down answers again.
    retry: Arc<AtomicBool>,
    limits: CallLimits,
    #[expect(
        dead_code,
        reason = "held only so the probes stop when the toolset is dropped"
    )]
    probes: Vec<AbortOnDrop>,
}

impl std::fmt::Debug for Toolset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Toolset")
            .field("tools", &self.tools.len())
            .field("skills", &self.skills.len())
            .field("unavailable", &self.unavailable)
            .finish_non_exhaustive()
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One server's tools, under the names the model sees, and its skills, once
/// it is connected, listed and checked.
#[derive(Clone)]
struct Listed {
    server: McpServer,
    client: Arc<Client>,
    tools: Vec<(String, ToolDef, Remote)>,
    skills: Vec<(Skill, skills::Served)>,
    speaks_newt: bool,
    owner: Owner,
    /// Set when the server says its tools changed, its change notifications
    /// stop, or a call finds its connection broken.
    stale: Arc<AtomicBool>,
    /// The task that sets `stale` when the server announces a tool change.
    watcher: Option<Arc<AbortOnDrop>>,
}

impl Listed {
    /// Still connected, and nothing it said since it was listed changes it.
    fn is_current(&self) -> bool {
        !self.stale.load(Ordering::Relaxed) && !self.client.is_closed()
    }

    fn offers_resources(&self) -> bool {
        self.client
            .peer_info()
            .is_some_and(|info| info.capabilities.resources.is_some())
    }

    /// A server newt runs over stdio keeps its state in its process, so while
    /// the process lives newt lists it again on the same connection instead
    /// of starting a new one.
    fn keeps_its_process(&self) -> bool {
        self.server.url.starts_with(STDIO_SCHEME) && !self.client.is_closed()
    }

    async fn relist(&self) -> Result<Self, String> {
        if let Some(watcher) = &self.watcher {
            watcher.0.abort();
        }
        within_connect_timeout(
            "list its tools",
            list(
                self.server.clone(),
                Arc::clone(&self.client),
                self.speaks_newt,
                self.owner.clone(),
            ),
        )
        .await?
    }
}

impl Toolset {
    /// Connects the home, then the servers it adds.
    pub async fn connect(home: &McpServer, added: &[McpServer]) -> Result<Self, String> {
        HomeConnection::connect(home)
            .await?
            .with_servers(added)
            .await
    }

    /// True when the next turn needs the toolset rebuilt: a server's
    /// connection broke or closed, it said its tools changed, or a server
    /// that was down answers again.
    pub fn is_stale(&self) -> bool {
        self.retry.load(Ordering::Relaxed) || self.servers.iter().any(|server| !server.is_current())
    }

    /// True when rebuilding needs a new connection to the home, which reads
    /// its declaration again: its connection closed, or a home reached over
    /// HTTP said its tools changed.
    pub fn needs_new_home(&self) -> bool {
        self.servers
            .first()
            .is_none_or(|home| !home.is_current() && !home.keeps_its_process())
    }

    /// The next toolset, built from this one. A server that is still
    /// current is kept as it is, and one newt runs over stdio is listed again
    /// on the same process. With a new connection to the home, every other
    /// server connects again; without one, a server still down or invalid
    /// stays out until a probe sees it answer, so a reload never waits on it.
    pub async fn refresh(
        &self,
        new_home: Option<HomeConnection>,
        added: &[McpServer],
    ) -> Result<Self, String> {
        let home = new_home.unwrap_or_else(|| self.home.clone());
        let folders = check_added(&home.server.name, added)?;
        let reconnected = !Arc::ptr_eq(&home.client, &self.home.client);
        let home_listed = match self.servers.first() {
            Some(listed) if !reconnected && listed.is_current() => listed.clone(),
            Some(listed) if !reconnected && listed.keeps_its_process() => listed.relist().await?,
            _ => list_home(&home.server, &home.client).await?,
        };
        let answered = self.answered();
        let attempts = join_all(added.iter().map(|server| {
            let answered = &answered;
            async move {
                let previous = self
                    .servers
                    .iter()
                    .skip(1)
                    .find(|listed| listed.server == *server);
                let attempt = match previous {
                    Some(listed) if listed.is_current() => Ok(listed.clone()),
                    Some(listed) if listed.keeps_its_process() => {
                        listed.relist().await.map_err(Left::Unreachable)
                    }
                    None if !reconnected && self.invalid.contains(&server.name) => Err(
                        Left::Invalid("still offers what newt cannot load".to_owned()),
                    ),
                    None if !reconnected
                        && self.unavailable.contains(&server.name)
                        && !answered.contains(&server.name) =>
                    {
                        Err(Left::Unreachable("still not answering".to_owned()))
                    }
                    _ => connect_added(server).await,
                };
                (server.clone(), attempt)
            }
        }))
        .await;
        assemble(home, home_listed, attempts, folders)
    }

    fn answered(&self) -> HashSet<String> {
        match self.answered.lock() {
            Ok(answered) => answered.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub const fn home(&self) -> &HomeConnection {
        &self.home
    }

    /// One session's workspace pages, from every server that offers
    /// resources, or `None` when no server does.
    pub fn pages(&self, session: &str, state: &SessionState) -> Option<Arc<ServerPages>> {
        let sources: Vec<pages::McpPages> = self
            .servers
            .iter()
            .filter(|listed| listed.offers_resources())
            .map(|listed| {
                pages::McpPages::new(
                    Arc::clone(&listed.client),
                    &listed.server.name,
                    session,
                    listed.speaks_newt,
                    listed.owner.clone(),
                    state.clone(),
                    Arc::clone(&self.folders),
                )
            })
            .collect();
        (!sources.is_empty()).then(|| Arc::new(ServerPages::new(sources)))
    }

    #[must_use]
    pub fn with_call_limits(mut self, limits: CallLimits) -> Self {
        self.limits = limits;
        self
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
            limits: self.limits,
            session: session.to_owned(),
            turn_state,
            session_state,
        }
    }
}

/// Checks the added servers' names, and returns them: the folders the home's
/// pages may not use.
fn check_added(agent: &str, added: &[McpServer]) -> Result<HashSet<String>, String> {
    let mut folders = HashSet::new();
    for server in added {
        check_server_name(&server.name)?;
        if server.name == agent {
            return Err(format!(
                "an added server cannot be named `{}`, like the agent: their skills would share names",
                server.name
            ));
        }
        if !folders.insert(server.name.clone()) {
            return Err(format!("two servers are both named `{}`", server.name));
        }
    }
    Ok(folders)
}

async fn list_home(server: &McpServer, client: &Arc<Client>) -> Result<Listed, String> {
    within_connect_timeout(
        "list its tools",
        list(server.clone(), Arc::clone(client), true, Owner::HOME),
    )
    .await
    .map_err(|error| format!("MCP server `{}`: {error}", server.name))?
}

/// The toolset from the listed home and the attempts to reach each added
/// server. A server whose attempt failed, or whose tool names clash with the
/// home's, is left out; one that did not answer is probed.
fn assemble(
    home: HomeConnection,
    home_listed: Listed,
    attempts: Vec<(McpServer, Result<Listed, Left>)>,
    folders: HashSet<String>,
) -> Result<Toolset, String> {
    let retry = Arc::new(AtomicBool::new(false));
    let answered = Arc::new(Mutex::new(HashSet::new()));
    let home_names: HashSet<&str> = home_listed
        .tools
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect();
    let mut servers = Vec::new();
    let mut unavailable = Vec::new();
    let mut invalid = HashSet::new();
    let mut probes = Vec::new();
    for (server, attempt) in attempts {
        let attempt = attempt.and_then(|listed| {
            match listed
                .tools
                .iter()
                .find(|(name, _, _)| home_names.contains(name.as_str()))
            {
                Some((name, _, _)) => Err(Left::Invalid(format!(
                    "its tool `{name}` has the same name as one of the home's"
                ))),
                None => Ok(listed),
            }
        });
        match attempt {
            Ok(listed) => servers.push(listed),
            Err(left) => {
                tracing::warn!(server = %server.name, error = %left, "MCP server left out of the agent's tools");
                unavailable.push(server.name.clone());
                match left {
                    Left::Unreachable(_) => {
                        probes.push(probe(server, Arc::clone(&retry), Arc::clone(&answered)));
                    }
                    Left::Invalid(_) => {
                        invalid.insert(server.name);
                    }
                }
            }
        }
    }
    servers.insert(0, home_listed);
    let Indexed {
        tools,
        remotes,
        mut offered,
        served,
    } = index(&servers)?;
    offered.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(Toolset {
        tools,
        skills: offered,
        unavailable,
        invalid,
        answered,
        home,
        servers,
        remotes: Arc::new(remotes),
        folders: Arc::new(folders),
        served: Arc::new(served),
        verified: skills::Verified::default(),
        retry,
        limits: CallLimits::default(),
        probes,
    })
}

/// The connected servers' tools and skills, gathered under the names the
/// model sees.
#[derive(Default)]
struct Indexed {
    tools: Vec<ToolDef>,
    remotes: HashMap<String, Remote>,
    offered: Vec<Skill>,
    served: HashMap<String, skills::Served>,
}

fn index(servers: &[Listed]) -> Result<Indexed, String> {
    let mut indexed = Indexed::default();
    for listed in servers {
        for (name, def, remote) in &listed.tools {
            if indexed
                .remotes
                .insert(name.clone(), remote.clone())
                .is_some()
            {
                return Err(format!("two tools are both named `{name}`"));
            }
            indexed.tools.push(def.clone());
        }
        for (skill, entry) in &listed.skills {
            indexed.served.insert(skill.name.clone(), entry.clone());
            indexed.offered.push(skill.clone());
        }
    }
    Ok(indexed)
}

async fn within_connect_timeout<T>(
    what: &str,
    work: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    tokio::time::timeout(CONNECT_TIMEOUT, work)
        .await
        .map_err(|_| {
            format!(
                "did not {what} within {} seconds",
                CONNECT_TIMEOUT.as_secs()
            )
        })
}

/// Connects an added server and lists and checks its tools. Any failure,
/// including a tool newt cannot offer, leaves the server out of the turn.
async fn connect_added(server: &McpServer) -> Result<Listed, Left> {
    let client = within_connect_timeout("connect", connect(server))
        .await
        .and_then(|connected| connected)
        .map_err(Left::Unreachable)?;
    let speaks_newt = client.peer_info().is_some_and(|info| {
        info.capabilities
            .extensions
            .as_ref()
            .is_some_and(|extensions| extensions.contains_key(NEWT_EXTENSION))
    });
    within_connect_timeout(
        "list its tools",
        list(
            server.clone(),
            Arc::new(client),
            speaks_newt,
            Owner::added(&server.name),
        ),
    )
    .await
    .map_err(Left::Unreachable)?
    .map_err(Left::Invalid)
}

async fn list(
    server: McpServer,
    client: Arc<Client>,
    speaks_newt: bool,
    owner: Owner,
) -> Result<Listed, String> {
    let stale = Arc::new(AtomicBool::new(false));
    let mut tools = client
        .list_all_tools()
        .await
        .map_err(|error| format!("MCP server `{}`: listing tools: {error}", server.name))?;
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let tools = tools
        .into_iter()
        .map(|tool| offer(tool, &server.name, &client, speaks_newt, &owner, &stale))
        .collect::<Result<Vec<_>, String>>()?;
    let watcher = watch_tool_list(&server.name, &client, &stale)
        .await
        .map(Arc::new);
    let skills = skills::list(&server.name, &client, speaks_newt, &owner, &stale).await?;
    tracing::info!(server = %server.name, tools = tools.len(), skills = skills.len(), speaks_newt, "MCP server connected");
    Ok(Listed {
        server,
        client,
        tools,
        skills,
        speaks_newt,
        owner,
        stale,
        watcher,
    })
}

/// A tool as the model will see it: its name, its definition and where calls
/// to it go. Fails for a tool newt cannot offer.
fn offer(
    tool: Tool,
    server: &str,
    client: &Arc<Client>,
    speaks_newt: bool,
    owner: &Owner,
    stale: &Arc<AtomicBool>,
) -> Result<(String, ToolDef, Remote), String> {
    let tool_name = tool.name.to_string();
    let name = match &owner.0 {
        Some(prefix) => format!("{prefix}{TOOL_SEPARATOR}{tool_name}"),
        None => tool_name.clone(),
    };
    if name.len() > MAX_TOOL_NAME {
        return Err(format!(
            "MCP server `{server}`: tool name `{name}` is longer than the {MAX_TOOL_NAME} characters model APIs accept"
        ));
    }
    let def = tool_def(tool, &name, speaks_newt, owner)
        .map_err(|error| format!("MCP server `{server}`, tool `{tool_name}`: {error}"))?;
    let remote = Remote {
        client: Arc::clone(client),
        server: server.to_owned(),
        tool: tool_name,
        speaks_newt,
        owner: owner.clone(),
        stale: Arc::clone(stale),
    };
    Ok((name, def, remote))
}

/// Keeps trying a server that was down, the same way the toolset connects
/// it. Once it connects and lists cleanly, the toolset is marked stale, so
/// the next turn reconnects with it.
fn probe(
    server: McpServer,
    retry: Arc<AtomicBool>,
    answered: Arc<Mutex<HashSet<String>>>,
) -> AbortOnDrop {
    let task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(PROBE_INTERVAL).await;
            if retry.load(Ordering::Relaxed) {
                return;
            }
            if connect_added(&server).await.is_ok() {
                tracing::info!(server = %server.name, "MCP server answers again; reconnecting before the next turn");
                if let Ok(mut answered) = answered.lock() {
                    answered.insert(server.name.clone());
                }
                retry.store(true, Ordering::Relaxed);
                return;
            }
        }
    });
    AbortOnDrop(task.abort_handle())
}

async fn watch_tool_list(
    server: &str,
    client: &Client,
    stale: &Arc<AtomicBool>,
) -> Option<AbortOnDrop> {
    let filter = SubscriptionFilter::builder().tools_list_changed().build();
    let mut subscription = match client.peer().listen(filter).await {
        Ok(subscription) if subscription.acknowledged().tools_list_changed == Some(true) => {
            subscription
        }
        Ok(_) => {
            tracing::debug!(server, "MCP server does not announce tool changes");
            return None;
        }
        Err(error) => {
            tracing::debug!(server, %error, "MCP server offers no change notifications");
            return None;
        }
    };
    let server = server.to_owned();
    let stale = Arc::clone(stale);
    let task = tokio::spawn(async move {
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
    Some(AbortOnDrop(task.abort_handle()))
}

fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::LATEST],
        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
    }
}

async fn connect(server: &McpServer) -> Result<Client, String> {
    McpServer::check_url(&server.url)
        .map_err(|error| format!("MCP server `{}`: {error}", server.name))?;
    if let Some(command) = server.url.strip_prefix(STDIO_SCHEME) {
        return connect_stdio(server, command.trim()).await;
    }
    let mut config = StreamableHttpClientTransportConfig::with_uri(server.url.as_str())
        .max_concurrent_requests(MAX_CALLS_IN_FLIGHT);
    if let McpAuth::Bearer(token) = &server.auth {
        config = config.auth_header(token.as_str());
    }
    Handler::default()
        .serve_with_lifecycle(
            StreamableHttpClientTransport::from_config(config),
            lifecycle(),
        )
        .await
        .map_err(|error| format!("MCP server `{}`: connecting: {error}", server.name))
}

/// Starts the server's command and speaks MCP over its stdin and stdout. Its
/// stderr goes to newt's. Dropping the last connection stops it: rmcp closes
/// its stdin, waits, then sends SIGTERM and finally kills it.
async fn connect_stdio(server: &McpServer, command: &str) -> Result<Client, String> {
    let mut shell = tokio::process::Command::new("sh");
    shell.arg("-c").arg(format!("exec {command}"));
    let transport = TokioChildProcess::new(shell).map_err(|error| {
        format!(
            "MCP server `{}`: starting `{command}`: {error}",
            server.name
        )
    })?;
    Handler::default()
        .serve_with_lifecycle(transport, lifecycle())
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

/// A tool as the model sees it. A server that speaks newt decides its tool's
/// retention and invalidations; any other server's tools are `durable` and
/// invalidate nothing, whatever their `_meta` says.
fn tool_def(tool: Tool, name: &str, speaks_newt: bool, owner: &Owner) -> Result<ToolDef, String> {
    let (retention, invalidates) = if speaks_newt {
        let meta = tool.meta.as_deref();
        let retention: Retention = meta_field(meta, META_RETENTION)?.unwrap_or(Retention::Durable);
        let invalidates: Vec<PageKey> = meta_field(meta, META_INVALIDATES)?.unwrap_or_default();
        let idempotent: bool = meta_field(meta, META_IDEMPOTENT)?.unwrap_or(false);
        if retention == Retention::Outward && !idempotent {
            return Err(format!(
                "outward tools must set `_meta.{META_IDEMPOTENT}: true` and honour `_meta.{META_IDEMPOTENCY_KEY}`, or a crash could send twice"
            ));
        }
        (retention, invalidates)
    } else {
        (Retention::Durable, Vec::new())
    };
    let mut def = ToolDef::new(
        ToolSpec {
            name: name.to_owned(),
            description: tool
                .description
                .map(|text| text.to_string())
                .unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
        },
        retention,
    );
    def.invalidates = invalidates.into_iter().map(|key| owner.key(key)).collect();
    Ok(def)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names_are_short_lowercase_words() {
        assert!(check_server_name("paraform").is_ok());
        assert!(check_server_name("crm-2").is_ok());
        for bad in ["", "Paraform", "-x", "a/b", "a__b", &"x".repeat(33)] {
            assert!(check_server_name(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn urls_are_http_or_a_command() {
        assert!(McpServer::check_url("https://x.example/mcp").is_ok());
        assert!(McpServer::check_url("http://localhost:4000/mcp").is_ok());
        assert!(McpServer::check_url("stdio:node server.js").is_ok());
        assert!(McpServer::check_url("stdio:  ").is_err());
        assert!(McpServer::check_url("localhost:4000").is_err());
    }

    #[test]
    fn an_added_server_owns_a_folder_and_a_key_scope() {
        let home = Owner::HOME;
        let added = Owner::added("paraform");
        assert_eq!(home.path("tasks.md"), "tasks.md");
        assert_eq!(added.path("jobs/sre.md"), "paraform/jobs/sre.md");
        assert_eq!(home.key(vec!["tasks".into()]), vec!["tasks".to_owned()]);
        assert_eq!(
            added.key(vec!["jobs".into()]),
            vec!["\u{0}paraform".to_owned(), "jobs".to_owned()]
        );
        let scoped = added.key(vec!["jobs".into()]);
        assert!(added.may_own(&scoped));
        assert!(!home.may_own(&scoped));
        assert!(!added.may_own(&["paraform".to_owned()]));
        assert!(home.may_own(&[]) && added.may_own(&[]));
    }
}
