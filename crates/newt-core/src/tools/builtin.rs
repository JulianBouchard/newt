use serde::Deserialize;
use serde_json::json;

use super::{Retention, ToolDef, ToolOutput, parse};
use crate::batch_tool::BATCH;
use crate::checkpoint::CHECKPOINT;
use crate::message::ToolCall;
use crate::prompt::ToolSpec;
use crate::skills::LOAD_SKILL;

pub const END_TURN: &str = "end_turn";
pub const READ_FILE: &str = "read_file";
pub const REMEMBER: &str = "remember";
pub const REMEMBER_LIMIT: usize = 1000;

const END_TURN_STRICT: &str = "End the turn. Every turn ends with this call. List what you recorded this turn, or say why there was nothing new to record.";

const END_TURN_OPTIONAL: &str = "End the turn without a reply. A reply with no tool calls already ends the turn, so use this when you end on an action: put it in the same response as your last tool call to save a model call. Optionally list what you recorded this turn, or say why there was nothing new.";

/// One of newt's own tools: its name, and how it is defined for a turn.
/// `load_skill` has no definition here, as the skills index defines it.
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    pub name: &'static str,
    define: Option<fn(bool) -> ToolDef>,
}

/// Every tool newt itself offers, in the order the model sees them. A new
/// built-in is one entry here, and its handler where the turn plans calls.
pub const BUILTINS: [Builtin; 8] = [
    Builtin {
        name: READ_FILE,
        define: Some(|_| {
            ephemeral(
                READ_FILE,
                "Read one or more workspace pages. Pages change between turns: read again rather than relying on an earlier read.",
                json!({
                    "type": "object",
                    "properties": { "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1 } },
                    "required": ["paths"],
                }),
            )
        }),
    },
    Builtin {
        name: "glob",
        define: Some(|_| {
            ephemeral(
                "glob",
                "List workspace pages whose path matches a glob pattern, such as `*.md` or `roles/**`.",
                json!({
                    "type": "object",
                    "properties": { "pattern": { "type": "string" } },
                    "required": ["pattern"],
                }),
            )
        }),
    },
    Builtin {
        name: "grep",
        define: Some(|_| {
            ephemeral(
                "grep",
                "Search workspace pages line by line with a regular expression, optionally only in pages matching a glob.",
                json!({
                    "type": "object",
                    "properties": { "pattern": { "type": "string" }, "glob": { "type": "string" } },
                    "required": ["pattern"],
                }),
            )
        }),
    },
    Builtin {
        name: END_TURN,
        define: Some(|require_end_turn| {
            ephemeral(
                END_TURN,
                if require_end_turn {
                    END_TURN_STRICT
                } else {
                    END_TURN_OPTIONAL
                },
                json!({
                    "type": "object",
                    "properties": {
                        "recorded": { "type": "array", "items": { "type": "string" } },
                        "nothing_new": { "type": "string" },
                    },
                }),
            )
        }),
    },
    Builtin {
        name: REMEMBER,
        define: Some(|_| {
            ToolDef::new(
                ToolSpec {
                    name: REMEMBER.to_owned(),
                    description: format!(
                        "Save one durable fact worth keeping across turns: a preference, a constraint in the person's words, a commitment you made. One fact per call, at most {REMEMBER_LIMIT} characters. Facts the workspace already shows need no saving."
                    ),
                    input_schema: json!({
                        "type": "object",
                        "properties": { "note": { "type": "string" } },
                        "required": ["note"],
                    }),
                },
                Retention::Durable,
            )
        }),
    },
    Builtin {
        name: BATCH,
        define: Some(|_| crate::batch_tool::batch_tool()),
    },
    Builtin {
        name: CHECKPOINT,
        define: Some(|_| crate::checkpoint::checkpoint_tool()),
    },
    Builtin {
        name: LOAD_SKILL,
        define: None,
    },
];

fn ephemeral(name: &str, description: &str, input_schema: serde_json::Value) -> ToolDef {
    ToolDef::new(
        ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema,
        },
        Retention::Ephemeral,
    )
}

/// The built-ins that define their own tool, for a turn that does or does
/// not require `end_turn`.
pub fn builtin_tools(require_end_turn: bool) -> Vec<ToolDef> {
    BUILTINS
        .iter()
        .filter_map(|builtin| builtin.define.map(|define| define(require_end_turn)))
        .collect()
}

#[derive(Deserialize)]
struct RememberArgs {
    note: String,
}

pub(crate) fn remember(call: &ToolCall) -> ToolOutput {
    match parse::<RememberArgs>(call) {
        Ok(args) if args.note.trim().is_empty() => {
            ToolOutput::error("remember needs a non-empty note")
        }
        Ok(args) if args.note.chars().count() > REMEMBER_LIMIT => ToolOutput::error(format!(
            "A note holds at most {REMEMBER_LIMIT} characters: save the fact, not the conversation."
        )),
        Ok(_) => ToolOutput {
            summary: Some("Remembered.".to_owned()),
            ..ToolOutput::ok("Remembered.")
        },
        Err(message) => ToolOutput::error(message),
    }
}
