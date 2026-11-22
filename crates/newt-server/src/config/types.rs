use crate::cache_keys::Buckets;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tracing {
    pub endpoint: String,
    #[serde(default)]
    pub profile: newt_core::otlp::Profile,
    #[serde(default)]
    pub headers_env: Option<String>,
    #[serde(default = "default_true")]
    pub capture_content: bool,
    #[serde(default = "default_service_name")]
    pub service_name: String,
}

pub(super) const fn default_true() -> bool {
    true
}

pub(super) fn default_service_name() -> String {
    "newt".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listen {
    pub listen: SocketAddr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub url_env: Option<String>,
    pub embedded: Option<PathBuf>,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

pub(super) const fn default_max_connections() -> u32 {
    10
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Events {
    pub jsonl: Option<PathBuf>,
    #[serde(default)]
    pub capture_content: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runner {
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    #[serde(default)]
    pub reserved_interactive: Option<usize>,
    #[serde(default)]
    pub max_waiting: Option<usize>,
    #[serde(default = "default_cache_key_buckets")]
    pub cache_key_buckets: Buckets,
}

impl Runner {
    /// The configured `max_waiting`, or four waiting sessions per slot.
    #[must_use]
    pub fn max_waiting(&self) -> usize {
        self.max_waiting
            .unwrap_or_else(|| crate::runner::default_max_waiting(self.max_sessions))
    }

    /// The configured `reserved_interactive`, or a quarter of the slots.
    #[must_use]
    pub fn reserved_interactive(&self) -> usize {
        self.reserved_interactive
            .unwrap_or_else(|| crate::runner::default_reserved_interactive(self.max_sessions))
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self {
            max_sessions: default_max_sessions(),
            reserved_interactive: None,
            max_waiting: None,
            cache_key_buckets: default_cache_key_buckets(),
        }
    }
}

pub(super) const fn default_cache_key_buckets() -> Buckets {
    Buckets::AUTO
}

pub(super) const fn default_max_sessions() -> usize {
    64
}

/// Days to keep consumed events and live feed rows; 0 keeps them forever.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionDays {
    #[serde(default = "default_inbox_days")]
    pub inbox_days: u32,
    #[serde(default = "default_live_days")]
    pub live_days: u32,
}

impl Default for RetentionDays {
    fn default() -> Self {
        Self {
            inbox_days: default_inbox_days(),
            live_days: default_live_days(),
        }
    }
}

impl RetentionDays {
    #[must_use]
    pub fn retention(&self) -> crate::retention::Retention {
        let days = |count: u32| {
            (count > 0).then(|| std::time::Duration::from_secs(u64::from(count) * 86_400))
        };
        crate::retention::Retention {
            inbox: days(self.inbox_days),
            live: days(self.live_days),
        }
    }
}

pub(super) const fn default_inbox_days() -> u32 {
    30
}

pub(super) const fn default_live_days() -> u32 {
    7
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub protocol: Protocol,
    pub model: String,
    pub base_url: String,
    pub key_env: String,
    #[serde(default)]
    pub api_key_header: Option<String>,
    #[serde(default)]
    pub headers_env: Option<String>,
    pub max_concurrent: u32,
    #[serde(default = "default_replay_reasoning")]
    pub replay_reasoning: bool,
}

pub(super) const fn default_replay_reasoning() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    AnthropicMessages,
    OpenaiResponses,
    OpenaiChat,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChain {
    pub chain: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// The agent's home MCP server: `https://…`, `http://…` or
    /// `stdio:<command>`.
    pub mcp: String,
    pub token_env: String,
    /// Put the event's `meta` on its live `received` frame.
    #[serde(default)]
    pub live_meta: bool,
    /// Stream the model's answer text and tool-call arguments on the live feed.
    #[serde(default)]
    pub live_stream: bool,
    /// Unconsumed events one session of this agent may hold.
    #[serde(default = "default_max_pending_events")]
    pub max_pending_events: usize,
}

pub(super) const fn default_max_pending_events() -> usize {
    crate::store::DEFAULT_MAX_PENDING_EVENTS
}
