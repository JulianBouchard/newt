use std::future::Future;

use newt_core::prompt::Effort;
use newt_core::skills::Skill;
use newt_core::tools::{NoTools, ToolDef, ToolHost};
use newt_core::workspace::Workspace;

#[derive(Debug)]
pub struct Prepared<H> {
    pub system: String,
    pub tools: Vec<ToolDef>,
    pub context: Option<String>,
    pub skills: Vec<Skill>,
    pub workspace: Workspace,
    pub model: String,
    pub first_call_effort: Option<Effort>,
    pub effort: Option<Effort>,
    pub max_output_tokens: u32,
    pub max_model_calls: u32,
    pub history_budget: u32,
    pub host: H,
}

#[derive(Debug, thiserror::Error)]
#[error("could not prepare the turn: {0}")]
pub struct BackendError(pub String);

pub trait Backend: Send + Sync + 'static {
    type Host: ToolHost + 'static;

    fn prepare(
        &self,
        agent: &str,
        key: &str,
    ) -> impl Future<Output = Result<Prepared<Self::Host>, BackendError>> + Send;
}

#[derive(Debug, Clone)]
pub struct StaticBackend {
    pub system: String,
    pub model: String,
    pub workspace: Workspace,
}

impl Backend for StaticBackend {
    type Host = NoTools;

    fn prepare(
        &self,
        _agent: &str,
        _key: &str,
    ) -> impl Future<Output = Result<Prepared<NoTools>, BackendError>> + Send {
        std::future::ready(Ok(Prepared {
            system: self.system.clone(),
            tools: Vec::new(),
            context: None,
            skills: Vec::new(),
            workspace: self.workspace.clone(),
            model: self.model.clone(),
            first_call_effort: Some(Effort::Low),
            effort: Some(Effort::Medium),
            max_output_tokens: 4096,
            max_model_calls: 8,
            history_budget: 60_000,
            host: NoTools,
        }))
    }
}
