use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::message::ToolCall;
use crate::prompt::ToolSpec;
use crate::workspace::Workspace;

pub const END_TURN: &str = "end_turn";
const GREP_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    Ephemeral,
    Durable,
    Outward,
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub spec: ToolSpec,
    pub retention: Retention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub summary: Option<String>,
}

pub trait ToolHost: Send + Sync {
    fn call(&self, call: &ToolCall) -> impl Future<Output = ToolOutput> + Send;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoTools;

impl ToolHost for NoTools {
    fn call(&self, call: &ToolCall) -> impl Future<Output = ToolOutput> + Send {
        std::future::ready(ToolOutput {
            content: format!("unknown tool `{}`", call.name),
            is_error: true,
            summary: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EndTurn {
    #[serde(default)]
    pub recorded: Vec<String>,
    #[serde(default)]
    pub nothing_new: Option<String>,
}

impl EndTurn {
    pub fn parse(arguments: &serde_json::Value) -> Result<Self, String> {
        let end = Self::deserialize(arguments)
            .map_err(|error| format!("invalid end_turn arguments: {error}"))?;
        let recorded = end.recorded.iter().any(|item| !item.trim().is_empty());
        let nothing_new = end
            .nothing_new
            .as_deref()
            .is_some_and(|reason| !reason.trim().is_empty());
        if recorded == nothing_new {
            return Err(
                "end_turn needs exactly one of `recorded` (what you saved this turn) or `nothing_new` (why there was nothing to save)"
                    .to_owned(),
            );
        }
        Ok(end)
    }
}

pub fn builtin_tools() -> Vec<ToolDef> {
    let ephemeral = |name: &str, description: &str, input_schema| ToolDef {
        spec: ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema,
        },
        retention: Retention::Ephemeral,
    };
    vec![
        ephemeral(
            "read_file",
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
            "End the turn. Every turn ends with this call. List what you recorded this turn, or say why there was nothing new to record.",
            json!({
                "type": "object",
                "properties": {
                    "recorded": { "type": "array", "items": { "type": "string" } },
                    "nothing_new": { "type": "string" },
                },
            }),
        ),
    ]
}

#[derive(Deserialize)]
struct ReadFileArgs {
    paths: Vec<String>,
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    glob: Option<String>,
}

pub(crate) fn run_workspace_tool(call: &ToolCall, workspace: &Workspace) -> Option<ToolOutput> {
    let outcome = match call.name.as_str() {
        "read_file" => parse(call).and_then(|args: ReadFileArgs| read_file(workspace, &args.paths)),
        "glob" => parse(call).and_then(|args: GlobArgs| glob(workspace, &args.pattern)),
        "grep" => parse(call).and_then(|args: GrepArgs| grep(workspace, &args)),
        _ => return None,
    };
    let (content, is_error) = match outcome {
        Ok(content) => (content, false),
        Err(message) => (message, true),
    };
    Some(ToolOutput {
        content,
        is_error,
        summary: None,
    })
}

fn parse<'a, T: Deserialize<'a>>(call: &'a ToolCall) -> Result<T, String> {
    T::deserialize(&call.arguments)
        .map_err(|error| format!("invalid arguments for {}: {error}", call.name))
}

fn read_file(workspace: &Workspace, paths: &[String]) -> Result<String, String> {
    if paths.is_empty() {
        return Err("read_file needs at least one path".to_owned());
    }
    Ok(paths
        .iter()
        .map(|path| {
            let content = workspace.read(path).unwrap_or("(no such page)");
            format!("--- {path} ---\n{content}\n")
        })
        .collect::<Vec<_>>()
        .concat())
}

fn glob(workspace: &Workspace, pattern: &str) -> Result<String, String> {
    let paths = workspace.glob(pattern).map_err(|error| error.to_string())?;
    if paths.is_empty() {
        return Ok("(no matching pages)".to_owned());
    }
    Ok(paths.join("\n"))
}

fn grep(workspace: &Workspace, args: &GrepArgs) -> Result<String, String> {
    let matches = workspace
        .grep(&args.pattern, args.glob.as_deref(), GREP_LIMIT + 1)
        .map_err(|error| error.to_string())?;
    if matches.is_empty() {
        return Ok("(no matches)".to_owned());
    }
    let mut out = matches
        .iter()
        .take(GREP_LIMIT)
        .map(|found| format!("{}:{}: {}\n", found.path, found.line, found.text))
        .collect::<Vec<_>>()
        .concat();
    if matches.len() > GREP_LIMIT {
        out.push_str("(more matches not shown: narrow the pattern or the glob)\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use std::collections::BTreeMap;

    use super::*;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments,
        }
    }

    fn workspace() -> Workspace {
        Workspace::new(BTreeMap::from([(
            "profile.md".into(),
            "Remote only".into(),
        )]))
    }

    #[test]
    fn read_file_reads_several_pages_and_marks_missing_ones() {
        let result = run_workspace_tool(
            &call("read_file", json!({ "paths": ["profile.md", "nope.md"] })),
            &workspace(),
        )
        .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "--- profile.md ---\nRemote only\n--- nope.md ---\n(no such page)\n"
        );
    }

    #[test]
    fn read_file_without_paths_is_an_error() {
        let result =
            run_workspace_tool(&call("read_file", json!({ "paths": [] })), &workspace()).unwrap();
        assert!(result.is_error);
    }

    #[test]
    fn grep_says_when_matches_were_cut_off() {
        let many = "hit\n".repeat(GREP_LIMIT + 5);
        let workspace = Workspace::new(BTreeMap::from([("big.md".into(), many)]));
        let result =
            run_workspace_tool(&call("grep", json!({ "pattern": "hit" })), &workspace).unwrap();
        assert_eq!(result.content.lines().count(), GREP_LIMIT + 1);
        assert!(result.content.contains("more matches not shown"));
    }

    #[test]
    fn bad_arguments_become_error_results() {
        let result =
            run_workspace_tool(&call("grep", json!({ "pattern": "(" })), &workspace()).unwrap();
        assert!(result.is_error);
        let result = run_workspace_tool(&call("glob", json!({})), &workspace()).unwrap();
        assert!(result.is_error);
    }

    #[test]
    fn non_workspace_tools_are_not_handled_here() {
        assert!(run_workspace_tool(&call("send_message", json!({})), &workspace()).is_none());
    }

    #[test]
    fn end_turn_needs_exactly_one_of_recorded_or_nothing_new() {
        assert!(EndTurn::parse(&json!({ "recorded": ["prefers remote"] })).is_ok());
        assert!(EndTurn::parse(&json!({ "nothing_new": "just a greeting" })).is_ok());
        assert!(EndTurn::parse(&json!({})).is_err());
        assert!(EndTurn::parse(&json!({ "recorded": ["x"], "nothing_new": "y" })).is_err());
        assert!(EndTurn::parse(&json!({ "recorded": [" "] })).is_err());
    }
}
