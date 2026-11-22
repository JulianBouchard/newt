use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use newt_core::skills::Skill;
use newt_core::tools::ToolDef;

use super::connection::HomeConnection;
use super::listing::{AbortOnDrop, Listed, connect_added, list_home, probe};
use super::{
    CallLimits, McpHost, McpServer, Remote, ServerPages, check_server_name, pages, skills,
};
use crate::backend::{SessionState, TurnState};

/// Why an added server is left out of a toolset.
pub(super) enum Left {
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
pub(super) fn check_added(agent: &str, added: &[McpServer]) -> Result<HashSet<String>, String> {
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

/// The toolset from the listed home and the attempts to reach each added
/// server. A server whose attempt failed, or whose tool names clash with the
/// home's, is left out; one that did not answer is probed.
pub(super) fn assemble(
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
pub(super) struct Indexed {
    tools: Vec<ToolDef>,
    remotes: HashMap<String, Remote>,
    offered: Vec<Skill>,
    served: HashMap<String, skills::Served>,
}

pub(super) fn index(servers: &[Listed]) -> Result<Indexed, String> {
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
