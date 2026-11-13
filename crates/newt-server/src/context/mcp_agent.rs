//! An agent defined by one MCP server and nothing else: its instructions are
//! the server's, its tools and skills are the server's, and its workspace
//! pages are the server's resources for the session.

use std::collections::BTreeMap;
use std::sync::Arc;

use newt_core::workspace::Workspace;

use super::{
    Bootstrap, ContextProviderBackend, Loaded, default_history_budget, default_max_model_calls,
    default_max_output_tokens,
};
use crate::backend::Prepared;
use crate::mcp::{McpAuth, McpHost, McpServer, Toolset};

const VERSION: &str = "mcp";

#[derive(Clone)]
pub struct McpAgent {
    pub url: String,
    pub model: String,
    pub token: String,
}

impl std::fmt::Debug for McpAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpAgent")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl ContextProviderBackend {
    async fn load_mcp(&self, agent: &str, mcp: &McpAgent) -> Result<Arc<Loaded>, String> {
        let mut slot = self.slot(agent)?.lock().await;
        if let Some(loaded) = slot.as_ref().filter(|loaded| !loaded.toolset.is_stale()) {
            return Ok(Arc::clone(loaded));
        }
        let server = McpServer {
            name: agent.to_owned(),
            url: mcp.url.clone(),
            headers: BTreeMap::new(),
            auth: McpAuth::AgentToken,
        };
        let toolset = Toolset::connect(std::slice::from_ref(&server), &mcp.token).await?;
        if toolset.instructions.is_empty() {
            return Err(
                "the MCP server sent no instructions; they are the agent's system prompt"
                    .to_owned(),
            );
        }
        let bootstrap = Bootstrap {
            version: VERSION.to_owned(),
            instructions: toolset.instructions.join("\n\n"),
            model: mcp.model.clone(),
            first_call_effort: None,
            effort: None,
            max_output_tokens: default_max_output_tokens(),
            max_model_calls: default_max_model_calls(),
            max_turn_tokens: None,
            history_budget: default_history_budget(),
            skills: Vec::new(),
            mcp_servers: vec![server],
        };
        self.check_bootstrap(&bootstrap)?;
        let skills = toolset.skills.clone();
        tracing::info!(
            agent,
            tools = toolset.tools.len(),
            skills = skills.len(),
            "loaded MCP agent"
        );
        let loaded = Arc::new(Loaded {
            bootstrap,
            toolset,
            skills,
        });
        *slot = Some(Arc::clone(&loaded));
        Ok(loaded)
    }

    pub(super) async fn prepare_mcp_turn(
        &self,
        agent: &str,
        key: &str,
        mcp: &McpAgent,
    ) -> Result<Prepared<McpHost>, String> {
        let loaded = self.load_mcp(agent, mcp).await?;
        let workspace = match loaded.toolset.pages(key) {
            Some(pages) => {
                let workspace = Workspace::loading(Arc::clone(&pages) as _);
                workspace.watch(|invalidator| pages.watch(invalidator));
                workspace
            }
            None => Workspace::default(),
        };
        let bootstrap = &loaded.bootstrap;
        Ok(Prepared {
            system: bootstrap.instructions.clone(),
            tools: loaded.toolset.tools.clone(),
            context: None,
            skills: loaded.skills.clone(),
            workspace,
            model: bootstrap.model.clone(),
            first_call_effort: bootstrap.first_call_effort,
            effort: bootstrap.effort,
            max_output_tokens: bootstrap.max_output_tokens,
            max_model_calls: bootstrap.max_model_calls,
            max_turn_tokens: bootstrap.max_turn_tokens,
            history_budget: bootstrap.history_budget,
            host: loaded.toolset.host(key),
        })
    }
}
