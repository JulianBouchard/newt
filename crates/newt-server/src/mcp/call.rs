//! Making one MCP tool call: the request `_meta`, the quiet and total limits,
//! cancelling an abandoned call, and turning the result into a tool output.

use std::future::Future;
use std::sync::atomic::Ordering;

use futures_util::{FutureExt, StreamExt};
use newt_core::message::ToolCall;
use newt_core::skills::Skill;
use newt_core::tools::{CallContext, Progress, ToolHost, ToolOutput};
use newt_core::workspace::{Page, PageKey};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotification,
    CancelledNotificationParam, ClientRequest, ContentBlock, CustomRequest, CustomResult,
    ProgressNotificationParam, RequestId, RequestMetaObject, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions};
use rmcp::{RoleClient, ServiceError};
use serde_json::Value;

use super::{
    META_ENDS_TURN, META_IDEMPOTENCY_KEY, META_INVALIDATE, META_PAGES, META_PAGES_READ,
    META_SESSION, META_SKILLS_LOADED, META_STATE, META_STATE_PATCH, META_SUMMARY, META_TURN_STATE,
    McpHost, Owner, Remote, meta_field,
};

const META_TRACEPARENT: &str = "traceparent";
/// How long the home may take to answer `newt/turn` or `newt/turn_end`.
const CUSTOM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const OUTCOME_UNKNOWN: &str = "it may or may not have taken effect. Do not repeat it: check the workspace first, and tell the person if you cannot confirm it.";
const BAD_STATE_PATCH: &str = "(newt ignored this tool's state patch: it must be a JSON object)";
const BAD_META: &str = "(newt could not read this tool's workspace metadata, so every page will be refetched before its next read)";

impl McpHost {
    async fn run(&self, call: &ToolCall, context: &CallContext) -> ToolOutput {
        let Some(remote) = self.tools.get(&call.name) else {
            return ToolOutput::error(format!("unknown tool `{}`", call.name));
        };
        let Value::Object(mut arguments) = call.arguments.clone() else {
            return ToolOutput::error(format!("arguments for {} must be a JSON object", call.name));
        };
        let input = match super::mrtr::take_input(&mut arguments) {
            Ok(input) => input,
            Err(why) => return ToolOutput::error(why),
        };
        let meta = self.call_meta(remote, context);
        let mut params = CallToolRequestParams::new(remote.tool.clone()).with_arguments(arguments);
        params.meta = Some(meta);
        if let Some((responses, state)) = input {
            params.input_responses = Some(responses);
            params.request_state = state;
        }
        match self.send(remote, params, context.progress.as_ref()).await {
            Ok(ServerResult::CallToolResult(result)) => self.output(remote, result),
            Ok(ServerResult::InputRequiredResult(result)) => {
                tracing::info!(server = %remote.server, tool = %call.name, "MCP tool asked for the person's input");
                super::mrtr::ask(&call.name, &result)
            }
            Ok(ServerResult::CreateTaskResult(created)) => {
                match super::mrtr::finish_task(&remote.client, created).await {
                    Ok(result) => self.output(remote, result),
                    Err(why) => {
                        tracing::warn!(server = %remote.server, tool = %call.name, %why, "MCP task did not finish");
                        ToolOutput::error(format!(
                            "The tool call ran as a task, and {why}, so {OUTCOME_UNKNOWN}"
                        ))
                    }
                }
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
                    remote.stale.store(true, Ordering::Relaxed);
                }
                tracing::warn!(server = %remote.server, tool = %call.name, %error, "MCP call lost its connection");
                ToolOutput::error(format!(
                    "The connection to the tool broke during the call, so {OUTCOME_UNKNOWN}"
                ))
            }
        }
    }

    /// What a call carries in `_meta`: the idempotency key and trace context
    /// for every server, and the session data only for a server that speaks
    /// newt.
    fn call_meta(&self, remote: &Remote, context: &CallContext) -> RequestMetaObject {
        let mut meta = RequestMetaObject::new();
        meta.insert(
            META_IDEMPOTENCY_KEY.to_owned(),
            Value::from(context.idempotency_key.as_str()),
        );
        if let Some(traceparent) = &context.traceparent {
            meta.insert(
                META_TRACEPARENT.to_owned(),
                Value::from(traceparent.as_str()),
            );
        }
        if remote.speaks_newt {
            meta.insert(META_SESSION.to_owned(), Value::from(self.session.as_str()));
            meta.insert(
                META_SKILLS_LOADED.to_owned(),
                serde_json::json!(context.skills_loaded),
            );
            meta.insert(
                META_PAGES_READ.to_owned(),
                serde_json::json!(context.pages_read),
            );
            let turn_state = self.turn_state.get();
            if !turn_state.is_null() {
                meta.insert(META_TURN_STATE.to_owned(), turn_state);
            }
            if let Some(state) = self.session_state.get() {
                meta.insert(META_STATE.to_owned(), state);
            }
        }
        meta
    }

    /// The tool output. For a server that speaks newt, also takes the turn
    /// state, the session state patch and the workspace fields its result
    /// carries; any other server's `newt/*` fields are ignored.
    fn output(&self, remote: &Remote, result: CallToolResult) -> ToolOutput {
        if !remote.speaks_newt {
            return plain_output(result);
        }
        let meta = result.meta.as_deref();
        if let Ok(Some(state)) = meta_field::<Value>(meta, META_TURN_STATE) {
            self.turn_state.set(state);
        }
        let patch = meta_field::<serde_json::Map<String, Value>>(meta, META_STATE_PATCH);
        if let Ok(Some(patch)) = &patch {
            self.session_state.patch(patch);
        }
        let mut output = tool_output(result, &remote.owner);
        if let Err(why) = patch {
            tracing::error!(%why, "MCP result had a state patch that is not an object");
            output.content.push('\n');
            output.content.push_str(BAD_STATE_PATCH);
        }
        output
    }

    async fn send(
        &self,
        remote: &Remote,
        params: CallToolRequestParams,
        progress: Option<&Progress>,
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
        let dispatcher = &remote.client.service().progress;
        let token = handle.progress_token.clone();
        let mut reports = dispatcher.subscribe(token.clone()).await;
        let response = handle.await_response();
        tokio::pin!(response);
        let response = loop {
            tokio::select! {
                response = &mut response => break response,
                Some(report) = reports.next() => {
                    if let Some(progress) = progress {
                        progress.report(describe(&report));
                    }
                }
            }
        };
        while let Some(Some(report)) = reports.next().now_or_never() {
            if let Some(progress) = progress {
                progress.report(describe(&report));
            }
        }
        dispatcher.unsubscribe(&token).await;
        abandoned.0 = None;
        response
    }
}

/// A progress report in words: the server's message, or how far it got.
fn describe(report: &ProgressNotificationParam) -> String {
    match (&report.message, report.total) {
        (Some(message), _) if !message.trim().is_empty() => message.trim().to_owned(),
        (_, Some(total)) => format!("{} of {total}", report.progress),
        (_, None) => format!("{}", report.progress),
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

    fn load_skill(
        &self,
        skill: &Skill,
        file: Option<&str>,
    ) -> impl Future<Output = ToolOutput> + Send {
        self.load_served(skill, file)
    }
}

fn plain_output(result: CallToolResult) -> ToolOutput {
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
    ToolOutput {
        content: content.join("\n"),
        is_error: result.is_error == Some(true),
        ..ToolOutput::default()
    }
}

/// A newt-speaking server's result, with its workspace fields scoped to the
/// server's own pages.
pub(super) fn tool_output(result: CallToolResult, owner: &Owner) -> ToolOutput {
    let meta = result.meta.clone();
    let mut output = plain_output(result);
    let meta = meta.as_deref();
    let summary = meta_field::<String>(meta, META_SUMMARY);
    let invalidate = meta_field::<Vec<PageKey>>(meta, META_INVALIDATE);
    let ends_turn = meta_field::<bool>(meta, META_ENDS_TURN);
    let pages = meta_field::<Vec<Page>>(meta, META_PAGES).and_then(|pages| {
        if let Some(pages) = &pages {
            super::check_pages(pages)?;
        }
        let pages: Option<Vec<Page>> = pages.map(|pages| {
            pages
                .into_iter()
                .map(|page| Page {
                    key: owner.key(page.key),
                    path: owner.path(&page.path),
                    ..page
                })
                .collect()
        });
        if let Some(pages) = &pages {
            super::check_pages(pages)?;
        }
        Ok(pages)
    });
    match (summary, invalidate, pages, ends_turn) {
        (Ok(summary), Ok(invalidate), Ok(pages), Ok(ends_turn)) => {
            output.summary = summary;
            output.ends_turn = ends_turn;
            output.invalidate = invalidate
                .unwrap_or_default()
                .into_iter()
                .map(|key| owner.key(key))
                .collect();
            output.pages = pages.unwrap_or_default();
        }
        (summary, invalidate, pages, ends_turn) => {
            let problems: Vec<String> = [
                summary.err(),
                invalidate.err(),
                pages.err(),
                ends_turn.err(),
            ]
            .into_iter()
            .flatten()
            .collect();
            tracing::error!(problems = %problems.join("; "), "MCP result had invalid newt metadata");
            output.invalidate = vec![owner.key(Vec::new())];
            output.content.push('\n');
            output.content.push_str(BAD_META);
        }
    }
    output
}

/// Sends a request the newt extension defines (`newt/turn`, `newt/turn_end`)
/// and returns its result, `{}` for an empty one.
pub(super) async fn custom(
    client: &super::Client,
    server: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let request = ClientRequest::CustomRequest(CustomRequest::new(method, Some(params)));
    let handle = client
        .send_request_with_option(request, PeerRequestOptions::with_timeout(CUSTOM_TIMEOUT))
        .await
        .map_err(|error| format!("MCP server `{server}`: {method}: {error}"))?;
    match handle.await_response().await {
        Ok(ServerResult::CustomResult(CustomResult(value))) => Ok(value),
        Ok(ServerResult::EmptyResult(_)) => Ok(Value::Object(serde_json::Map::new())),
        Ok(_) => Err(format!(
            "MCP server `{server}`: {method}: answered with something other than a result"
        )),
        Err(ServiceError::McpError(error)) => Err(format!(
            "MCP server `{server}`: {method} failed: {}",
            error.message
        )),
        Err(error) => Err(format!("MCP server `{server}`: {method}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolResult, ContentBlock, MetaObject};
    use serde_json::json;

    use super::{Owner, tool_output};

    fn result_with(meta: serde_json::Value) -> CallToolResult {
        let mut fields = MetaObject::new();
        if let serde_json::Value::Object(meta) = meta {
            for (key, value) in meta {
                fields.insert(key, value);
            }
        }
        CallToolResult::success(vec![ContentBlock::text("ok")]).with_meta(Some(fields))
    }

    #[test]
    fn a_result_can_say_whether_its_call_ends_the_turn() {
        let ends = tool_output(result_with(json!({ "newt/ends_turn": true })), &Owner::HOME);
        assert_eq!(ends.ends_turn, Some(true));
        let keeps = tool_output(
            result_with(json!({ "newt/ends_turn": false })),
            &Owner::HOME,
        );
        assert_eq!(keeps.ends_turn, Some(false));
        let silent = tool_output(result_with(json!({})), &Owner::HOME);
        assert_eq!(silent.ends_turn, None);
    }
}
