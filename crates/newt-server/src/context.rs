use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use newt_core::prompt::Effort;
use newt_core::workspace::{Page, PageFuture, PageKey, PageSource, Workspace};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::backend::{Backend, BackendError, Prepared};
use crate::mcp::{McpHost, McpServer, Toolset};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_BODY_LIMIT: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub version: String,
    pub instructions: String,
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
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug)]
struct Loaded {
    bootstrap: Bootstrap,
    toolset: Toolset,
}

const fn default_max_output_tokens() -> u32 {
    4096
}

const fn default_max_model_calls() -> u32 {
    8
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnContext {
    version: String,
    pages: Vec<Page>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    constraints: Vec<String>,
}

impl TurnContext {
    fn render(&self) -> Option<String> {
        let instructions = self
            .instructions
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let constraints: Vec<&str> = self
            .constraints
            .iter()
            .map(|constraint| constraint.trim())
            .filter(|constraint| !constraint.is_empty())
            .collect();
        if instructions.is_none() && constraints.is_empty() {
            return None;
        }
        let mut out = String::from("<turn_context>\n");
        if let Some(instructions) = instructions {
            out.push_str("<instructions>\n");
            out.push_str(instructions);
            out.push_str("\n</instructions>\n");
        }
        if !constraints.is_empty() {
            out.push_str("<active_constraints>\n");
            for constraint in constraints {
                out.push_str("- ");
                out.push_str(constraint);
                out.push('\n');
            }
            out.push_str("</active_constraints>\n");
        }
        out.push_str("</turn_context>");
        Some(out)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PagesResponse {
    pages: Vec<Page>,
}

#[derive(Serialize)]
struct PagesRequest<'a> {
    session: &'a str,
    keys: &'a [PageKey],
}

#[derive(Clone)]
pub struct ProviderEndpoint {
    pub base_url: String,
    pub token: String,
}

impl std::fmt::Debug for ProviderEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderEndpoint")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
struct Client {
    http: reqwest::Client,
    endpoint: Arc<ProviderEndpoint>,
}

impl Client {
    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.endpoint.base_url.trim_end_matches('/'))
    }

    async fn call<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, String> {
        let request = match body {
            Some(body) => self.http.post(self.url(path)).json(&body),
            None => self.http.get(self.url(path)),
        };
        let response = request
            .bearer_auth(&self.endpoint.token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("{path}: {}", error.without_url()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| format!("{path}: reading the body failed: {}", error.without_url()))?;
        if !status.is_success() {
            let snippet: String = text.chars().take(ERROR_BODY_LIMIT).collect();
            return Err(format!("{path}: HTTP {status}: {snippet}"));
        }
        serde_json::from_str(&text).map_err(|error| format!("{path}: invalid response: {error}"))
    }
}

struct ProviderPages {
    client: Client,
    session: String,
}

impl PageSource for ProviderPages {
    fn fetch(&self, keys: Vec<PageKey>) -> PageFuture<'_> {
        Box::pin(async move {
            let body = serde_json::to_value(PagesRequest {
                session: &self.session,
                keys: &keys,
            })
            .map_err(|error| error.to_string())?;
            let response: PagesResponse = self.client.call("pages", Some(body)).await?;
            check_pages(&response.pages)?;
            Ok(response.pages)
        })
    }
}

pub struct ContextProviderBackend {
    http: reqwest::Client,
    agents: HashMap<String, Arc<ProviderEndpoint>>,
    models: HashSet<String>,
    loaded: HashMap<String, tokio::sync::Mutex<Option<Arc<Loaded>>>>,
}

impl std::fmt::Debug for ContextProviderBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContextProviderBackend")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ContextProviderBackend {
    pub fn new(
        agents: BTreeMap<String, ProviderEndpoint>,
        models: impl IntoIterator<Item = String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            loaded: agents
                .keys()
                .map(|name| (name.clone(), tokio::sync::Mutex::new(None)))
                .collect(),
            agents: agents
                .into_iter()
                .map(|(name, endpoint)| (name, Arc::new(endpoint)))
                .collect(),
            models: models.into_iter().collect(),
        })
    }

    async fn load(
        &self,
        agent: &str,
        client: &Client,
        version: &str,
    ) -> Result<Arc<Loaded>, String> {
        let slot = self
            .loaded
            .get(agent)
            .ok_or_else(|| format!("agent `{agent}` is not in server.toml"))?;
        let mut slot = slot.lock().await;
        if let Some(loaded) = slot
            .as_ref()
            .filter(|loaded| loaded.bootstrap.version == version && !loaded.toolset.is_broken())
        {
            return Ok(Arc::clone(loaded));
        }
        let bootstrap: Bootstrap = client.call("bootstrap", None).await?;
        self.check_bootstrap(&bootstrap)?;
        if bootstrap.version != version {
            return Err(format!(
                "bootstrap is version `{}` but the turn context asked for `{version}`",
                bootstrap.version
            ));
        }
        let toolset = Toolset::connect(&bootstrap.mcp_servers, &client.endpoint.token).await?;
        tracing::info!(
            agent,
            version,
            tools = toolset.tools.len(),
            "loaded agent bootstrap"
        );
        let loaded = Arc::new(Loaded { bootstrap, toolset });
        *slot = Some(Arc::clone(&loaded));
        Ok(loaded)
    }

    fn check_bootstrap(&self, bootstrap: &Bootstrap) -> Result<(), String> {
        if bootstrap.version.trim().is_empty() {
            return Err("bootstrap version is empty".to_owned());
        }
        if bootstrap.instructions.trim().is_empty() {
            return Err("bootstrap instructions are empty".to_owned());
        }
        if !self.models.contains(&bootstrap.model) {
            return Err(format!(
                "bootstrap names model `{}`, which server.toml does not define",
                bootstrap.model
            ));
        }
        if bootstrap.max_output_tokens == 0 || bootstrap.max_model_calls == 0 {
            return Err(
                "bootstrap max_output_tokens and max_model_calls must be at least 1".to_owned(),
            );
        }
        Ok(())
    }

    async fn prepare_turn(&self, agent: &str, key: &str) -> Result<Prepared<McpHost>, String> {
        let endpoint = self
            .agents
            .get(agent)
            .ok_or_else(|| format!("agent `{agent}` is not in server.toml"))?;
        let client = Client {
            http: self.http.clone(),
            endpoint: Arc::clone(endpoint),
        };
        let turn: TurnContext = client.call("turn", Some(json!({ "session": key }))).await?;
        check_pages(&turn.pages)?;
        let loaded = self.load(agent, &client, &turn.version).await?;
        let bootstrap = &loaded.bootstrap;
        let source = ProviderPages {
            client,
            session: key.to_owned(),
        };
        Ok(Prepared {
            system: bootstrap.instructions.clone(),
            tools: loaded.toolset.tools.clone(),
            context: turn.render(),
            workspace: Workspace::with_source(turn.pages, Arc::new(source)),
            model: bootstrap.model.clone(),
            first_call_effort: bootstrap.first_call_effort,
            effort: bootstrap.effort,
            max_output_tokens: bootstrap.max_output_tokens,
            max_model_calls: bootstrap.max_model_calls,
            host: loaded.toolset.host(key),
        })
    }
}

impl Backend for ContextProviderBackend {
    type Host = McpHost;

    async fn prepare(&self, agent: &str, key: &str) -> Result<Prepared<McpHost>, BackendError> {
        self.prepare_turn(agent, key)
            .await
            .map_err(|error| BackendError(format!("context provider for `{agent}`: {error}")))
    }
}

pub(crate) fn check_pages(pages: &[Page]) -> Result<(), String> {
    let mut paths = HashSet::new();
    for page in pages {
        if page.path.trim().is_empty() || page.key.is_empty() {
            return Err("every page needs a path and a key".to_owned());
        }
        if !paths.insert(page.path.as_str()) {
            return Err(format!("page path `{}` appears twice", page.path));
        }
    }
    Ok(())
}
