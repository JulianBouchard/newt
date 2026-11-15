use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use newt_core::prompt::Effort;
use newt_core::skills::Skill;
use newt_core::tools::{NoTools, ToolDef, ToolHost};
use newt_core::workspace::Workspace;
use serde::Serialize;
use serde_json::Value;

/// What the runner tells the backend about the turn it prepares: the events
/// the turn answers and the session state the provider returned last.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnRequest {
    pub events: Vec<TurnEvent>,
    pub state: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnEvent {
    pub id: String,
    pub meta: Option<Value>,
}

/// The turn's tool state: sent with every tool call, replaced by a result
/// that carries a new one. Shared by the turn's tool host and the runner,
/// which hands the final value to the turn-end callback.
#[derive(Debug, Clone, Default)]
pub struct TurnState(Arc<Mutex<Value>>);

impl TurnState {
    fn value(&self) -> MutexGuard<'_, Value> {
        match self.0.lock() {
            Ok(value) => value,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn get(&self) -> Value {
        self.value().clone()
    }

    pub fn set(&self, state: Value) {
        *self.value() = state;
    }
}

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
    pub max_turn_tokens: Option<u64>,
    pub recite: bool,
    pub history_budget: u32,
    /// The session state the provider returned for this turn, saved when
    /// the turn commits. `None` keeps the previous state.
    pub state: Option<Value>,
    /// Whether the provider wants a turn-end callback.
    pub turn_end: bool,
    pub turn_state: TurnState,
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
        request: &TurnRequest,
    ) -> impl Future<Output = Result<Prepared<Self::Host>, BackendError>> + Send;

    /// Delivers a committed turn's turn-end callback. An error leaves it to
    /// be retried.
    fn turn_end(
        &self,
        _agent: &str,
        _payload: &Value,
    ) -> impl Future<Output = Result<(), BackendError>> + Send {
        std::future::ready(Ok(()))
    }
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
        _request: &TurnRequest,
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
            max_turn_tokens: None,
            recite: false,
            history_budget: 60_000,
            state: None,
            turn_end: false,
            turn_state: TurnState::default(),
            host: NoTools,
        }))
    }
}
