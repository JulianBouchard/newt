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
    pub routes: BTreeMap<String, Route>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelChain>,
    #[serde(default)]
    pub agents: BTreeMap<String, Agent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listen {
    pub listen: SocketAddr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub url_env: String,
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
    pub max_concurrent: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    AnthropicMessages,
    OpenaiResponses,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChain {
    pub chain: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub context_provider: String,
    pub token_env: String,
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
        Ok(config)
    }

    pub fn missing_secrets(&self) -> Vec<&str> {
        let route_keys = self.routes.values().map(|route| route.key_env.as_str());
        let agent_tokens = self.agents.values().map(|agent| agent.token_env.as_str());
        route_keys
            .chain(agent_tokens)
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
        }
        Ok(())
    }

    fn check_agents(&self) -> anyhow::Result<()> {
        for (name, agent) in &self.agents {
            if !is_http_url(&agent.context_provider) {
                bail!("agent `{name}` context_provider must start with http:// or https://");
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
    fn missing_secrets_lists_unset_route_keys_and_agent_tokens() {
        let raw = r#"
            server = { listen = "127.0.0.1:0" }
            database = { url_env = "X" }
            [routes.local]
            protocol = "openai-responses"
            model = "m"
            base_url = "http://localhost:3456"
            key_env = "NEWT_TEST_SURELY_UNSET_ROUTE_KEY"
            max_concurrent = 1
            [agents.demo]
            context_provider = "http://localhost:4000"
            token_env = "NEWT_TEST_SURELY_UNSET_AGENT_TOKEN"
        "#;
        let config = ServerConfig::parse(raw).unwrap();
        assert_eq!(
            config.missing_secrets(),
            vec![
                "NEWT_TEST_SURELY_UNSET_ROUTE_KEY",
                "NEWT_TEST_SURELY_UNSET_AGENT_TOKEN"
            ]
        );
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
