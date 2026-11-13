use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    ListResourcesResult, ListSkillsResult, ListToolsResult, MetaObject, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ServerCapabilities, ServerConfig,
    SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{Demo, Task, resources};

/// The pages a successful call of this tool changes, from its
/// `newt/invalidates`.
fn changes(name: &str) -> Vec<String> {
    catalogue()
        .into_iter()
        .find(|tool| tool.name == name)
        .and_then(|tool| tool.meta)
        .and_then(|meta| meta.get("newt/invalidates").cloned())
        .and_then(|keys| serde_json::from_value::<Vec<Vec<String>>>(keys).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|key| key.into_iter().next())
        .collect()
}

#[derive(Clone)]
pub(crate) struct Tools(pub(crate) Arc<Demo>);

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(fields) => fields,
        _ => JsonObject::new(),
    }
}

fn meta(value: Value) -> MetaObject {
    let mut meta = MetaObject::new();
    for (key, field) in object(value) {
        meta.insert(key, field);
    }
    meta
}

fn tool(name: &'static str, description: &'static str, properties: Value, newt: Value) -> Tool {
    let required: Vec<Value> = match &properties {
        Value::Object(fields) => fields.keys().cloned().map(Value::String).collect(),
        _ => Vec::new(),
    };
    let mut schema = JsonObject::new();
    schema.insert("type".to_owned(), Value::from("object"));
    schema.insert("properties".to_owned(), properties);
    schema.insert("required".to_owned(), Value::Array(required));
    Tool::new(name, description, Arc::new(schema)).with_meta(meta(newt))
}

fn catalogue() -> Vec<Tool> {
    let text = json!({ "text": { "type": "string" } });
    vec![
        tool(
            "current_time",
            "The current date and time in UTC.",
            json!({}),
            json!({ "newt/retention": "ephemeral" }),
        ),
        tool(
            "add_note",
            "Save a fact about the person to their notes.",
            text.clone(),
            json!({ "newt/retention": "durable", "newt/invalidates": [["notes"]] }),
        ),
        tool(
            "add_task",
            "Add a task to the person's task list.",
            text.clone(),
            json!({ "newt/retention": "durable" }),
        ),
        tool(
            "complete_task",
            "Mark a task done by its number.",
            json!({ "id": { "type": "integer" } }),
            json!({ "newt/retention": "durable", "newt/invalidates": [["tasks"]] }),
        ),
        tool(
            "send_message",
            "Send a chat message to the person. This is the only way they hear from you.",
            text,
            json!({ "newt/retention": "outward", "newt/idempotent": true }),
        ),
    ]
}

fn done(content: String, newt: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(content)]).with_meta(Some(meta(newt)))
}

fn refused(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

impl Tools {
    fn run(
        &self,
        name: &str,
        arguments: &JsonObject,
        session: &str,
        key: Option<&str>,
    ) -> CallToolResult {
        let text = arguments.get("text").and_then(Value::as_str).map(str::trim);
        match (name, text) {
            ("current_time", _) => CallToolResult::success(vec![ContentBlock::text(
                OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .unwrap_or_default(),
            )]),
            ("add_note", Some(text)) if !text.is_empty() => {
                self.0.with(session, |state| {
                    state.notes.push(text.to_owned());
                    state.revision += 1;
                });
                done(
                    "{\"saved\":true}".into(),
                    json!({ "newt/summary": format!("Noted: {text}") }),
                )
            }
            ("add_task", Some(text)) if !text.is_empty() => {
                let (id, page) = self.0.with(session, |state| {
                    let id = u32::try_from(state.tasks.len()).unwrap_or(u32::MAX) + 1;
                    state.tasks.push(Task {
                        id,
                        text: text.to_owned(),
                        done: false,
                    });
                    state.revision += 1;
                    (id, state.page("tasks"))
                });
                done(
                    format!("{{\"task\":{id}}}"),
                    json!({ "newt/summary": format!("Added task #{id}: {text}"), "newt/pages": [page] }),
                )
            }
            ("complete_task", _) => {
                let Some(id) = arguments.get("id").and_then(Value::as_u64) else {
                    return refused("complete_task needs the task number as id");
                };
                let found = self.0.with(session, |state| {
                    let task = state
                        .tasks
                        .iter_mut()
                        .find(|task| u64::from(task.id) == id)?;
                    task.done = true;
                    state.revision += 1;
                    Some(task.text.clone())
                });
                match found {
                    Some(text) => done(
                        "{\"done\":true}".into(),
                        json!({ "newt/summary": format!("Completed task #{id}: {text}") }),
                    ),
                    None => refused("There is no task with that number."),
                }
            }
            ("send_message", Some(text)) if !text.is_empty() => {
                let Some(key) = key else {
                    return refused("send_message needs newt/idempotency_key");
                };
                let first_time = self.0.with(session, |state| {
                    let first_time = state.sent_keys.insert(key.to_owned());
                    if first_time {
                        state.sent.push(text.to_owned());
                        state.revision += 1;
                    }
                    first_time
                });
                if first_time {
                    tracing::info!(%session, message = %text, "sent to the person");
                }
                done(
                    "{\"delivered\":true}".into(),
                    json!({ "newt/summary": format!("Sent: {text}"), "newt/invalidate": [["sent"]] }),
                )
            }
            (_, _) => refused("Unknown tool, or a required field is missing or empty."),
        }
    }
}

impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_skills()
                .build(),
        )
        .with_instructions(crate::provider::INSTRUCTIONS)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(resources::accept(requested))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        resources::listen(&self.0, context).await;
        Ok(())
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + Send + '_ {
        std::future::ready(match resources::session(&context.meta) {
            Some(session) => Ok(resources::list(&self.0, session)),
            None => Ok(ListResourcesResult::default()),
        })
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, ErrorData>> + Send + '_ {
        std::future::ready(resources::read(
            &self.0,
            resources::session(&context.meta),
            &request,
        ))
    }

    fn list_skills(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListSkillsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(resources::skills()))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(catalogue())))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send + '_ {
        let field = |name: &str| {
            context
                .meta
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let session = field("newt/session");
        let key = field("newt/idempotency_key");
        async move {
            let Some(session) = session else {
                return Ok(refused(
                    "This server only takes calls from newt: newt/session is missing.",
                )
                .into());
            };
            let result = self.run(
                &request.name,
                &request.arguments.unwrap_or_default(),
                &session,
                key.as_deref(),
            );
            if result.is_error != Some(true) {
                resources::announce(&self.0, &session, &changes(&request.name)).await;
            }
            Ok(result.into())
        }
    }
}
