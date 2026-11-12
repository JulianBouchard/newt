use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::message::ToolCall;
use crate::prompt::ToolSpec;
use crate::tools::{Retention, ToolDef, ToolOutput};

pub const LOAD_SKILL: &str = "load_skill";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
}

pub(crate) fn load_skill_tool(skills: &[Skill]) -> ToolDef {
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    ToolDef::new(
        ToolSpec {
            name: LOAD_SKILL.to_owned(),
            description: "Load a skill: instructions for one kind of task, listed under <skills> in your instructions. Load it before doing the task it covers; it stays loaded for this turn only.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": { "name": { "type": "string", "enum": names } },
                "required": ["name"],
            }),
        },
        Retention::Ephemeral,
    )
}

pub(crate) fn with_index(system: &str, skills: &[Skill]) -> String {
    if skills.is_empty() {
        return system.to_owned();
    }
    let mut out = String::with_capacity(system.len() + skills.len() * 80);
    out.push_str(system);
    out.push_str("\n\n<skills>\nLoad a skill with load_skill before doing what it covers.\n");
    for skill in skills {
        out.push_str("- ");
        out.push_str(&skill.name);
        out.push_str(": ");
        out.push_str(skill.description.trim());
        out.push('\n');
    }
    out.push_str("</skills>");
    out
}

pub(crate) fn requested(call: &ToolCall) -> Option<&str> {
    call.arguments
        .get("name")
        .and_then(serde_json::Value::as_str)
}

pub(crate) fn load(call: &ToolCall, skills: &[Skill]) -> ToolOutput {
    let Some(name) = requested(call) else {
        return ToolOutput::error("load_skill needs the skill's name");
    };
    match skills.iter().find(|skill| skill.name == name) {
        Some(skill) => ToolOutput::ok(skill.content.clone()),
        None => ToolOutput::error(format!("There is no skill named `{name}`.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skills() -> Vec<Skill> {
        vec![Skill {
            name: "writing".into(),
            description: "How to word a message to the person.".into(),
            content: "Short sentences.".into(),
        }]
    }

    #[test]
    fn the_index_follows_the_instructions_and_lists_each_skill() {
        assert_eq!(with_index("Be useful.", &[]), "Be useful.");
        assert_eq!(
            with_index("Be useful.", &skills()),
            "Be useful.\n\n<skills>\nLoad a skill with load_skill before doing what it covers.\n- writing: How to word a message to the person.\n</skills>"
        );
    }

    #[test]
    fn loading_returns_the_content_or_an_error() {
        let call = |name: &str| ToolCall {
            id: "s1".into(),
            name: LOAD_SKILL.into(),
            arguments: json!({ "name": name }),
        };
        assert_eq!(
            load(&call("writing"), &skills()).content,
            "Short sentences."
        );
        assert!(load(&call("nope"), &skills()).is_error);
    }
}
