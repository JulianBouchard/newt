use std::collections::{HashMap, HashSet};

use super::{TurnError, TurnInput, TurnSettings};
use crate::checkpoint::CHECKPOINT;
use crate::event::{Event, EventKind};
use crate::message::Message;
use crate::prompt::ToolSpec;
use crate::skills::{LOAD_SKILL, Skill, load_skill_tool};
use crate::tools::{BuiltinTools, ENDS_TURN_NOTE, Retention, ToolDef, builtin_tools};
use crate::workspace::PageKey;

pub(super) fn turn_started(input: &TurnInput<'_>, settings: &TurnSettings<'_>) -> Event {
    Event::now(EventKind::TurnStarted {
        session: input.session.to_owned(),
        turn: input.turn.to_owned(),
        model: settings.model.to_owned(),
        reset: settings.reset,
        history_messages: input.history.len(),
        events: input.events.len(),
        compaction: settings.compaction.is_some(),
    })
}

pub(super) fn initial_scratch(
    context: Option<String>,
    events: Vec<Message>,
) -> (Vec<Message>, HashSet<usize>) {
    let mut scratch = Vec::with_capacity(events.len() + 1);
    let mut harness_notes = HashSet::new();
    if let Some(context) = context.filter(|context| !context.trim().is_empty()) {
        harness_notes.insert(0);
        scratch.push(Message::user_text(context));
    }
    scratch.extend(events);
    (scratch, harness_notes)
}

pub(super) struct ToolTable {
    pub(super) specs: Vec<ToolSpec>,
    pub(super) retention: HashMap<String, Retention>,
    pub(super) invalidates: HashMap<String, Vec<PageKey>>,
    pub(super) ends_turn: HashSet<String>,
    pub(super) idempotent: HashSet<String>,
}

pub(super) fn offered_skills(skills: &[Skill], builtin: BuiltinTools) -> &[Skill] {
    if builtin.offers(LOAD_SKILL) {
        skills
    } else {
        &[]
    }
}

pub(super) fn tool_table(
    external_tools: &[ToolDef],
    skills: &[Skill],
    require_end_turn: bool,
    builtin: BuiltinTools,
    compacting: bool,
) -> Result<ToolTable, TurnError> {
    let tools: Vec<ToolDef> = builtin_tools(require_end_turn)
        .into_iter()
        .filter(|tool| {
            builtin.offers(&tool.spec.name) || (compacting && tool.spec.name == CHECKPOINT)
        })
        .chain((!skills.is_empty()).then(|| load_skill_tool(skills)))
        .chain(external_tools.iter().cloned())
        .collect();
    let mut retention = HashMap::new();
    let mut invalidates = HashMap::new();
    let mut ends_turn = HashSet::new();
    let mut idempotent = HashSet::new();
    for tool in &tools {
        if retention
            .insert(tool.spec.name.clone(), tool.retention)
            .is_some()
        {
            return Err(TurnError::DuplicateTool(tool.spec.name.clone()));
        }
        if !tool.invalidates.is_empty() {
            invalidates.insert(tool.spec.name.clone(), tool.invalidates.clone());
        }
        if tool.ends_turn {
            ends_turn.insert(tool.spec.name.clone());
        }
        if tool.idempotent {
            idempotent.insert(tool.spec.name.clone());
        }
    }
    let specs = tools
        .into_iter()
        .map(|tool| {
            let mut spec = tool.spec;
            if tool.ends_turn {
                spec.description.push_str(ENDS_TURN_NOTE);
            }
            spec
        })
        .collect();
    Ok(ToolTable {
        specs,
        retention,
        invalidates,
        ends_turn,
        idempotent,
    })
}
