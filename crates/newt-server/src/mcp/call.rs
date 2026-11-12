//! Making one MCP tool call: the request `_meta`, the quiet and total limits,
//! cancelling an abandoned call, and turning the result into a tool output.

use std::future::Future;
use std::sync::atomic::Ordering;

use newt_core::message::ToolCall;
use newt_core::tools::{CallContext, ToolHost, ToolOutput};
use newt_core::workspace::{Page, PageKey};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotification,
    CancelledNotificationParam, ClientRequest, ContentBlock, RequestId, RequestMetaObject,
    ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions};
use rmcp::{RoleClient, ServiceError};
use serde_json::Value;

use super::{
    META_IDEMPOTENCY_KEY, META_INVALIDATE, META_PAGES, META_PAGES_READ, META_SESSION,
    META_SKILLS_LOADED, META_SUMMARY, McpHost, Remote, meta_field,
};

const META_TRACEPARENT: &str = "traceparent";
const OUTCOME_UNKNOWN: &str = "it may or may not have taken effect. Do not repeat it: check the workspace first, and tell the person if you cannot confirm it.";
const BAD_META: &str = "(newt could not read this tool's workspace metadata, so every page will be refetched before its next read)";

impl McpHost {
    async fn run(&self, call: &ToolCall, context: &CallContext) -> ToolOutput {
        let Some(remote) = self.tools.get(&call.name) else {
            return ToolOutput::error(format!("unknown tool `{}`", call.name));
        };
        let Value::Object(arguments) = call.arguments.clone() else {
            return ToolOutput::error(format!("arguments for {} must be a JSON object", call.name));
        };
        let mut meta = RequestMetaObject::new();
        meta.insert(META_SESSION.to_owned(), Value::from(self.session.as_str()));
        meta.insert(
            META_IDEMPOTENCY_KEY.to_owned(),
            Value::from(context.idempotency_key.as_str()),
        );
        meta.insert(
            META_SKILLS_LOADED.to_owned(),
            serde_json::json!(context.skills_loaded),
        );
        meta.insert(
            META_PAGES_READ.to_owned(),
            serde_json::json!(context.pages_read),
        );
        if let Some(traceparent) = &context.traceparent {
            meta.insert(
                META_TRACEPARENT.to_owned(),
                Value::from(traceparent.as_str()),
            );
        }
        let mut params = CallToolRequestParams::new(call.name.clone()).with_arguments(arguments);
        params.meta = Some(meta);
        match self.send(remote, params).await {
            Ok(ServerResult::CallToolResult(result)) => tool_output(result),
            Ok(ServerResult::InputRequiredResult(_)) => {
                tracing::warn!(server = %remote.server, tool = %call.name, "MCP tool asked for input newt cannot give yet");
                ToolOutput::error(
                    "The tool asked for more input before it would act, which this runtime cannot give it yet, so nothing was done.",
                )
            }
            Ok(_) => {
                tracing::warn!(server = %remote.server, tool = %call.name, "MCP tool answered with something other than a result");
                ToolOutput::error(format!(
                    "The tool answered with something other than a result, so {OUTCOME_UNKNOWN}"
                ))
            }
            Err(ServiceError::McpError(error)) => {
                tracing::warn!(server = %remote.server, tool = %call.name, %error, "MCP call returned an error");
                ToolOutput::error(format!("The tool call failed: {}", error.message))
            }
            Err(ServiceError::Timeout { timeout }) => {
                tracing::warn!(server = %remote.server, tool = %call.name, ?timeout, "MCP call timed out");
                ToolOutput::error(if timeout >= self.limits.total {
                    format!(
                        "The tool call did not finish within {} seconds, so {OUTCOME_UNKNOWN}",
                        self.limits.total.as_secs()
                    )
                } else {
                    format!(
                        "The tool call sent no result or progress for {} seconds, so {OUTCOME_UNKNOWN}",
                        self.limits.quiet.as_secs()
                    )
                })
            }
            Err(error) => {
                if matches!(
                    error,
                    ServiceError::TransportSend(_) | ServiceError::TransportClosed
                ) {
                    self.stale.store(true, Ordering::Relaxed);
                }
                tracing::warn!(server = %remote.server, tool = %call.name, %error, "MCP call lost its connection");
                ToolOutput::error(format!(
                    "The connection to the tool broke during the call, so {OUTCOME_UNKNOWN}"
                ))
            }
        }
    }

    async fn send(
        &self,
        remote: &Remote,
        params: CallToolRequestParams,
    ) -> Result<ServerResult, ServiceError> {
        let mut options = PeerRequestOptions::with_timeout(self.limits.quiet);
        options.reset_timeout_on_progress = true;
        options.max_total_timeout = Some(self.limits.total);
        let handle = remote
            .client
            .send_request_with_option(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                options,
            )
            .await?;
        let mut abandoned = CancelIfAbandoned(Some((handle.peer.clone(), handle.id.clone())));
        let response = handle.await_response().await;
        abandoned.0 = None;
        response
    }
}

struct CancelIfAbandoned(Option<(Peer<RoleClient>, RequestId)>);

impl Drop for CancelIfAbandoned {
    fn drop(&mut self) {
        let Some((peer, id)) = self.0.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let notification = CancelledNotification::new(CancelledNotificationParam::new(
                Some(id),
                Some("the turn that made this call ended".to_owned()),
            ));
            if let Err(error) = peer.send_notification(notification.into()).await {
                tracing::debug!(%error, "could not cancel an abandoned MCP call");
            }
        });
    }
}

impl ToolHost for McpHost {
    fn call(
        &self,
        call: &ToolCall,
        context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        self.run(call, context)
    }
}

fn tool_output(result: CallToolResult) -> ToolOutput {
    let mut content: Vec<String> = result
        .content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text,
            ContentBlock::Image(_) => "[image]".to_owned(),
            ContentBlock::Audio(_) => "[audio]".to_owned(),
            _ => "[resource]".to_owned(),
        })
        .collect();
    if content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        content.push(structured.to_string());
    }
    let mut output = ToolOutput {
        content: content.join("\n"),
        is_error: result.is_error == Some(true),
        ..ToolOutput::default()
    };
    let meta = result.meta.as_deref();
    let summary = meta_field::<String>(meta, META_SUMMARY);
    let invalidate = meta_field::<Vec<PageKey>>(meta, META_INVALIDATE);
    let pages = meta_field::<Vec<Page>>(meta, META_PAGES).and_then(|pages| {
        if let Some(pages) = &pages {
            crate::context::check_pages(pages)?;
        }
        Ok(pages)
    });
    match (summary, invalidate, pages) {
        (Ok(summary), Ok(invalidate), Ok(pages)) => {
            output.summary = summary;
            output.invalidate = invalidate.unwrap_or_default();
            output.pages = pages.unwrap_or_default();
        }
        (summary, invalidate, pages) => {
            let problems: Vec<String> = [summary.err(), invalidate.err(), pages.err()]
                .into_iter()
                .flatten()
                .collect();
            tracing::error!(problems = %problems.join("; "), "MCP result had invalid newt metadata");
            output.invalidate = vec![Vec::new()];
            output.content.push('\n');
            output.content.push_str(BAD_META);
        }
    }
    output
}
