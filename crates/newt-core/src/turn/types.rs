use tokio_util::sync::CancellationToken;

use crate::checkpoint::Checkpoint;
use crate::journal::JournalEntry;
use crate::message::Message;
use crate::prompt::{Effort, Lane};
use crate::provider::{CallTiming, ProviderError, StopReason, Usage};
use crate::sink::EventSink;
use crate::skills::Skill;
use crate::tools::{EndTurn, ToolDef};
use crate::workspace::Workspace;

#[derive(Debug)]
pub struct TurnInput<'a> {
    pub session: &'a str,
    pub cache_key: &'a str,
    pub turn: &'a str,
    pub system: &'a str,
    pub external_tools: &'a [ToolDef],
    pub history: &'a [Message],
    pub context: Option<String>,
    pub skills: &'a [Skill],
    pub events: Vec<Message>,
    pub workspace: &'a Workspace,
    pub replay: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Copy)]
pub struct Compaction<'a> {
    pub open_commitments: &'a [String],
    pub max_checkpoint_chars: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct TurnSettings<'a> {
    pub model: &'a str,
    pub first_call_effort: Option<Effort>,
    pub effort: Option<Effort>,
    pub max_output_tokens: u32,
    pub max_model_calls: u32,
    /// Input and output tokens one turn may spend across its model calls.
    /// Past it the model is told to end the turn, and one more call that
    /// does not end it fails the turn.
    pub max_turn_tokens: Option<u64>,
    /// Restate the turn's events to the model on long turns (recitation).
    /// Internal while its effect is measured; the server always sets it.
    pub recite: bool,
    /// Every turn must end with `end_turn`, which must then say what was recorded:
    /// a reply with no tool calls gets a reminder instead of ending the turn.
    /// Off by default, when a reply with no tool calls ends the turn.
    pub require_end_turn: bool,
    /// Which of newt's built-in tools the agent is offered. A compaction turn
    /// is always offered `checkpoint`.
    pub builtin_tools: crate::tools::BuiltinTools,
    pub lane: Lane,
    pub reset: bool,
    pub compaction: Option<Compaction<'a>>,
}

#[derive(Debug, Clone)]
pub struct CallReport {
    pub usage: Usage,
    pub timing: CallTiming,
    pub stop: StopReason,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub committed: Vec<Message>,
    pub end: EndTurn,
    /// What the agent said when it finished: the text of the message that
    /// called `end_turn`, or of the text-only message just before it that
    /// made newt remind it to end.
    pub reply: Option<String>,
    pub checkpoint: Option<Checkpoint>,
    pub calls: Vec<CallReport>,
    pub steered: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("the model stopped twice without calling end_turn")]
    NoEndTurn,
    #[error("the turn used all {0} model calls without ending")]
    TooManyModelCalls(u32),
    #[error("the turn spent its budget of {0} tokens and did not end when told to")]
    OverBudget(u64),
    #[error("two tools share the name `{0}`")]
    DuplicateTool(String),
}

#[derive(Debug, thiserror::Error)]
#[error("turn failed: {error}")]
pub struct TurnFailure {
    #[source]
    pub error: TurnError,
    pub committed: Vec<Message>,
    pub calls: Vec<CallReport>,
    pub steered: Vec<String>,
    pub effects: bool,
}

#[derive(Clone, Copy)]
pub struct TurnDeps<'a, M, H, S, J> {
    pub model: &'a M,
    pub host: &'a H,
    pub steering: &'a S,
    pub journal: &'a J,
    pub sink: &'a dyn EventSink,
    pub cancel: &'a CancellationToken,
}

impl<M, H, S, J> std::fmt::Debug for TurnDeps<'_, M, H, S, J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("TurnDeps").finish_non_exhaustive()
    }
}

impl TurnFailure {
    /// A turn that failed before its first model call: nothing ran, so its events are
    /// handed back to be committed or retried.
    pub(super) fn before_start(error: TurnError, committed: Vec<Message>) -> Self {
        Self {
            error,
            committed,
            calls: Vec::new(),
            steered: Vec::new(),
            effects: false,
        }
    }
}
