//! An agent is its home MCP server and the servers it adds. The home's
//! instructions are the system prompt, its `newt` extension holds the agent's
//! settings, its resources are the root of the workspace, and it can answer
//! `newt/turn` and `newt/turn_end` when it declares them.

mod servers;
mod turn_context;

pub use servers::{AddedServer, HomeServer, Settings};

use servers::resolve;
use turn_context::{Loaded, TurnCall, TurnContext};

use crate::backend::{Backend, BackendError, Prepared, SessionState, TurnRequest, TurnState};
use crate::mcp::{HomeConnection, McpAuth, McpHost, McpServer, NEWT_EXTENSION, STDIO_SCHEME};
use newt_core::workspace::Workspace;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

pub struct HomeBackend {
    homes: HashMap<String, Arc<HomeServer>>,
    models: HashSet<String>,
    loaded: HashMap<String, tokio::sync::Mutex<Option<Arc<Loaded>>>>,
}

impl std::fmt::Debug for HomeBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HomeBackend")
            .field("agents", &self.homes.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl HomeBackend {
    pub fn new(
        homes: BTreeMap<String, HomeServer>,
        models: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            loaded: homes
                .keys()
                .map(|name| (name.clone(), tokio::sync::Mutex::new(None)))
                .collect(),
            homes: homes
                .into_iter()
                .map(|(name, home)| (name, Arc::new(home)))
                .collect(),
            models: models.into_iter().collect(),
        }
    }

    /// The agent's connected servers and settings, reconnecting when the
    /// toolset went stale.
    async fn load(&self, agent: &str) -> Result<Arc<Loaded>, String> {
        let home = self
            .homes
            .get(agent)
            .ok_or_else(|| format!("agent `{agent}` is not in server.toml"))?;
        let mut slot = self
            .loaded
            .get(agent)
            .ok_or_else(|| format!("agent `{agent}` is not in server.toml"))?
            .lock()
            .await;
        if let Some(loaded) = slot.as_ref().filter(|loaded| !loaded.toolset.is_stale()) {
            return Ok(Arc::clone(loaded));
        }
        let previous = slot.as_ref().map(|loaded| &loaded.toolset);
        let (settings, toolset) = match slot.as_ref() {
            Some(loaded) if !loaded.toolset.needs_new_home() => {
                let added = resolve(&loaded.settings.servers)?;
                let toolset = loaded.toolset.refresh(None, &added).await?;
                (loaded.settings.clone(), toolset)
            }
            _ => {
                let server = McpServer {
                    name: agent.to_owned(),
                    url: home.url.clone(),
                    auth: if home.url.starts_with(STDIO_SCHEME) {
                        McpAuth::None
                    } else {
                        McpAuth::Bearer(home.token.clone())
                    },
                };
                let connected = HomeConnection::connect(&server).await?;
                let settings = self.declared_settings(&connected)?;
                let added = resolve(&settings.servers)?;
                let toolset = match previous {
                    Some(previous) => previous.refresh(Some(connected), &added).await?,
                    None => connected.with_servers(&added).await?,
                };
                (settings, toolset)
            }
        };
        tracing::info!(
            agent,
            tools = toolset.tools.len(),
            skills = toolset.skills.len(),
            servers = settings.servers.len(),
            unavailable = toolset.unavailable.len(),
            "loaded agent"
        );
        let loaded = Arc::new(Loaded { settings, toolset });
        *slot = Some(Arc::clone(&loaded));
        Ok(loaded)
    }

    /// The agent's settings, as the home declared them when newt connected.
    fn declared_settings(&self, connected: &HomeConnection) -> Result<Settings, String> {
        if connected.instructions.is_empty() {
            return Err(
                "the home server sent no instructions; they are the agent's system prompt"
                    .to_owned(),
            );
        }
        let extension = connected.extension.clone().ok_or_else(|| {
            format!(
                "the home server does not declare the `{NEWT_EXTENSION}` extension, which holds the agent's model and settings"
            )
        })?;
        let settings: Settings = serde_json::from_value(Value::Object(extension))
            .map_err(|error| format!("the home's `{NEWT_EXTENSION}` extension: {error}"))?;
        self.check_settings(&settings)?;
        Ok(settings)
    }

    fn check_settings(&self, settings: &Settings) -> Result<(), String> {
        self.check_model(&settings.model, "the home")?;
        if settings.history_budget < 1_000 {
            return Err("history_budget must be at least 1000 tokens".to_owned());
        }
        if settings.max_output_tokens == 0 || settings.max_model_calls == 0 {
            return Err("max_output_tokens and max_model_calls must be at least 1".to_owned());
        }
        Ok(())
    }

    fn check_model(&self, model: &str, source: &str) -> Result<(), String> {
        if self.models.contains(model) {
            return Ok(());
        }
        Err(format!(
            "{source} names model `{model}`, which server.toml does not define"
        ))
    }

    async fn prepare_turn(
        &self,
        agent: &str,
        key: &str,
        request: &TurnRequest,
    ) -> Result<Prepared<McpHost>, String> {
        let loaded = self.load(agent).await?;
        let settings = &loaded.settings;
        let turn = if settings.turn {
            let params = serde_json::to_value(TurnCall {
                session: key,
                events: &request.events,
                state: request.state.as_ref(),
            })
            .map_err(|error| error.to_string())?;
            let answer = loaded.toolset.home().request("newt/turn", params).await?;
            serde_json::from_value::<TurnContext>(answer)
                .map_err(|error| format!("newt/turn: invalid result: {error}"))?
        } else {
            TurnContext::default()
        };
        if let Some(model) = &turn.model {
            self.check_model(model, "newt/turn")?;
        }
        let turn_state = TurnState::default();
        if let Some(initial) = turn.turn_state.clone() {
            turn_state.set(initial);
        }
        let state = SessionState::starting_from(request.state.clone());
        if let Some(replaced) = turn.state.clone() {
            state.replace(replaced);
        }
        let workspace = match loaded.toolset.pages(key, &state) {
            Some(pages) => {
                let workspace = Workspace::loading(Arc::clone(&pages) as _);
                workspace.watch(|invalidator| pages.watch(invalidator));
                workspace
            }
            None => Workspace::default(),
        };
        Ok(Prepared {
            system: with_pinned(&loaded.toolset.home().instructions, turn.pinned.as_deref()),
            tools: loaded.toolset.tools.clone(),
            context: turn.render(&loaded.toolset.unavailable),
            skills: loaded.toolset.skills.clone(),
            workspace,
            model: settings.model.clone(),
            first_call_effort: settings.first_call_effort,
            effort: settings.effort,
            max_output_tokens: settings.max_output_tokens,
            max_model_calls: settings.max_model_calls,
            max_turn_tokens: settings.max_turn_tokens,
            recite: settings.recite,
            require_end_turn: settings.require_end_turn,
            history_budget: settings.history_budget,
            choice: turn.choice(settings),
            turn_end: settings.turn_end,
            host: loaded.toolset.host(key, turn_state.clone(), state.clone()),
            state,
            turn_state,
        })
    }
}

impl Backend for HomeBackend {
    type Host = McpHost;

    async fn prepare(
        &self,
        agent: &str,
        key: &str,
        request: &TurnRequest,
    ) -> Result<Prepared<McpHost>, BackendError> {
        self.prepare_turn(agent, key, request)
            .await
            .map_err(|error| BackendError(format!("agent `{agent}`: {error}")))
    }

    async fn turn_end(&self, agent: &str, payload: &Value) -> Result<(), BackendError> {
        let loaded = self
            .load(agent)
            .await
            .map_err(|error| BackendError(format!("turn end for `{agent}`: {error}")))?;
        loaded
            .toolset
            .home()
            .request("newt/turn_end", payload.clone())
            .await
            .map(|_| ())
            .map_err(|error| BackendError(format!("turn end for `{agent}`: {error}")))
    }
}

fn with_pinned(instructions: &str, pinned: Option<&str>) -> String {
    match pinned.map(str::trim).filter(|pinned| !pinned.is_empty()) {
        Some(pinned) => format!("{instructions}\n\n<pinned>\n{pinned}\n</pinned>"),
        None => instructions.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;
    use serde_json::json;

    #[test]
    fn settings_take_defaults_and_refuse_unknown_fields() {
        let settings: Settings = serde_json::from_value(json!({ "model": "default" })).unwrap();
        assert_eq!(settings.max_model_calls, 8);
        assert_eq!(settings.history_budget, 60_000);
        assert!(!settings.turn && !settings.turn_end && settings.servers.is_empty());
        assert!(
            serde_json::from_value::<Settings>(json!({ "model": "default", "version": "1" }))
                .is_err()
        );
    }

    #[test]
    fn an_unavailable_server_is_named_in_the_turn_context() {
        let rendered = TurnContext::default()
            .render(&["paraform".to_owned()])
            .unwrap();
        assert!(rendered.contains("<unavailable_servers>\nparaform: unavailable"));
        assert_eq!(TurnContext::default().render(&[]), None);
    }
}
