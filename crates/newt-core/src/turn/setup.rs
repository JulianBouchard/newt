use super::{
    ENDS_TURN_NOTE, Event, EventKind, HashMap, HashSet, Message, PageKey, Retention, Skill,
    ToolDef, ToolSpec, TurnError, TurnInput, TurnSettings, builtin_tools, load_skill_tool,
};

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
}

pub(super) fn tool_table(
    external_tools: &[ToolDef],
    skills: &[Skill],
    require_end_turn: bool,
) -> Result<ToolTable, TurnError> {
    let tools: Vec<ToolDef> = builtin_tools(require_end_turn)
        .into_iter()
        .chain((!skills.is_empty()).then(|| load_skill_tool(skills)))
        .chain(external_tools.iter().cloned())
        .collect();
    let mut retention = HashMap::new();
    let mut invalidates = HashMap::new();
    let mut ends_turn = HashSet::new();
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
    })
}
