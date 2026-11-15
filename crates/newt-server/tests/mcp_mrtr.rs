#![expect(
    clippy::unwrap_used,
    clippy::unused_async_trait_impl,
    reason = "tests fail loudly on setup errors; the test server's handlers read like real async ones"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use newt_core::message::ToolCall;
use newt_core::tools::{CallContext, ToolHost};
use newt_server::backend::{SessionState, TurnState};
use newt_server::mcp::{McpAuth, McpServer, Toolset};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CreateTaskResult,
    DetailedTask, GetTaskParams, GetTaskResult, InputRequest, InputRequests, InputRequiredResult,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Task,
    TaskPayload, TaskStatus, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct Bank {
    paid: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
}

fn confirm() -> InputRequests {
    let request: InputRequest = serde_json::from_value(json!({
        "method": "elicitation/create",
        "params": {
            "mode": "form",
            "message": "Send $5 to Ana?",
            "requestedSchema": {
                "type": "object",
                "properties": { "approved": { "type": "boolean" } },
                "required": ["approved"]
            }
        }
    }))
    .unwrap();
    BTreeMap::from([("confirm".to_owned(), request)])
}

fn text(content: &str) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(content)])
}

impl ServerHandler for Bank {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = Arc::new(JsonObject::from_iter([(
            "type".to_owned(),
            json!("object"),
        )]));
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new("pay", "Pay someone.", Arc::clone(&schema)),
            Tool::new("report", "Build a long report.", schema),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name == "report" {
            let task = Task::new(
                "t1",
                TaskStatus::Working,
                "2026-11-12T00:00:00Z",
                "2026-11-12T00:00:00Z",
            );
            return Ok(CreateTaskResult::new(task).into());
        }
        if request
            .arguments
            .as_ref()
            .is_some_and(|arguments| arguments.contains_key("_input"))
        {
            return Err(ErrorData::invalid_params(
                "newt should have taken _input out",
                None,
            ));
        }
        let Some(answers) = request.input_responses else {
            return Ok(InputRequiredResult::new(Some(confirm()), Some("state-1".into())).into());
        };
        assert_eq!(request.request_state.as_deref(), Some("state-1"));
        Ok(match answers["confirm"]["action"].as_str() {
            Some("accept") if answers["confirm"]["content"]["approved"] == json!(true) => {
                self.paid.fetch_add(1, Ordering::SeqCst);
                text("paid")
            }
            _ => text("not paid"),
        }
        .into())
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        let at = "2026-11-12T00:00:00Z";
        let polls = self.polls.fetch_add(1, Ordering::SeqCst);
        let payload = if polls == 0 {
            TaskPayload::Working
        } else {
            let result = serde_json::to_value(text("report ready")).unwrap();
            let Value::Object(result) = result else {
                unreachable!()
            };
            TaskPayload::Completed { result }
        };
        let mut task = Task::new(request.task_id, TaskStatus::Working, at, at);
        task.poll_interval_ms = Some(10);
        Ok(GetTaskResult::new(DetailedTask::new(task, payload)))
    }
}

async fn connect(bank: Bank) -> Toolset {
    let service: StreamableHttpService<Bank, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(bank.clone()),
        Arc::default(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Toolset::connect(
        &[McpServer {
            name: "bank".into(),
            url: format!("http://{address}/mcp"),
            headers: BTreeMap::new(),
            auth: McpAuth::None,
        }],
        "agent-token",
    )
    .await
    .unwrap()
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments,
    }
}

#[tokio::test]
async fn a_tool_that_asks_for_approval_waits_for_the_persons_answer() {
    let bank = Bank::default();
    let paid = Arc::clone(&bank.paid);
    let host = connect(bank)
        .await
        .host("ana", TurnState::default(), SessionState::default());

    let asked = host
        .call(
            &call("pay", json!({ "to": "Ana" })),
            &CallContext::default(),
        )
        .await;
    assert!(asked.is_error);
    assert!(
        asked.content.contains("Nothing was done yet"),
        "{}",
        asked.content
    );
    assert!(
        asked.content.contains("- `confirm`: Send $5 to Ana?"),
        "{}",
        asked.content
    );
    assert!(
        asked.content.contains(r#""state":"state-1""#),
        "{}",
        asked.content
    );
    assert_eq!(asked.summary.as_deref(), Some(asked.content.as_str()));
    assert_eq!(paid.load(Ordering::SeqCst), 0);

    let declined = host
        .call(
            &call("pay", json!({ "to": "Ana", "_input": { "answers": { "confirm": "decline" }, "state": "state-1" } })),
            &CallContext::default(),
        )
        .await;
    assert_eq!(declined.content, "not paid");

    let approved = host
        .call(
            &call("pay", json!({ "to": "Ana", "_input": { "answers": { "confirm": { "approved": true } }, "state": "state-1" } })),
            &CallContext::default(),
        )
        .await;
    assert_eq!(
        (approved.content.as_str(), approved.is_error),
        ("paid", false)
    );
    assert_eq!(paid.load(Ordering::SeqCst), 1);

    let malformed = host
        .call(
            &call("pay", json!({ "_input": { "answers": { "confirm": 3 } } })),
            &CallContext::default(),
        )
        .await;
    assert!(malformed.is_error);
    assert!(
        malformed.content.contains("must be an object"),
        "{}",
        malformed.content
    );
}

#[tokio::test]
async fn a_tool_that_runs_as_a_task_is_polled_until_it_finishes() {
    let bank = Bank::default();
    let polls = Arc::clone(&bank.polls);
    let host = connect(bank)
        .await
        .host("ana", TurnState::default(), SessionState::default());
    let output = host
        .call(&call("report", json!({})), &CallContext::default())
        .await;
    assert_eq!(
        (output.content.as_str(), output.is_error),
        ("report ready", false)
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}
