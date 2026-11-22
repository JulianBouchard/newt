use crate::cache_keys::Buckets;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub server: Listen,
    pub database: Database,
    #[serde(default)]
    pub events: Events,
    #[serde(default)]
    pub runner: Runner,
    #[serde(default)]
    pub retention: RetentionDays,
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelChain>,
    #[serde(default)]
    pub agents: BTreeMap<String, Agent>,
    #[serde(default)]
    pub tracing: Option<Tracing>,
}

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

const fn default_true() -> bool {
    true
}

fn default_service_name() -> String {
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

const fn default_max_connections() -> u32 {
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

const fn default_cache_key_buckets() -> Buckets {
    Buckets::AUTO
}

const fn default_max_sessions() -> usize {
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

const fn default_inbox_days() -> u32 {
    30
}

const fn default_live_days() -> u32 {
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

const fn default_replay_reasoning() -> bool {
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

const fn default_max_pending_events() -> usize {
    crate::store::DEFAULT_MAX_PENDING_EVENTS
}

impl ServerConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("invalid config {}", path.display()))
    }

    fn parse(raw: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(raw)?;
        config.check_routes()?;
        config.check_model_chains()?;
        config.check_agents()?;
        if config.database.url_env.is_some() == config.database.embedded.is_some() {
            bail!("database needs exactly one of url_env and embedded");
        }
        if config.runner.cache_key_buckets == Buckets::Fixed(0) {
            bail!("runner cache_key_buckets must be at least 1");
        }
        if config.runner.max_sessions == 0 {
            bail!("runner max_sessions must be at least 1");
        }
        if config.runner.max_waiting() == 0 {
            bail!("runner max_waiting must be at least 1");
        }
        if config.runner.reserved_interactive() >= config.runner.max_sessions
            && config.runner.max_sessions > 1
        {
            bail!("runner reserved_interactive must be less than max_sessions");
        }
        if let Some(tracing) = &config.tracing
            && !is_http_url(&tracing.endpoint)
        {
            bail!("tracing endpoint must start with http:// or https://");
        }
        Ok(config)
    }

    pub fn missing_secrets(&self) -> Vec<&str> {
        let route_keys = self.routes.values().flat_map(|route| {
            std::iter::once(route.key_env.as_str()).chain(route.headers_env.as_deref())
        });
        let agent_tokens = self.agents.values().map(|agent| agent.token_env.as_str());
        let tracing_headers = self
            .tracing
            .iter()
            .filter_map(|tracing| tracing.headers_env.as_deref());
        route_keys
            .chain(agent_tokens)
            .chain(tracing_headers)
            .filter(|name| std::env::var_os(name).is_none())
            .collect()
    }

    fn check_routes(&self) -> anyhow::Result<()> {
        for (name, route) in &self.routes {
            if !is_http_url(&route.base_url) {
                bail!("route `{name}` base_url must start with http:// or https://");
            }
            if route.max_concurrent == 0 {
                bail!("route `{name}` max_concurrent must be at least 1");
            }
            if !route.replay_reasoning && route.protocol != Protocol::OpenaiResponses {
                bail!("route `{name}` replay_reasoning applies only to openai-responses routes");
            }
        }
        Ok(())
    }

    fn check_agents(&self) -> anyhow::Result<()> {
        for (name, agent) in &self.agents {
            if agent.max_pending_events == 0 {
                bail!("agent `{name}` max_pending_events must be at least 1");
            }
            if let Err(error) = crate::mcp::McpServer::check_url(&agent.mcp) {
                bail!("agent `{name}`: mcp {error}");
            }
        }
        Ok(())
    }

    fn check_model_chains(&self) -> anyhow::Result<()> {
        for (model, chain) in &self.models {
            if chain.chain.is_empty() {
                bail!("model `{model}` has an empty chain");
            }
            if let Some(missing) = chain
                .chain
                .iter()
                .find(|route| !self.routes.contains_key(*route))
            {
                bail!("model `{model}` names unknown route `{missing}`");
            }
        }
        Ok(())
    }
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    const EXAMPLE: &str = include_str!("../../../server.example.toml");

    #[test]
    fn the_example_config_parses() {
        let config = ServerConfig::parse(EXAMPLE).unwrap();
        assert_eq!(config.models["default"].chain, vec!["openai", "anthropic"]);
        assert_eq!(
            config.routes["anthropic"].protocol,
            Protocol::AnthropicMessages
        );
        assert!(config.agents.contains_key("assistant"));
    }

    #[test]
    fn a_chain_naming_an_unknown_route_is_rejected() {
        let raw = r#"
            server = { listen = "127.0.0.1:0" }
            database = { url_env = "X" }
            [models.default]
            chain = ["missing"]
        "#;
        let error = ServerConfig::parse(raw).unwrap_err().to_string();
        assert!(error.contains("unknown route `missing`"), "{error}");
    }

    #[test]
    fn a_route_with_zero_concurrency_is_rejected() {
        let raw = r#"
            server = { listen = "127.0.0.1:0" }
            database = { url_env = "X" }
            [routes.local]
            protocol = "openai-responses"
            model = "m"
            base_url = "http://localhost:3456"
            key_env = "K"
            max_concurrent = 0
        "#;
        let error = ServerConfig::parse(raw).unwrap_err().to_string();
        assert!(error.contains("max_concurrent"), "{error}");
    }

    #[test]
    fn replay_reasoning_off_is_rejected_outside_openai_responses() {
        let raw = r#"
            server = { listen = "127.0.0.1:0" }
            database = { url_env = "X" }
            [routes.local]
            protocol = "anthropic-messages"
            model = "m"
            base_url = "http://localhost:3456"
            key_env = "K"
            max_concurrent = 1
            replay_reasoning = false
        "#;
        let error = ServerConfig::parse(raw).unwrap_err().to_string();
        assert!(error.contains("replay_reasoning"), "{error}");
    }

    #[test]
    fn missing_secrets_lists_unset_route_keys_route_headers_and_agent_tokens() {
        let raw = r#"
            server = { listen = "127.0.0.1:0" }
            database = { url_env = "X" }
            [routes.local]
            protocol = "openai-responses"
            model = "m"
            base_url = "http://localhost:3456"
            key_env = "NEWT_TEST_SURELY_UNSET_ROUTE_KEY"
            headers_env = "NEWT_TEST_SURELY_UNSET_ROUTE_HEADERS"
            max_concurrent = 1
            [agents.demo]
            mcp = "http://localhost:4000/mcp"
            token_env = "NEWT_TEST_SURELY_UNSET_AGENT_TOKEN"
        "#;
        let config = ServerConfig::parse(raw).unwrap();
        assert_eq!(
            config.missing_secrets(),
            vec![
                "NEWT_TEST_SURELY_UNSET_ROUTE_KEY",
                "NEWT_TEST_SURELY_UNSET_ROUTE_HEADERS",
                "NEWT_TEST_SURELY_UNSET_AGENT_TOKEN"
            ]
        );
    }

    #[test]
    fn an_agent_is_a_home_mcp_server_reached_over_http_or_stdio() {
        let with = |agent: &str| {
            ServerConfig::parse(&format!(
                r#"
                server = {{ listen = "127.0.0.1:0" }}
                database = {{ url_env = "X" }}
                [agents.demo]
                token_env = "T"
                {agent}
                "#
            ))
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
        };
        assert_eq!(with(r#"mcp = "http://localhost:4000/mcp""#), Ok(()));
        assert_eq!(with(r#"mcp = "stdio:node ./agent/server.js""#), Ok(()));
        assert!(
            with(r#"mcp = "localhost:4000/mcp""#)
                .unwrap_err()
                .contains("must start with https://, http:// or stdio:")
        );
        assert!(with("").is_err(), "an agent needs its home server");
        assert!(
            with("mcp = \"http://localhost:4000/mcp\"\nmodel = \"default\"").is_err(),
            "the home names the model, not server.toml"
        );
    }

    #[test]
    fn max_pending_events_defaults_to_100_and_must_be_positive() {
        let agent = |extra: &str| {
            let raw = format!(
                "server = {{ listen = \"127.0.0.1:0\" }}\ndatabase = {{ url_env = \"X\" }}\n\
                 [routes.r]\nprotocol = \"openai-chat\"\nmodel = \"m\"\nbase_url = \"http://localhost:1\"\nkey_env = \"K\"\nmax_concurrent = 1\n\
                 [models.default]\nchain = [\"r\"]\n\
                 [agents.demo]\nmcp = \"http://localhost:4000/mcp\"\ntoken_env = \"T\"\n{extra}"
            );
            ServerConfig::parse(&raw)
        };
        assert_eq!(agent("").unwrap().agents["demo"].max_pending_events, 100);
        assert_eq!(
            agent("max_pending_events = 7").unwrap().agents["demo"].max_pending_events,
            7
        );
        let error = agent("max_pending_events = 0").unwrap_err().to_string();
        assert!(
            error.contains("max_pending_events must be at least 1"),
            "{error}"
        );
    }

    #[test]
    fn reserved_interactive_is_a_quarter_of_the_slots_and_less_than_all_of_them() {
        let runner = |max_sessions: usize, reserved: Option<usize>| Runner {
            max_sessions,
            reserved_interactive: reserved,
            ..Runner::default()
        };
        assert_eq!(runner(64, None).reserved_interactive(), 16);
        assert_eq!(runner(8, None).reserved_interactive(), 2);
        assert_eq!(runner(2, None).reserved_interactive(), 1);
        assert_eq!(runner(1, None).reserved_interactive(), 0);
        assert_eq!(runner(8, Some(5)).reserved_interactive(), 5);

        let raw = |extra: &str| {
            format!(
                "server = {{ listen = \"127.0.0.1:0\" }}\ndatabase = {{ url_env = \"X\" }}\n[runner]\nmax_sessions = 4\n{extra}"
            )
        };
        assert!(ServerConfig::parse(&raw("reserved_interactive = 3")).is_ok());
        let error = ServerConfig::parse(&raw("reserved_interactive = 4"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("reserved_interactive must be less than max_sessions"),
            "{error}"
        );
    }

    #[test]
    fn max_waiting_is_four_per_slot_unless_set_and_never_zero() {
        let runner = |max_sessions: usize, waiting: Option<usize>| Runner {
            max_sessions,
            max_waiting: waiting,
            ..Runner::default()
        };
        assert_eq!(runner(64, None).max_waiting(), 256);
        assert_eq!(runner(2, Some(5)).max_waiting(), 5);
        let raw = "server = { listen = \"127.0.0.1:0\" }\ndatabase = { url_env = \"X\" }\n[runner]\nmax_waiting = 0\n";
        let error = ServerConfig::parse(raw).unwrap_err().to_string();
        assert!(error.contains("max_waiting must be at least 1"), "{error}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let raw = r#"
            server = { listen = "127.0.0.1:0", port = 1 }
            database = { url_env = "X" }
        "#;
        assert!(ServerConfig::parse(raw).is_err());
    }
}
