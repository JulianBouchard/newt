use crate::backend::AgentSettings;
use crate::mcp::{McpAuth, McpServer};
use newt_core::prompt::Effort;
use newt_core::tools::BuiltinTools;
use serde::Deserialize;

/// Where an agent's home server is, from `server.toml`.
#[derive(Clone)]
pub struct HomeServer {
    pub url: String,
    /// The agent's token: sent to a home reached over HTTP, never over stdio.
    pub token: String,
}

impl std::fmt::Debug for HomeServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HomeServer")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// The agent's settings: the `newt` object in the home's declared extensions.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a field of the declared settings object, decoded as it is written"
)]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub model: String,
    #[serde(default)]
    pub first_call_effort: Option<Effort>,
    #[serde(default)]
    pub effort: Option<Effort>,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_max_model_calls")]
    pub max_model_calls: u32,
    #[serde(default)]
    pub max_turn_tokens: Option<u64>,
    #[serde(default)]
    pub recite: bool,
    /// Every turn must end with `end_turn` saying what it recorded; otherwise a reply
    /// with no tool calls ends the turn.
    #[serde(default)]
    pub require_end_turn: bool,
    /// Which built-in tools the agent is offered: `true` (all), `false` (none) or
    /// the names of the ones to keep.
    #[serde(default)]
    pub builtin_tools: BuiltinTools,
    #[serde(default = "default_history_budget")]
    pub history_budget: u32,
    /// Pages whose MCP `annotations.priority` is at least this are read in
    /// the background as each turn starts.
    #[serde(default = "default_preload_priority")]
    pub preload_priority: f32,
    /// How long a turn's first model call waits for the page index, in
    /// milliseconds; 0 leaves the index out.
    #[serde(default = "default_page_index_wait_ms")]
    pub page_index_wait_ms: u64,
    /// Ask the home with `newt/turn` before each turn.
    #[serde(default)]
    pub turn: bool,
    /// Send the home `newt/turn_end` after each committed turn.
    #[serde(default)]
    pub turn_end: bool,
    #[serde(default)]
    pub servers: Vec<AddedServer>,
}

impl Settings {
    /// How the agent's turns run, from its declared settings.
    pub fn agent(&self) -> AgentSettings {
        AgentSettings {
            model: self.model.clone(),
            first_call_effort: self.first_call_effort,
            effort: self.effort,
            max_output_tokens: self.max_output_tokens,
            max_model_calls: self.max_model_calls,
            max_turn_tokens: self.max_turn_tokens,
            recite: self.recite,
            require_end_turn: self.require_end_turn,
            builtin_tools: self.builtin_tools,
            page_index_wait: std::time::Duration::from_millis(self.page_index_wait_ms),
            history_budget: self.history_budget,
        }
    }
}

/// A server the home adds to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddedServer {
    pub name: String,
    pub url: String,
    /// The environment variable that holds the server's bearer token.
    #[serde(default)]
    pub auth_env: Option<String>,
}

pub(super) const fn default_history_budget() -> u32 {
    60_000
}

pub(super) const fn default_preload_priority() -> f32 {
    1.0
}

pub(super) const fn default_page_index_wait_ms() -> u64 {
    2_000
}

pub(super) const fn default_max_output_tokens() -> u32 {
    4096
}

pub(super) const fn default_max_model_calls() -> u32 {
    8
}

/// The servers the home adds, with their tokens read from the environment.
pub(super) fn resolve(servers: &[AddedServer]) -> Result<Vec<McpServer>, String> {
    servers
        .iter()
        .map(|server| {
            McpServer::check_url(&server.url)
                .map_err(|error| format!("server `{}`: {error}", server.name))?;
            let auth = match &server.auth_env {
                Some(variable) => {
                    let token = std::env::var(variable).map_err(|_| {
                        format!(
                            "server `{}`: environment variable {variable} is not set",
                            server.name
                        )
                    })?;
                    if token.trim().is_empty() {
                        return Err(format!(
                            "server `{}`: environment variable {variable} is empty",
                            server.name
                        ));
                    }
                    McpAuth::Bearer(token)
                }
                None => McpAuth::None,
            };
            Ok(McpServer {
                name: server.name.clone(),
                url: server.url.clone(),
                auth,
            })
        })
        .collect()
}
