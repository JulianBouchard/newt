use serde::Deserialize;
use serde_json::json;

use super::{Retention, ToolDef, ToolOutput, parse};
use crate::message::ToolCall;
use crate::prompt::ToolSpec;

pub const END_TURN: &str = "end_turn";
pub const READ_FILE: &str = "read_file";
pub const REMEMBER: &str = "remember";
pub const REMEMBER_LIMIT: usize = 1000;

const END_TURN_STRICT: &str = "End the turn. Every turn ends with this call. List what you recorded this turn, or say why there was nothing new to record.";

const END_TURN_OPTIONAL: &str = "End the turn without a reply. A reply with no tool calls already ends the turn, so use this when you end on an action: put it in the same response as your last tool call to save a model call. Optionally list what you recorded this turn, or say why there was nothing new.";

pub fn builtin_tools(require_end_turn: bool) -> Vec<ToolDef> {
    let ephemeral = |name: &str, description: &str, input_schema| {
        ToolDef::new(
            ToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                input_schema,
            },
            Retention::Ephemeral,
        )
    };
    vec![
        ephemeral(
            READ_FILE,
            "Read one or more workspace pages. Pages change between turns: read again rather than relying on an earlier read.",
            json!({
                "type": "object",
                "properties": { "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1 } },
                "required": ["paths"],
            }),
        ),
        ephemeral(
            "glob",
            "List workspace pages whose path matches a glob pattern, such as `*.md` or `roles/**`.",
            json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" } },
                "required": ["pattern"],
            }),
        ),
        ephemeral(
            "grep",
            "Search workspace pages line by line with a regular expression, optionally only in pages matching a glob.",
            json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" }, "glob": { "type": "string" } },
                "required": ["pattern"],
            }),
        ),
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
        ),
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
        ),
        crate::batch_tool::batch_tool(),
        crate::checkpoint::checkpoint_tool(),
    ]
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
