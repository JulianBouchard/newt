use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use newt_core::skills::Skill;
use newt_core::tools::ToolDef;
use rmcp::model::{ServerNotification, SubscriptionFilter};

use super::connection::{Client, connect, within_connect_timeout};
use super::tools::offer;
use super::toolset::Left;
use super::{McpServer, NEWT_EXTENSION, Owner, PROBE_INTERVAL, Remote, STDIO_SCHEME, skills};

pub(super) struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One server's tools, under the names the model sees, and its skills, once
/// it is connected, listed and checked.
#[derive(Clone)]
pub(super) struct Listed {
    pub(super) server: McpServer,
    pub(super) client: Arc<Client>,
    pub(super) tools: Vec<(String, ToolDef, Remote)>,
    pub(super) skills: Vec<(Skill, skills::Served)>,
    pub(super) speaks_newt: bool,
    pub(super) owner: Owner,
    /// Set when the server says its tools changed, its change notifications
    /// stop, or a call finds its connection broken.
    stale: Arc<AtomicBool>,
    /// The task that sets `stale` when the server announces a tool change.
    watcher: Option<Arc<AbortOnDrop>>,
}

impl Listed {
    /// Still connected, and nothing it said since it was listed changes it.
    pub(super) fn is_current(&self) -> bool {
        !self.stale.load(Ordering::Relaxed) && !self.client.is_closed()
    }

    pub(super) fn offers_resources(&self) -> bool {
        self.client
            .peer_info()
            .is_some_and(|info| info.capabilities.resources.is_some())
    }

    /// A server newt runs over stdio keeps its state in its process, so while
    /// the process lives newt lists it again on the same connection instead
    /// of starting a new one.
    pub(super) fn keeps_its_process(&self) -> bool {
        self.server.url.starts_with(STDIO_SCHEME) && !self.client.is_closed()
    }

    pub(super) async fn relist(&self) -> Result<Self, String> {
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

pub(super) async fn list_home(server: &McpServer, client: &Arc<Client>) -> Result<Listed, String> {
    within_connect_timeout(
        "list its tools",
        list(server.clone(), Arc::clone(client), true, Owner::HOME),
    )
    .await
    .map_err(|error| format!("MCP server `{}`: {error}", server.name))?
}

/// Connects an added server and lists and checks its tools. Any failure,
/// including a tool newt cannot offer, leaves the server out of the turn.
pub(super) async fn connect_added(server: &McpServer) -> Result<Listed, Left> {
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

pub(super) async fn list(
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

/// Keeps trying a server that was down, the same way the toolset connects
/// it. Once it connects and lists cleanly, the toolset is marked stale, so
/// the next turn reconnects with it.
pub(super) fn probe(
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

pub(super) async fn watch_tool_list(
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
