use serde::Deserialize;

use super::{ToolOutput, parse};
use crate::message::ToolCall;
use crate::workspace::Workspace;

const GREP_LIMIT: usize = 200;

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

async fn read_file(workspace: &Workspace, paths: &[String]) -> Result<String, String> {
    if paths.is_empty() {
        return Err("read_file needs at least one path".to_owned());
    }
    let reads = futures_util::future::join_all(paths.iter().map(|path| workspace.read(path))).await;
    let mut out = String::new();
    let mut failed = false;
    for (path, read) in paths.iter().zip(reads) {
        let content = match read {
            Ok(Some(content)) => content,
            Ok(None) => "(no such page)".to_owned(),
            Err(error) => {
                failed = true;
                format!("(could not read this page: {error})")
            }
        };
        for piece in ["--- ", path, " ---\n", &content, "\n"] {
            out.push_str(piece);
        }
    }
    if failed { Err(out) } else { Ok(out) }
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
mod fuzz;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

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

    struct HalfBroken;

    impl crate::workspace::PageSource for HalfBroken {
        fn list(&self) -> crate::workspace::ListingFuture<'_> {
            Box::pin(std::future::ready(Ok(crate::workspace::Listing {
                pages: ["good.md", "broken.md"]
                    .map(|path| crate::workspace::Listed::new(vec![path.to_owned()], path))
                    .to_vec(),
                templates: Vec::new(),
            })))
        }

        fn read(&self, keys: Vec<crate::workspace::PageKey>) -> crate::workspace::PageFuture<'_> {
            Box::pin(std::future::ready(
                if keys.iter().flatten().any(|key| key == "broken.md") {
                    Err("route timed out".to_owned())
                } else {
                    Ok(vec![crate::workspace::Page {
                        key: vec!["good.md".into()],
                        path: "good.md".into(),
                        version: "1".into(),
                        content: "fine".into(),
                    }])
                },
            ))
        }
    }

    #[tokio::test]
    async fn read_file_keeps_the_pages_that_read_when_one_fails() {
        let workspace = Workspace::loading(std::sync::Arc::new(HalfBroken));
        let result = run_workspace_tool(
            &call("read_file", json!({ "paths": ["good.md", "broken.md"] })),
            &workspace,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("--- good.md ---\nfine\n"));
        assert!(
            result
                .content
                .contains("--- broken.md ---\n(could not read this page:")
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
}
