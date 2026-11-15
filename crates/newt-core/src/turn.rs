use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use std::sync::Mutex;

use crate::checkpoint::Checkpoint;
use crate::commit::{CommitRules, committed_messages};
use crate::event::{Event, EventKind};
use crate::journal::{Journal, JournalEntry, Replay};
use crate::message::{Message, Part, Role, ToolCall};
use crate::prompt::{Effort, Lane, LayerFingerprint, ModelRequest, Prompt, ToolSpec};
use crate::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use crate::sink::EventSink;
use crate::skills::{Skill, load_skill_tool, with_index};
use crate::steering::Steering;
use crate::tools::{EndTurn, Retention, ToolDef, ToolHost, builtin_tools};
use crate::workspace::{PageKey, Workspace};

mod call;
mod setup;

use call::{next_response, take_arrivals};
use setup::{ToolTable, initial_scratch, tool_table, turn_started};

/// From this model call on (counting from 0), and every [`RECITE_EVERY`]
/// calls after, the model is reminded what the turn is answering.
const RECITE_FROM: u32 = 4;
const RECITE_EVERY: u32 = 4;
const GOAL_EVENT_CHARS: usize = 500;
const OVER_BUDGET: &str = "This turn has spent its token budget. Finish now: send what you have if the person is waiting, then call end_turn.";
const END_TURN_REMINDER: &str =
    "Every turn ends with a call to end_turn. Call it now if you are done, or keep working.";
const CHECKPOINT_REMINDER: &str = "Compaction ends with a call to checkpoint. Call it now.";

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
    pub lane: Lane,
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

impl TurnContext<'_> {
    fn loaded(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        match self.loaded_skills.lock() {
            Ok(loaded) => loaded,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(crate) fn mark_skill_loaded(&self, call: &ToolCall) {
        if let Some(name) = crate::skills::requested(call)
            && crate::skills::requested_file(call).is_none()
        {
            self.loaded().insert(name.to_owned());
        }
    }

    pub(crate) fn skills_loaded(&self) -> Vec<String> {
        self.loaded().iter().cloned().collect()
    }
}

pub(crate) struct TurnIds<'a> {
    pub(crate) session: &'a str,
    pub(crate) cache_key: &'a str,
    pub(crate) turn: &'a str,
    pub(crate) started: time::OffsetDateTime,
}

pub(crate) struct TurnContext<'a> {
    pub(crate) ids: TurnIds<'a>,
    system: &'a str,
    history: &'a [Message],
    pub(crate) workspace: &'a Workspace,
    specs: &'a [ToolSpec],
    pub(crate) retention: &'a HashMap<String, Retention>,
    pub(crate) invalidates: &'a HashMap<String, Vec<PageKey>>,
    pub(crate) compaction: Option<Compaction<'a>>,
    pub(crate) skills: &'a [Skill],
    loaded_skills: Mutex<BTreeSet<String>>,
    prefix_fingerprint: LayerFingerprint,
    history_fingerprint: LayerFingerprint,
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

enum CallResult {
    Response(Box<ModelResponse>),
    Interrupted,
}

struct TurnState {
    effects: bool,
    replay: Replay,
    steered: Vec<String>,
    scratch: Vec<Message>,
    summaries: HashMap<String, String>,
    kept: HashSet<String>,
    harness_notes: HashSet<usize>,
    calls: Vec<CallReport>,
    traced: usize,
    /// The events this turn answers, restated to the model on long turns.
    goal: String,
    told_over_budget: bool,
}

impl TurnState {
    fn new(context: Option<String>, events: Vec<Message>, replay: Vec<JournalEntry>) -> Self {
        let goal = goal_of(&events);
        let (scratch, harness_notes) = initial_scratch(context, events);
        Self {
            effects: false,
            replay: Replay::new(replay),
            steered: Vec::new(),
            scratch,
            summaries: HashMap::new(),
            kept: HashSet::new(),
            harness_notes,
            calls: Vec::new(),
            traced: 0,
            goal,
            told_over_budget: false,
        }
    }

    fn note(&mut self, text: impl Into<String>) {
        self.harness_notes.insert(self.scratch.len());
        self.scratch.push(Message::user_text(text));
    }

    /// What the harness tells the model before a call: that the turn spent
    /// its token budget, or on a long turn, what the turn is answering.
    fn before_call(&mut self, settings: &TurnSettings<'_>, call: u32) -> Result<(), TurnError> {
        if let Some(budget) = settings.max_turn_tokens {
            let spent: u64 = self
                .calls
                .iter()
                .map(|report| report.usage.input_tokens + report.usage.output_tokens)
                .sum();
            if spent >= budget {
                if self.told_over_budget {
                    return Err(TurnError::OverBudget(budget));
                }
                self.told_over_budget = true;
                self.note(OVER_BUDGET);
                return Ok(());
            }
        }
        if settings.recite
            && call >= RECITE_FROM
            && (call - RECITE_FROM).is_multiple_of(RECITE_EVERY)
            && !self.goal.is_empty()
        {
            let goal = format!("<goal>\nThis turn is answering:\n{}\n</goal>", self.goal);
            self.note(goal);
        }
        Ok(())
    }
}

/// The text of the turn's events, each cut to [`GOAL_EVENT_CHARS`].
fn goal_of(events: &[Message]) -> String {
    events
        .iter()
        .flat_map(|event| &event.parts)
        .filter_map(|part| match part {
            Part::Text { text } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .map(|text| {
            let mut line: String = text.chars().take(GOAL_EVENT_CHARS).collect();
            if line.len() < text.len() {
                line.push('…');
            }
            format!("- {}", line.replace('\n', " "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[expect(
    clippy::result_large_err,
    reason = "a failed turn is rare and must hand back what it committed"
)]
pub async fn run_turn<M: Model, H: ToolHost, S: Steering, J: Journal>(
    deps: &TurnDeps<'_, M, H, S, J>,
    input: TurnInput<'_>,
    settings: &TurnSettings<'_>,
) -> Result<TurnOutcome, TurnFailure> {
    let sink = deps.sink;
    let ToolTable {
        specs,
        retention,
        invalidates,
    } = match tool_table(input.external_tools, input.skills) {
        Ok(table) => table,
        Err(error) => {
            return Err(TurnFailure {
                error,
                committed: input.events,
                calls: Vec::new(),
                steered: Vec::new(),
                effects: false,
            });
        }
    };
    let system = with_index(input.system, input.skills);
    let layers = Prompt {
        system: &system,
        tools: &specs,
        history: input.history,
        scratch: &[],
    };
    let started = turn_started(&input, settings);
    let context = TurnContext {
        ids: TurnIds {
            session: input.session,
            cache_key: input.cache_key,
            turn: input.turn,
            started: started.at,
        },
        system: &system,
        history: input.history,
        workspace: input.workspace,
        specs: &specs,
        retention: &retention,
        invalidates: &invalidates,
        compaction: settings.compaction,
        skills: input.skills,
        loaded_skills: Mutex::new(BTreeSet::new()),
        prefix_fingerprint: layers.prefix_fingerprint(),
        history_fingerprint: layers.history_fingerprint(),
    };
    sink.emit(started);
    let mut state = TurnState::new(input.context, input.events, input.replay);

    let result = drive(deps, &context, settings, &mut state).await;
    let committed = committed_messages(
        state.scratch,
        &CommitRules {
            retention: &retention,
            summaries: &state.summaries,
            kept: &state.kept,
            harness_notes: &state.harness_notes,
        },
    );
    let ids = &context.ids;
    match result {
        Ok(Finish {
            end,
            reply,
            checkpoint,
        }) => {
            sink.emit(Event::now(EventKind::TurnCompleted {
                session: ids.session.to_owned(),
                turn: ids.turn.to_owned(),
                model_calls: u32::try_from(state.calls.len()).unwrap_or(u32::MAX),
                committed_messages: committed.len(),
            }));
            Ok(TurnOutcome {
                committed,
                end,
                reply,
                checkpoint,
                calls: state.calls,
                steered: state.steered,
            })
        }
        Err(error) => {
            sink.emit(Event::now(EventKind::TurnFailed {
                session: ids.session.to_owned(),
                turn: ids.turn.to_owned(),
                error: error.to_string(),
                committed_messages: committed.len(),
            }));
            Err(TurnFailure {
                error,
                committed,
                calls: state.calls,
                steered: state.steered,
                effects: state.effects,
            })
        }
    }
}

struct Finish {
    end: EndTurn,
    reply: Option<String>,
    checkpoint: Option<Checkpoint>,
}

fn remind_to_end(state: &mut TurnState, context: &TurnContext<'_>) {
    state.harness_notes.insert(state.scratch.len());
    state
        .scratch
        .push(Message::user_text(if context.compaction.is_some() {
            CHECKPOINT_REMINDER
        } else {
            END_TURN_REMINDER
        }));
}

fn text_of(message: &Message) -> Option<String> {
    let text = message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

async fn drive<M: Model, H: ToolHost, S: Steering, J: Journal>(
    deps: &TurnDeps<'_, M, H, S, J>,
    context: &TurnContext<'_>,
    settings: &TurnSettings<'_>,
    state: &mut TurnState,
) -> Result<Finish, TurnError> {
    let mut reminded = false;
    let mut unended_text: Option<String> = None;
    for call in 0..settings.max_model_calls {
        state.before_call(settings, call)?;
        let arrived = take_arrivals(deps, state).await;
        if !arrived.is_empty() {
            deps.sink.emit(Event::now(EventKind::TurnSteered {
                session: context.ids.session.to_owned(),
                turn: context.ids.turn.to_owned(),
                events: arrived.len(),
            }));
            for steered in arrived {
                state.scratch.push(steered.message);
                state.steered.push(steered.id);
            }
        }
        let request = ModelRequest {
            model: settings.model,
            prompt: Prompt {
                system: context.system,
                tools: context.specs,
                history: context.history,
                scratch: &state.scratch,
            },
            effort: if call == 0 {
                settings.first_call_effort
            } else {
                settings.effort
            },
            max_output_tokens: settings.max_output_tokens,
            cache_key: context.ids.cache_key,
            lane: settings.lane,
        };
        let next = next_response(
            deps,
            &request,
            call,
            context,
            &mut state.replay,
            state.traced,
        )
        .await?;
        state.traced = state.scratch.len();
        let Some(response) = next else {
            continue;
        };
        state.calls.push(CallReport {
            usage: response.usage,
            timing: response.timing,
            stop: response.stop,
        });

        let tool_calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
        let text = text_of(&response.message);
        if !response.message.parts.is_empty() {
            state.scratch.push(response.message);
        }
        if tool_calls.is_empty() {
            if reminded {
                return Err(TurnError::NoEndTurn);
            }
            reminded = true;
            unended_text = text;
            remind_to_end(state, context);
            continue;
        }

        let batch = crate::batch::run_tools(&tool_calls, context, deps, &state.replay).await;
        state.summaries.extend(batch.summaries);
        state.kept.extend(batch.kept);
        state.effects |= batch.effects;
        state.scratch.push(Message {
            role: Role::User,
            parts: batch.results.into_iter().map(Part::ToolResult).collect(),
        });
        if let Some(checkpoint) = batch.checkpoint {
            return Ok(Finish {
                end: EndTurn {
                    recorded: Vec::new(),
                    nothing_new: Some("checkpoint written".to_owned()),
                },
                reply: None,
                checkpoint: Some(checkpoint),
            });
        }
        if let Some(end) = batch.end {
            return Ok(Finish {
                end,
                reply: text.or(unended_text),
                checkpoint: None,
            });
        }
        unended_text = None;
    }
    Err(TurnError::TooManyModelCalls(settings.max_model_calls))
}
