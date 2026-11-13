use std::collections::{BTreeMap, HashMap};

use time::OffsetDateTime;

use crate::event::{Event, EventKind};
use crate::message::Message;
use crate::prompt::Effort;
use crate::provider::{CallTiming, Usage};
use crate::tools::Retention;

#[derive(Debug, Default)]
pub(crate) struct CallTrace {
    pub(super) model: String,
    pub(super) effort: Option<Effort>,
    pub(super) prefix_hash: String,
    pub(super) started: Option<OffsetDateTime>,
    pub(super) finished: Option<OffsetDateTime>,
    pub(super) input: Vec<Message>,
    pub(super) output: Option<Message>,
    pub(super) usage: Usage,
    pub(super) timing: CallTiming,
    pub(super) response_id: Option<String>,
    pub(super) error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ToolTrace {
    pub(super) call_id: String,
    pub(super) name: String,
    pub(super) finished: OffsetDateTime,
    pub(super) ms: u64,
    pub(super) retention: Retention,
    pub(super) is_error: bool,
}

#[derive(Debug, Default)]
pub(crate) struct TurnTrace {
    pub(super) session: String,
    pub(super) turn: String,
    pub(super) compaction: bool,
    pub(super) started: Option<OffsetDateTime>,
    pub(super) ended: Option<OffsetDateTime>,
    pub(super) error: Option<String>,
    pub(super) calls: BTreeMap<u32, CallTrace>,
    pub(super) tools: Vec<ToolTrace>,
    pub(super) last_seen: Option<OffsetDateTime>,
}

#[derive(Debug, Default)]
pub(crate) struct Assembler {
    pub(super) turns: HashMap<(String, String), TurnTrace>,
    pub(super) finished: std::collections::VecDeque<(String, String)>,
}

const REMEMBERED_FINISHED: usize = 4096;

impl Assembler {
    fn turn(&mut self, session: &str, turn: &str) -> &mut TurnTrace {
        self.turns
            .entry((session.to_owned(), turn.to_owned()))
            .or_insert_with(|| TurnTrace {
                session: session.to_owned(),
                turn: turn.to_owned(),
                ..TurnTrace::default()
            })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one flat match over every event kind reads better than splitting it"
    )]
    pub(crate) fn apply(&mut self, event: Event) -> Option<TurnTrace> {
        let at = event.at;
        if let Some(key) = event_turn(&event.kind)
            && self.finished.contains(&key)
        {
            return None;
        }
        if let Some((session, turn)) = event_turn(&event.kind) {
            self.turn(&session, &turn).last_seen = Some(at);
        }
        match event.kind {
            EventKind::TurnStarted {
                session,
                turn,
                compaction,
                ..
            } => {
                let trace = self.turn(&session, &turn);
                trace.started = Some(at);
                trace.compaction = compaction;
            }
            EventKind::ModelCallStarted {
                session,
                turn,
                call,
                model,
                effort,
                prefix,
                input,
                ..
            } => {
                let entry = self.turn(&session, &turn).calls.entry(call).or_default();
                entry.model = model;
                entry.effort = effort;
                entry.prefix_hash = prefix.hash;
                entry.started = Some(at);
                entry.input = input;
            }
            EventKind::ModelCallFinished {
                session,
                turn,
                call,
                usage,
                timing,
                response_id,
                output,
                ..
            } => {
                let entry = self.turn(&session, &turn).calls.entry(call).or_default();
                entry.finished = Some(at);
                entry.usage = usage;
                entry.timing = timing;
                entry.response_id = response_id;
                entry.output = Some(output);
            }
            EventKind::ModelCallInterrupted {
                session,
                turn,
                call,
            } => {
                let entry = self.turn(&session, &turn).calls.entry(call).or_default();
                entry.finished = Some(at);
                entry.error = Some("interrupted by a new event".to_owned());
            }
            EventKind::ModelCallFailed {
                session,
                turn,
                call,
                error,
            } => {
                let entry = self.turn(&session, &turn).calls.entry(call).or_default();
                entry.finished = Some(at);
                entry.error = Some(error);
            }
            EventKind::ToolFinished {
                session,
                turn,
                call_id,
                name,
                retention,
                is_error,
                ms,
            } => {
                self.turn(&session, &turn).tools.push(ToolTrace {
                    call_id,
                    name,
                    finished: at,
                    ms,
                    retention,
                    is_error,
                });
            }
            EventKind::TurnCompleted { session, turn, .. } => {
                let mut trace = self.close(session, turn)?;
                trace.ended = Some(at);
                return Some(trace);
            }
            EventKind::TurnFailed {
                session,
                turn,
                error,
                ..
            } => {
                let mut trace = self.close(session, turn)?;
                trace.ended = Some(at);
                trace.error = Some(error);
                return Some(trace);
            }
            EventKind::ServerReady { .. }
            | EventKind::TurnSteered { .. }
            | EventKind::ToolStarted { .. }
            | EventKind::ToolProgress { .. }
            | EventKind::ModelCallReplayed { .. }
            | EventKind::CompactionCompleted { .. }
            | EventKind::EventAccepted { .. }
            | EventKind::SessionWoken { .. }
            | EventKind::SessionClaimed { .. }
            | EventKind::TurnPrepared { .. }
            | EventKind::TurnCommitted { .. }
            | EventKind::RunnerSample { .. } => {}
        }
        None
    }

    fn close(&mut self, session: String, turn: String) -> Option<TurnTrace> {
        let key = (session, turn);
        let trace = self.turns.remove(&key)?;
        if self.finished.len() == REMEMBERED_FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(key);
        Some(trace)
    }

    pub(crate) fn evict_older_than(&mut self, cutoff: OffsetDateTime) -> Vec<TurnTrace> {
        let stale: Vec<(String, String)> = self
            .turns
            .iter()
            .filter(|(_, trace)| trace.last_seen.is_none_or(|seen| seen < cutoff))
            .map(|(key, _)| key.clone())
            .collect();
        stale
            .into_iter()
            .filter_map(|(session, turn)| self.close(session, turn))
            .map(|mut trace| {
                trace.error = Some("the turn never finished".to_owned());
                trace
            })
            .collect()
    }

    pub(crate) fn drain(&mut self) -> Vec<TurnTrace> {
        self.turns
            .drain()
            .map(|(_, mut trace)| {
                trace.error = Some("the turn never finished".to_owned());
                trace
            })
            .collect()
    }
}

pub(super) fn event_turn(kind: &EventKind) -> Option<(String, String)> {
    match kind {
        EventKind::TurnStarted { session, turn, .. }
        | EventKind::ModelCallStarted { session, turn, .. }
        | EventKind::ModelCallFinished { session, turn, .. }
        | EventKind::ModelCallInterrupted { session, turn, .. }
        | EventKind::ModelCallFailed { session, turn, .. }
        | EventKind::ToolStarted { session, turn, .. }
        | EventKind::ToolFinished { session, turn, .. }
        | EventKind::TurnCompleted { session, turn, .. }
        | EventKind::TurnFailed { session, turn, .. } => Some((session.clone(), turn.clone())),
        EventKind::ServerReady { .. }
        | EventKind::ToolProgress { .. }
        | EventKind::TurnSteered { .. }
        | EventKind::ModelCallReplayed { .. }
        | EventKind::CompactionCompleted { .. }
        | EventKind::EventAccepted { .. }
        | EventKind::SessionWoken { .. }
        | EventKind::SessionClaimed { .. }
        | EventKind::TurnPrepared { .. }
        | EventKind::TurnCommitted { .. }
        | EventKind::RunnerSample { .. } => None,
    }
}
