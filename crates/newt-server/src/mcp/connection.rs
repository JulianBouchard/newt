use std::sync::Arc;

use futures_util::future::join_all;
use rmcp::handler::client::progress::ProgressDispatcher;
use rmcp::model::{
    ClientConfig, ElicitationCapability, FormElicitationCapability, JsonObject,
    ProgressNotificationParam, ProtocolVersion, TASKS_EXTENSION_ID, UrlElicitationCapability,
};
use rmcp::service::NotificationContext;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient};
use serde_json::Value;

use super::listing::{connect_added, list_home};
use super::toolset::{Toolset, assemble, check_added};
use super::{
    CONNECT_TIMEOUT, MAX_CALLS_IN_FLIGHT, McpAuth, McpServer, NEWT_EXTENSION, STDIO_SCHEME,
};

pub(super) type Client = RunningService<RoleClient, Handler>;

/// Newt's side of each MCP connection: hands progress notifications to the
/// call that is waiting for them.
#[derive(Debug, Clone, Default)]
pub(super) struct Handler {
    pub(super) progress: ProgressDispatcher,
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

/// A connection to the home server, with what it declared when newt
/// connected: its instructions and its `newt` extension.
#[derive(Clone)]
pub struct HomeConnection {
    pub(super) server: McpServer,
    pub(super) client: Arc<Client>,
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
        super::call::custom(&self.client, &self.server.name, method, params).await
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

pub(super) async fn within_connect_timeout<T>(
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

pub(super) fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::LATEST],
        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
    }
}

pub(super) async fn connect(server: &McpServer) -> Result<Client, String> {
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
            StreamableHttpClientTransport::with_client(http_client(), config),
            lifecycle(),
        )
        .await
        .map_err(|error| format!("MCP server `{}`: connecting: {error}", server.name))
}

/// The one HTTP client every MCP connection shares, so requests to a
/// server reuse its pooled connections instead of opening one each (rmcp's
/// own client turns pooling off). Redirects follow rmcp's rule: only a
/// same-origin 307 or 308, at most ten.
fn http_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let redirects = reqwest::redirect::Policy::custom(|attempt| {
                let keeps_body = matches!(
                    attempt.status(),
                    reqwest::StatusCode::TEMPORARY_REDIRECT
                        | reqwest::StatusCode::PERMANENT_REDIRECT
                );
                let same_origin = attempt
                    .previous()
                    .first()
                    .is_some_and(|original| original.origin() == attempt.url().origin());
                if !keeps_body || !same_origin {
                    attempt.stop()
                } else if attempt.previous().len() > 10 {
                    attempt.error("more than 10 redirects")
                } else {
                    attempt.follow()
                }
            });
            #[expect(
                clippy::expect_used,
                reason = "building a client with no TLS or proxy settings of its own cannot fail"
            )]
            reqwest::Client::builder()
                .tcp_nodelay(true)
                .redirect(redirects)
                .build()
                .expect("the MCP HTTP client builds")
        })
        .clone()
}

/// Starts the server's command and speaks MCP over its stdin and stdout. Its
/// stderr goes to newt's. Dropping the last connection stops it: rmcp closes
/// its stdin, waits, then sends SIGTERM and finally kills it.
pub(super) async fn connect_stdio(server: &McpServer, command: &str) -> Result<Client, String> {
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
