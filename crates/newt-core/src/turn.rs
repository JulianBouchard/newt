use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use crate::commit::{CommitRules, committed_messages};
use crate::event::{Event, EventKind, FailureReason};
use crate::journal::Journal;
use crate::message::{Message, ToolCall};
use crate::prompt::{LayerFingerprint, Prompt, ToolSpec};
use crate::provider::{Model, ProviderError};
use crate::skills::{Skill, with_index};
use crate::steering::Steering;
use crate::tools::{Retention, ToolHost};
use crate::workspace::{PageKey, Workspace};

mod call;
mod coalesce;
mod drive;
mod setup;
mod state;
mod types;

use drive::{Finish, drive};
use setup::{ToolTable, offered_skills, tool_table, turn_started};
use state::TurnState;
pub use types::{
    CallReport, Compaction, TurnDeps, TurnError, TurnFailure, TurnInput, TurnOutcome, TurnSettings,
};

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
    pub(crate) ends_turn: &'a HashSet<String>,
    pub(crate) require_end_turn: bool,
    pub(crate) compaction: Option<Compaction<'a>>,
    pub(crate) skills: &'a [Skill],
    loaded_skills: Mutex<BTreeSet<String>>,
    prefix_fingerprint: LayerFingerprint,
    history_fingerprint: LayerFingerprint,
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
    let skills = offered_skills(input.skills, settings.builtin_tools);
    let ToolTable {
        specs,
        retention,
        invalidates,
        ends_turn,
    } = match tool_table(
        input.external_tools,
        skills,
        settings.require_end_turn,
        settings.builtin_tools,
        settings.compaction.is_some(),
    ) {
        Ok(table) => table,
        Err(error) => return Err(TurnFailure::before_start(error, input.events)),
    };
    let system = with_index(input.system, skills);
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
        ends_turn: &ends_turn,
        require_end_turn: settings.require_end_turn,
        compaction: settings.compaction,
        skills,
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
                reason: matches!(error, TurnError::Provider(ProviderError::Cancelled))
                    .then_some(FailureReason::Cancelled),
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
