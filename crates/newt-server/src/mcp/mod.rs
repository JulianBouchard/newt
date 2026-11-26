//! Connecting to an agent's MCP servers and listing their tools: the home
//! server, which is the agent, and the servers it adds. One call's path lives
//! in `call.rs`, workspace pages in `pages.rs` and `server_pages.rs`.

mod call;
mod connection;
mod links;
mod listing;
mod mrtr;
mod pages;
#[cfg(test)]
mod props;
mod server_pages;
mod skills;
mod tools;
mod toolset;

pub use connection::HomeConnection;
pub use pages::META_KEY;
pub use server_pages::{ServerPages, check_pages};
pub use toolset::Toolset;

use connection::Client;
use tools::meta_field;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use newt_core::workspace::PageKey;

use crate::backend::{SessionState, TurnState};

pub const META_RETENTION: &str = "newt/retention";
pub const META_INVALIDATES: &str = "newt/invalidates";
pub const META_ENDS_TURN: &str = "newt/ends_turn";
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
/// Between an added server's name and its tool's name: `partner__search_orders`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names_are_short_lowercase_words() {
        assert!(check_server_name("partner").is_ok());
        assert!(check_server_name("crm-2").is_ok());
        for bad in ["", "Partner", "-x", "a/b", "a__b", &"x".repeat(33)] {
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
        let added = Owner::added("partner");
        assert_eq!(home.path("tasks.md"), "tasks.md");
        assert_eq!(added.path("orders/sre.md"), "partner/orders/sre.md");
        assert_eq!(home.key(vec!["tasks".into()]), vec!["tasks".to_owned()]);
        assert_eq!(
            added.key(vec!["orders".into()]),
            vec!["\u{0}partner".to_owned(), "orders".to_owned()]
        );
        let scoped = added.key(vec!["orders".into()]);
        assert!(added.may_own(&scoped));
        assert!(!home.may_own(&scoped));
        assert!(!added.may_own(&["partner".to_owned()]));
        assert!(home.may_own(&[]) && added.may_own(&[]));
    }
}
