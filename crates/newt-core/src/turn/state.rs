use std::collections::{HashMap, HashSet};

use super::setup::initial_scratch;
use super::{CallReport, TurnError, TurnSettings};
use crate::journal::{JournalEntry, Replay};
use crate::message::{Message, Part};

/// From this model call on (counting from 0), and every [`RECITE_EVERY`]
/// calls after, the model is reminded what the turn is answering.
const RECITE_FROM: u32 = 4;
const RECITE_EVERY: u32 = 4;
const GOAL_EVENT_CHARS: usize = 500;
const OVER_BUDGET: &str = "This turn has spent its token budget. Finish now: send what you have if the person is waiting, then call end_turn.";

pub(super) struct TurnState {
    pub(super) effects: bool,
    pub(super) replay: Replay,
    pub(super) steered: Vec<String>,
    pub(super) scratch: Vec<Message>,
    pub(super) summaries: HashMap<String, String>,
    pub(super) kept: HashSet<String>,
    pub(super) harness_notes: HashSet<usize>,
    pub(super) calls: Vec<CallReport>,
    pub(super) traced: usize,
    /// The events this turn answers, restated to the model on long turns.
    pub(super) goal: String,
    pub(super) told_over_budget: bool,
}

impl TurnState {
    pub(super) fn new(
        context: Option<String>,
        events: Vec<Message>,
        replay: Vec<JournalEntry>,
    ) -> Self {
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

    pub(super) fn note(&mut self, text: impl Into<String>) {
        self.harness_notes.insert(self.scratch.len());
        self.scratch.push(Message::user_text(text));
    }

    /// What the harness tells the model before a call: that the turn spent
    /// its token budget, or on a long turn, what the turn is answering.
    pub(super) fn before_call(
        &mut self,
        settings: &TurnSettings<'_>,
        call: u32,
    ) -> Result<(), TurnError> {
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
