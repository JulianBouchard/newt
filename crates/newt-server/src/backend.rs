use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use newt_core::journal::JournalEntry;
use newt_core::prompt::{Effort, Lane};
use newt_core::skills::Skill;
use newt_core::tools::{BuiltinTools, NoTools, ToolDef, ToolHost};
use newt_core::turn::{Compaction, TurnSettings};
use newt_core::workspace::Workspace;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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

/// The session's provider state during a turn: the state the last committed
/// turn left, replaced by the `/turn` response's `state` and changed by
/// merge patches from page fetches and tool results. Saved when the turn
/// commits, if anything changed it.
#[derive(Debug, Clone, Default)]
pub struct SessionState(Arc<Mutex<StateSlot>>);

#[derive(Debug, Default)]
struct StateSlot {
    value: Option<Value>,
    changed: bool,
}

impl SessionState {
    pub fn starting_from(previous: Option<Value>) -> Self {
        Self(Arc::new(Mutex::new(StateSlot {
            value: previous,
            changed: false,
        })))
    }

    fn slot(&self) -> MutexGuard<'_, StateSlot> {
        match self.0.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn replace(&self, state: Value) {
        let mut slot = self.slot();
        slot.value = Some(state);
        slot.changed = true;
    }

    /// Applies a JSON merge patch (RFC 7396): objects merge key by key, a
    /// `null` removes a key, anything else replaces.
    pub fn patch(&self, patch: &Map<String, Value>) {
        let mut slot = self.slot();
        let before = slot.value.clone();
        let mut value = slot.value.take().unwrap_or(Value::Null);
        merge_object(&mut value, patch);
        slot.changed |= before.as_ref() != Some(&value);
        slot.value = Some(value);
    }

    pub fn get(&self) -> Option<Value> {
        self.slot().value.clone()
    }

    /// The state to save at commit: `None` when nothing in the turn changed it.
    pub fn changed(&self) -> Option<Value> {
        let slot = self.slot();
        if slot.changed {
            slot.value.clone()
        } else {
            None
        }
    }
}

fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(patch) => merge_object(target, patch),
        _ => *target = patch.clone(),
    }
}

fn merge_object(target: &mut Value, patch: &Map<String, Value>) {
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(target) = target {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                merge_patch(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    }
}

/// What the home server's `newt/turn` chose for a turn, journaled when it
/// differs from the bootstrap. Once a model call is journaled, a resumed turn
/// keeps it, or the bootstrap's settings when none was journaled, whatever the
/// server answers the second time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnChoice {
    pub model: String,
    pub first_call_effort: Option<Effort>,
    pub effort: Option<Effort>,
    pub reset: bool,
}

impl TurnChoice {
    /// The choice as the turn journal stores it.
    pub fn journal_entry(&self) -> Result<JournalEntry, serde_json::Error> {
        match serde_json::to_value(self)? {
            Value::Object(fields) => Ok(JournalEntry::Host(fields)),
            _ => Err(serde::ser::Error::custom("a turn choice is an object")),
        }
    }

    /// The last choice journaled among `entries`.
    pub fn last_journaled(entries: &[JournalEntry]) -> Result<Option<Self>, serde_json::Error> {
        entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                JournalEntry::Host(fields) => Some(fields),
                _ => None,
            })
            .map(|fields| serde_json::from_value(Value::Object(fields.clone())))
            .transpose()
    }
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
    pub settings: AgentSettings,
    pub state: SessionState,
    /// What `/turn` chose for this turn, resolved against the bootstrap.
    /// The model and efforts in `settings` stay the bootstrap's, so a resumed turn
    /// whose first attempt chose nothing can fall back to them. `None` when
    /// the provider chose nothing.
    pub choice: Option<TurnChoice>,
    /// Whether the provider wants a turn-end callback.
    pub turn_end: bool,
    pub turn_state: TurnState,
    pub host: H,
}

/// How an agent's turns run: its model, efforts and limits, carried from the
/// agent's declared settings to each turn's [`TurnSettings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSettings {
    pub model: String,
    pub first_call_effort: Option<Effort>,
    pub effort: Option<Effort>,
    pub max_output_tokens: u32,
    pub max_model_calls: u32,
    pub max_turn_tokens: Option<u64>,
    pub recite: bool,
    pub require_end_turn: bool,
    pub builtin_tools: BuiltinTools,
    pub page_index_wait: std::time::Duration,
    pub history_budget: u32,
}

impl AgentSettings {
    /// The settings of an ordinary turn of this session.
    pub fn turn(&self, lane: Lane, reset: bool) -> TurnSettings<'_> {
        TurnSettings {
            model: &self.model,
            first_call_effort: self.first_call_effort,
            effort: self.effort,
            max_output_tokens: self.max_output_tokens,
            max_model_calls: self.max_model_calls,
            max_turn_tokens: self.max_turn_tokens,
            recite: self.recite,
            require_end_turn: self.require_end_turn,
            builtin_tools: self.builtin_tools,
            page_index_wait: self.page_index_wait,
            lane,
            reset,
            compaction: None,
        }
    }

    /// The settings of a compaction turn: in the background, at the agent's
    /// effort throughout, with no token budget, recitation or `end_turn`.
    pub fn compaction<'a>(&'a self, compaction: Compaction<'a>) -> TurnSettings<'a> {
        TurnSettings {
            model: &self.model,
            first_call_effort: self.effort,
            effort: self.effort,
            max_output_tokens: self.max_output_tokens,
            max_model_calls: self.max_model_calls,
            max_turn_tokens: None,
            recite: false,
            require_end_turn: false,
            builtin_tools: self.builtin_tools,
            page_index_wait: self.page_index_wait,
            lane: Lane::Background,
            reset: false,
            compaction: Some(compaction),
        }
    }
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
        request: &TurnRequest,
    ) -> impl Future<Output = Result<Prepared<NoTools>, BackendError>> + Send {
        std::future::ready(Ok(Prepared {
            system: self.system.clone(),
            tools: Vec::new(),
            context: None,
            skills: Vec::new(),
            workspace: self.workspace.clone(),
            settings: AgentSettings {
                model: self.model.clone(),
                first_call_effort: Some(Effort::Low),
                effort: Some(Effort::Medium),
                max_output_tokens: 4096,
                max_model_calls: 8,
                max_turn_tokens: None,
                recite: false,
                require_end_turn: false,
                builtin_tools: BuiltinTools::ALL,
                page_index_wait: std::time::Duration::from_secs(2),
                history_budget: 60_000,
            },
            state: SessionState::starting_from(request.state.clone()),
            choice: None,
            turn_end: false,
            turn_state: TurnState::default(),
            host: NoTools,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    fn patched(target: Value, patch: &Value) -> Value {
        let mut target = target;
        merge_patch(&mut target, patch);
        target
    }

    #[test]
    fn a_merge_patch_merges_objects_removes_nulls_and_replaces_the_rest() {
        assert_eq!(
            patched(
                json!({ "a": "b", "c": { "d": "e", "f": "g" } }),
                &json!({ "a": "z", "c": { "f": null } })
            ),
            json!({ "a": "z", "c": { "d": "e" } })
        );
        assert_eq!(
            patched(json!({ "a": ["b"] }), &json!({ "a": "c" })),
            json!({ "a": "c" })
        );
        assert_eq!(patched(json!(["a"]), &json!({ "b": 1 })), json!({ "b": 1 }));
        assert_eq!(patched(json!({ "a": 1 }), &json!([2])), json!([2]));
    }

    #[test]
    fn session_state_is_saved_only_when_the_turn_changed_it() {
        let state = SessionState::starting_from(Some(json!({ "seen": 1 })));
        assert_eq!(state.changed(), None);
        state.patch(&object(json!({ "seen": 1 })));
        assert_eq!(
            state.changed(),
            None,
            "a patch that changes nothing is not a change"
        );
        state.patch(&object(json!({ "sent": true })));
        assert_eq!(state.changed(), Some(json!({ "seen": 1, "sent": true })));
        state.replace(json!({ "seen": 2 }));
        assert_eq!(state.get(), Some(json!({ "seen": 2 })));

        let fresh = SessionState::starting_from(None);
        fresh.patch(&object(json!({ "seen": 3 })));
        assert_eq!(fresh.changed(), Some(json!({ "seen": 3 })));
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]
    fn a_turn_choice_journals_as_it_did_before_it_left_newt_core() {
        let stored = json!({
            "entry": "choice",
            "model": "strong",
            "first_call_effort": null,
            "effort": "high",
            "reset": true,
        });
        let entry: JournalEntry = serde_json::from_value(stored.clone()).unwrap();
        let choice = TurnChoice::last_journaled(std::slice::from_ref(&entry))
            .unwrap()
            .unwrap();
        assert_eq!(
            choice,
            TurnChoice {
                model: "strong".to_owned(),
                first_call_effort: None,
                effort: Some(Effort::High),
                reset: true,
            }
        );
        let written = serde_json::to_value(choice.journal_entry().unwrap()).unwrap();
        assert_eq!(written, stored);
    }
}
