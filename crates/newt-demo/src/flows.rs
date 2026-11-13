//! The demo's two tools that need more than one request: clearing the task
//! list asks the person first (MCP `input_required`), and the weekly review
//! runs as an MCP task that newt polls until it is done.

use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CreateTaskResult,
    DetailedTask, GetTaskResult, InputRequest, InputRequests, InputRequiredResult, Task,
    TaskPayload, TaskStatus,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::Demo;

/// How long the weekly review takes, so that newt has to poll it.
const REVIEW_TAKES: Duration = Duration::from_secs(2);
const REVIEW_POLL_MS: u64 = 500;

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn text(content: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(content)])
}

fn confirm(count: usize) -> Option<InputRequests> {
    let request: InputRequest = serde_json::from_value(json!({
        "method": "elicitation/create",
        "params": {
            "mode": "form",
            "message": format!("Delete all {count} tasks? This cannot be undone."),
            "requestedSchema": {
                "type": "object",
                "properties": { "approved": { "type": "boolean" } },
                "required": ["approved"]
            }
        }
    }))
    .ok()?;
    Some(InputRequests::from([("confirm".to_owned(), request)]))
}

/// Clears the task list once the person approves. Returns the response and
/// whether the tasks changed.
pub(crate) fn clear_tasks(
    demo: &Demo,
    session: &str,
    request: &CallToolRequestParams,
) -> (CallToolResponse, bool) {
    let count = demo.with(session, |state| state.tasks.len());
    let Some(answers) = &request.input_responses else {
        if count == 0 {
            return (
                CallToolResult::error(vec![ContentBlock::text("There are no tasks to clear.")])
                    .into(),
                false,
            );
        }
        let asked = InputRequiredResult::new(confirm(count), Some(format!("clear:{count}")));
        return (asked.into(), false);
    };
    let answer = answers.get("confirm").cloned().unwrap_or(Value::Null);
    let approved =
        answer["action"] == json!("accept") && answer["content"]["approved"] == json!(true);
    if !approved {
        return (
            text("{\"cleared\":0,\"approved\":false}".into()).into(),
            false,
        );
    }
    demo.with(session, |state| {
        state.tasks.clear();
        state.revision += 1;
    });
    (text(format!("{{\"cleared\":{count}}}")).into(), true)
}

/// Starts the weekly review as a task.
pub(crate) fn start_review(demo: &Demo, session: &str) -> CallToolResponse {
    let id = demo.start_review(session, Instant::now());
    let at = now();
    CreateTaskResult::new(Task::new(id, TaskStatus::Working, &at, &at)).into()
}

/// The review's state: working until [`REVIEW_TAKES`] has passed, then a
/// summary of the person's week.
pub(crate) fn review(demo: &Demo, id: &str) -> Option<GetTaskResult> {
    let (started, session) = demo.review(id)?;
    let at = now();
    let mut task = Task::new(id, TaskStatus::Working, &at, &at);
    task.poll_interval_ms = Some(REVIEW_POLL_MS);
    if started.elapsed() < REVIEW_TAKES {
        return Some(GetTaskResult::new(DetailedTask::new(
            task,
            TaskPayload::Working,
        )));
    }
    let summary = demo.with(&session, |state| {
        let open = state.tasks.iter().filter(|task| !task.done).count();
        let done = state.tasks.len() - open;
        format!(
            "Week in review: {open} open tasks, {done} done, {} notes, {} messages sent.",
            state.notes.len(),
            state.sent.len()
        )
    });
    let Ok(Value::Object(result)) = serde_json::to_value(text(summary)) else {
        return None;
    };
    task.status = TaskStatus::Completed;
    Some(GetTaskResult::new(DetailedTask::new(
        task,
        TaskPayload::Completed { result },
    )))
}
