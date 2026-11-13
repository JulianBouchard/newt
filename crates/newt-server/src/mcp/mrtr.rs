//! Calls that do not finish in one round trip: a tool that asks for input
//! before it acts (MCP's multi round-trip `input_required`), and a tool that
//! runs as a task newt polls (the `io.modelcontextprotocol/tasks` extension).

use std::fmt::Write as _;
use std::time::Duration;

use newt_core::tools::ToolOutput;
use rmcp::model::{
    CallToolResult, CreateTaskResult, ElicitRequestParams, GetTaskParams, InputRequest,
    InputRequiredResult, InputResponses, JsonObject, TaskPayload, TaskStatus,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Client;

/// The argument a model adds to call a tool again with the person's answers.
pub const INPUT_ARGUMENT: &str = "_input";
/// How long newt polls a task before giving up on it.
pub const TASK_LIMIT: Duration = Duration::from_hours(1);
const POLL_DEFAULT: Duration = Duration::from_secs(1);
const POLL_MIN: Duration = Duration::from_millis(250);
const POLL_MAX: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    answers: serde_json::Map<String, Value>,
    #[serde(default)]
    state: Option<String>,
}

/// Takes `_input` out of the arguments: the answers as MCP input responses
/// (an object accepts the form, `"decline"` declines it) and the server's
/// request state.
pub fn take_input(
    arguments: &mut JsonObject,
) -> Result<Option<(InputResponses, Option<String>)>, String> {
    let Some(raw) = arguments.remove(INPUT_ARGUMENT) else {
        return Ok(None);
    };
    let input: Input = serde_json::from_value(raw).map_err(|error| {
        format!(
            "`{INPUT_ARGUMENT}` must be {{ \"answers\": {{...}}, \"state\": \"...\" }}: {error}"
        )
    })?;
    let responses = input
        .answers
        .into_iter()
        .map(|(id, answer)| {
            let response = match answer {
                Value::String(word) if word == "decline" => json!({ "action": "decline" }),
                Value::Object(content) => json!({ "action": "accept", "content": content }),
                _ => {
                    return Err(format!(
                        "the answer to `{id}` must be an object of the asked fields, or \"decline\""
                    ));
                }
            };
            Ok((id, response))
        })
        .collect::<Result<InputResponses, String>>()?;
    Ok(Some((responses, input.state)))
}

/// What the model sees when a tool asks for input: the questions, and how to
/// call the tool again with the answers. Nothing has happened yet, so it is
/// an error result; it is also the summary, so a kept call keeps it whole.
pub fn ask(tool: &str, result: &InputRequiredResult) -> ToolOutput {
    let mut questions = String::new();
    let mut example = serde_json::Map::new();
    for (id, request) in result.input_requests.iter().flatten() {
        let InputRequest::Elicitation(elicit) = request else {
            return ToolOutput::error(
                "The tool asked for something other than the person's answers, which this runtime cannot give, so nothing was done.",
            );
        };
        match &elicit.params {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                let fields = serde_json::to_value(requested_schema).unwrap_or_default();
                let _ = writeln!(
                    questions,
                    "- `{id}`: {message}\n  answer fields: {}",
                    fields.get("properties").unwrap_or(&fields)
                );
                example.insert(id.clone(), json!({ "...": "..." }));
            }
            ElicitRequestParams::UrlElicitationParams { message, url, .. } => {
                let _ = writeln!(
                    questions,
                    "- `{id}`: {message}\n  the person answers at {url}, then answer `{id}` with {{}}"
                );
                example.insert(id.clone(), json!({}));
            }
            _ => {
                return ToolOutput::error(
                    "The tool asked for input in a form this runtime cannot give, so nothing was done.",
                );
            }
        }
    }
    let mut input = json!({ "answers": example });
    if let Some(state) = &result.request_state {
        input["state"] = Value::from(state.as_str());
    }
    let text = format!(
        "`{tool}` needs the person's answers before it acts. Nothing was done yet.\n{questions}Ask the person. When they answer, call `{tool}` again with the same arguments plus \"{INPUT_ARGUMENT}\": {input}, with each answer as an object of the asked fields, or \"decline\"."
    );
    ToolOutput {
        summary: Some(text.clone()),
        ..ToolOutput::error(text)
    }
}

/// Polls a task until it ends, then returns its tool result, or why there
/// is none.
pub async fn finish_task(
    client: &Client,
    created: CreateTaskResult,
) -> Result<CallToolResult, String> {
    let task_id = created.task.task_id;
    let started = tokio::time::Instant::now();
    let mut wait = POLL_DEFAULT;
    loop {
        if started.elapsed() > TASK_LIMIT {
            return Err(format!(
                "its task did not finish within {} minutes",
                TASK_LIMIT.as_secs() / 60
            ));
        }
        tokio::time::sleep(wait).await;
        let polled = client
            .get_task(GetTaskParams::new(task_id.clone()))
            .await
            .map_err(|error| format!("polling its task failed: {error}"))?;
        if let Some(interval) = polled.task.task.poll_interval_ms {
            wait = Duration::from_millis(interval).clamp(POLL_MIN, POLL_MAX);
        }
        match (polled.task.status(), polled.task.payload) {
            (_, TaskPayload::Completed { result }) => {
                return serde_json::from_value(Value::Object(result)).map_err(|error| {
                    format!("its task finished with an unreadable result: {error}")
                });
            }
            (TaskStatus::Working, _) => {}
            (TaskStatus::InputRequired, _) => {
                return Err(
                    "its task asked for input, which newt cannot give inside a task".to_owned(),
                );
            }
            (status, _) => return Err(format!("its task ended as {status:?}")),
        }
    }
}
