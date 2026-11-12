use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use newt_core::prompt::Effort;
use newt_core::tools::NoTools;
use newt_core::workspace::{Page, PageFuture, PageKey, PageSource, Workspace};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::backend::{Backend, BackendError, Prepared};

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

#[derive(Debug, Clone)]
pub struct ProviderEndpoint {
    pub base_url: String,
    pub token: String,
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
    bootstraps: Mutex<HashMap<String, Arc<Bootstrap>>>,
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
            agents: agents
                .into_iter()
                .map(|(name, endpoint)| (name, Arc::new(endpoint)))
                .collect(),
            models: models.into_iter().collect(),
            bootstraps: Mutex::new(HashMap::new()),
        })
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<String, Arc<Bootstrap>>> {
        match self.bootstraps.lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    async fn bootstrap(
        &self,
        agent: &str,
        client: &Client,
        version: &str,
    ) -> Result<Arc<Bootstrap>, String> {
        if let Some(cached) = self
            .cache()
            .get(agent)
            .filter(|cached| cached.version == version)
        {
            return Ok(Arc::clone(cached));
        }
        let bootstrap: Bootstrap = client.call("bootstrap", None).await?;
        self.check_bootstrap(&bootstrap)?;
        if bootstrap.version != version {
            return Err(format!(
                "bootstrap is version `{}` but the turn context asked for `{version}`",
                bootstrap.version
            ));
        }
        tracing::info!(agent, version, "loaded agent bootstrap");
        let bootstrap = Arc::new(bootstrap);
        self.cache()
            .insert(agent.to_owned(), Arc::clone(&bootstrap));
        Ok(bootstrap)
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

    async fn prepare_turn(&self, agent: &str, key: &str) -> Result<Prepared<NoTools>, String> {
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
        let bootstrap = self.bootstrap(agent, &client, &turn.version).await?;
        let source = ProviderPages {
            client,
            session: key.to_owned(),
        };
        Ok(Prepared {
            system: bootstrap.instructions.clone(),
            tools: Vec::new(),
            workspace: Workspace::with_source(turn.pages, Arc::new(source)),
            model: bootstrap.model.clone(),
            first_call_effort: bootstrap.first_call_effort,
            effort: bootstrap.effort,
            max_output_tokens: bootstrap.max_output_tokens,
            max_model_calls: bootstrap.max_model_calls,
            host: NoTools,
        })
    }
}

impl Backend for ContextProviderBackend {
    type Host = NoTools;

    async fn prepare(&self, agent: &str, key: &str) -> Result<Prepared<NoTools>, BackendError> {
        self.prepare_turn(agent, key)
            .await
            .map_err(|error| BackendError(format!("context provider for `{agent}`: {error}")))
    }
}

fn check_pages(pages: &[Page]) -> Result<(), String> {
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
