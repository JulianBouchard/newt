//! The same assistant as plain MCP, for `[agents.x] mcp = ...`: the
//! session's pages as resources, an update notification when a tool changes
//! one, and a skill served over MCP.

use rmcp::ErrorData;
use rmcp::model::{
    ListResourcesResult, ListSkillsResult, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, RequestMetaObject, Resource, ResourceContents, Skill, SkillFrontmatter,
    SkillResource, SkillResources, SubscriptionFilter,
};
use rmcp::service::{SubscriptionContext, SubscriptionSink};
use serde_json::{Value, json};

use crate::Demo;

pub(crate) const PAGES: [&str; 3] = ["notes", "tasks", "sent"];

const SKILL_URI: &str = "skill://writing/SKILL.md";
const SKILL: &str = "---\nname: writing\ndescription: How to word a message to the person.\n---\n\nLead with the answer. One or two short sentences. No greeting, no sign-off.\n";

pub(crate) fn session(meta: &RequestMetaObject) -> Option<&str> {
    meta.get("newt/session").and_then(Value::as_str)
}

pub(crate) fn uri(session: &str, page: &str) -> String {
    format!("demo://{session}/{page}")
}

pub(crate) fn list(demo: &Demo, session: &str) -> ListResourcesResult {
    let resources = demo.with(session, |state| {
        PAGES
            .iter()
            .filter_map(|name| state.page(name))
            .map(|page| {
                let mut meta = rmcp::model::MetaObject::new();
                meta.insert("newt/key".to_owned(), json!(page.key));
                Resource::new(uri(session, &page.key[0]), page.path).with_meta(meta)
            })
            .collect()
    });
    ListResourcesResult::with_all_items(resources)
}

pub(crate) fn read(
    demo: &Demo,
    session: Option<&str>,
    request: &ReadResourceRequestParams,
) -> Result<ReadResourceResponse, ErrorData> {
    if request.uri == SKILL_URI {
        return Ok(ReadResourceResult::new(vec![ResourceContents::text(SKILL, SKILL_URI)]).into());
    }
    let session =
        session.ok_or_else(|| ErrorData::invalid_params("newt/session is missing", None))?;
    let page = PAGES
        .iter()
        .find(|name| request.uri == uri(session, name))
        .and_then(|name| demo.with(session, |state| state.page(name)))
        .ok_or_else(|| ErrorData::invalid_params("no such page in this session", None))?;
    Ok(ReadResourceResult::new(vec![ResourceContents::text(
        page.content,
        request.uri.clone(),
    )])
    .into())
}

pub(crate) fn skills() -> ListSkillsResult {
    ListSkillsResult::with_all_items(vec![Skill::new(
        SKILL_URI,
        SkillFrontmatter::new("writing", "How to word a message to the person."),
        SkillResources::Files(vec![SkillResource::from_bytes(SKILL_URI, SKILL.as_bytes())]),
    )])
}

pub(crate) fn accept(requested: &SubscriptionFilter) -> SubscriptionFilter {
    requested.clone()
}

pub(crate) async fn listen(demo: &Demo, context: SubscriptionContext) {
    let id = demo
        .next_listener
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    demo.listeners().push((id, context.sink().clone()));
    context.cancelled().await;
    demo.listeners().retain(|(other, _)| *other != id);
}

/// Tells every subscriber watching these pages of this session that they
/// changed.
pub(crate) async fn announce(demo: &Demo, session: &str, pages: &[String]) {
    let listeners: Vec<SubscriptionSink> = demo
        .listeners()
        .iter()
        .map(|(_, listener)| listener.clone())
        .collect();
    for page in pages {
        let uri = uri(session, page);
        for listener in &listeners {
            let watching = listener
                .accepted()
                .resource_subscriptions
                .as_ref()
                .is_some_and(|uris| uris.contains(&uri));
            if watching && let Err(error) = listener.notify_resource_updated(uri.clone()).await {
                tracing::debug!(%error, "a subscriber went away");
            }
        }
    }
}
