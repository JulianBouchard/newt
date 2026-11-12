use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::message::ToolCall;
use crate::prompt::ToolSpec;
use crate::workspace::{Page, PageKey, Workspace};

pub const END_TURN: &str = "end_turn";
pub const REMEMBER: &str = "remember";
pub const REMEMBER_LIMIT: usize = 1000;
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
    pub invalidates: Vec<PageKey>,
}

impl ToolDef {
    pub fn new(spec: ToolSpec, retention: Retention) -> Self {
        Self {
            spec,
            retention,
            invalidates: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalidate: Vec<PageKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<Page>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PageRead {
    pub path: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CallContext {
    pub idempotency_key: String,
    pub skills_loaded: Vec<String>,
    pub pages_read: Vec<PageRead>,
}

pub trait ToolHost: Send + Sync {
    fn call(
        &self,
        call: &ToolCall,
        context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoTools;

impl ToolHost for NoTools {
    fn call(
        &self,
        call: &ToolCall,
        _context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        std::future::ready(ToolOutput::error(format!("unknown tool `{}`", call.name)))
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

pub(crate) fn is_workspace_tool(name: &str) -> bool {
    matches!(name, "read_file" | "glob" | "grep")
}

pub(crate) async fn run_workspace_tool(call: &ToolCall, workspace: &Workspace) -> ToolOutput {
    let outcome = match call.name.as_str() {
        "read_file" => match parse::<ReadFileArgs>(call) {
            Ok(args) => read_file(workspace, &args.paths).await,
            Err(message) => Err(message),
        },
        "glob" => match parse::<GlobArgs>(call) {
            Ok(args) => glob(workspace, &args.pattern).await,
            Err(message) => Err(message),
        },
        "grep" => match parse::<GrepArgs>(call) {
            Ok(args) => grep(workspace, &args).await,
            Err(message) => Err(message),
        },
        other => Err(format!("`{other}` is not a workspace tool")),
    };
    match outcome {
        Ok(content) => ToolOutput::ok(content),
        Err(message) => ToolOutput::error(message),
    }
}

fn parse<'a, T: Deserialize<'a>>(call: &'a ToolCall) -> Result<T, String> {
    T::deserialize(&call.arguments)
        .map_err(|error| format!("invalid arguments for {}: {error}", call.name))
}

async fn read_file(workspace: &Workspace, paths: &[String]) -> Result<String, String> {
    if paths.is_empty() {
        return Err("read_file needs at least one path".to_owned());
    }
    let mut out = String::new();
    for path in paths {
        let content = workspace
            .read(path)
            .await
            .map_err(|error| error.to_string())?;
        let content = content.as_deref().unwrap_or("(no such page)");
        for piece in ["--- ", path, " ---\n", content, "\n"] {
            out.push_str(piece);
        }
    }
    Ok(out)
}

async fn glob(workspace: &Workspace, pattern: &str) -> Result<String, String> {
    let paths = workspace
        .glob(pattern)
        .await
        .map_err(|error| error.to_string())?;
    if paths.is_empty() {
        return Ok("(no matching pages)".to_owned());
    }
    Ok(paths.join("\n"))
}

async fn grep(workspace: &Workspace, args: &GrepArgs) -> Result<String, String> {
    let matches = workspace
        .grep(&args.pattern, args.glob.as_deref(), GREP_LIMIT + 1)
        .await
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

    #[tokio::test]
    async fn read_file_reads_several_pages_and_marks_missing_ones() {
        let result = run_workspace_tool(
            &call("read_file", json!({ "paths": ["profile.md", "nope.md"] })),
            &workspace(),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "--- profile.md ---\nRemote only\n--- nope.md ---\n(no such page)\n"
        );
    }

    #[tokio::test]
    async fn read_file_without_paths_is_an_error() {
        let result =
            run_workspace_tool(&call("read_file", json!({ "paths": [] })), &workspace()).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn grep_says_when_matches_were_cut_off() {
        let many = "hit\n".repeat(GREP_LIMIT + 5);
        let workspace = Workspace::new(BTreeMap::from([("big.md".into(), many)]));
        let result =
            run_workspace_tool(&call("grep", json!({ "pattern": "hit" })), &workspace).await;
        assert_eq!(result.content.lines().count(), GREP_LIMIT + 1);
        assert!(result.content.contains("more matches not shown"));
    }

    #[tokio::test]
    async fn bad_arguments_become_error_results() {
        let result =
            run_workspace_tool(&call("grep", json!({ "pattern": "(" })), &workspace()).await;
        assert!(result.is_error);
        let result = run_workspace_tool(&call("glob", json!({})), &workspace()).await;
        assert!(result.is_error);
    }

    #[test]
    fn only_read_glob_and_grep_are_workspace_tools() {
        assert!(is_workspace_tool("grep"));
        assert!(!is_workspace_tool("send_message"));
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
