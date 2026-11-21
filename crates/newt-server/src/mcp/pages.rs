//! Workspace pages served as MCP resources: listed and read for one session,
//! and marked stale when the server reports a resource updated. The home's
//! pages sit at the root of the workspace; each added server's sit under its
//! own folder, with keys scoped to it.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::{join_all, try_join_all};
use newt_core::workspace::{Invalidator, Page, PageFuture, PageKey, PageSource, key_starts_with};
use rmcp::model::{
    PaginatedRequestParams, ReadResourceRequestParams, RequestMetaObject, Resource,
    ResourceContents, ServerNotification, SubscriptionFilter, sha256_digest,
};
use serde_json::Value;
use tokio::sync::watch;

use super::{Client, META_SESSION, META_STATE, META_STATE_PATCH, Owner, meta_field};
use crate::backend::SessionState;

/// A resource's page key, when it should not be its URI.
pub const META_KEY: &str = "newt/key";
const SKILL_SCHEME: &str = "skill://";

/// The resources one server lists for one session, as workspace pages.
pub(super) struct McpPages {
    client: Arc<Client>,
    server: String,
    session: String,
    speaks_newt: bool,
    owner: Owner,
    state: SessionState,
    listed: watch::Sender<Arc<Vec<(String, PageKey)>>>,
}

impl std::fmt::Debug for McpPages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpPages")
            .field("server", &self.server)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl McpPages {
    pub(super) fn new(
        client: Arc<Client>,
        server: &str,
        session: &str,
        speaks_newt: bool,
        owner: Owner,
        state: SessionState,
    ) -> Self {
        Self {
            client,
            server: server.to_owned(),
            session: session.to_owned(),
            speaks_newt,
            owner,
            state,
            listed: watch::Sender::new(Arc::new(Vec::new())),
        }
    }

    /// The session and its state, for a server that speaks newt; nothing for
    /// any other server.
    fn meta(&self) -> Option<RequestMetaObject> {
        if !self.speaks_newt {
            return None;
        }
        let mut meta = RequestMetaObject::new();
        meta.insert(META_SESSION.to_owned(), Value::from(self.session.as_str()));
        if let Some(state) = self.state.get() {
            meta.insert(META_STATE.to_owned(), state);
        }
        Some(meta)
    }

    async fn list(&self) -> Result<Vec<(Resource, PageKey)>, String> {
        let mut listed = Vec::new();
        let mut cursor = None;
        loop {
            let mut params = PaginatedRequestParams::default().with_cursor(cursor);
            params.meta = self.meta();
            let page = self
                .client
                .list_resources(Some(params))
                .await
                .map_err(|error| {
                    format!("MCP server `{}`: listing resources: {error}", self.server)
                })?;
            for resource in page.resources {
                if resource.uri.starts_with(SKILL_SCHEME) {
                    continue;
                }
                let key = meta_field::<PageKey>(resource.meta.as_deref(), META_KEY)
                    .map_err(|error| {
                        format!(
                            "MCP server `{}`, resource `{}`: {error}",
                            self.server, resource.uri
                        )
                    })?
                    .unwrap_or_else(|| vec![resource.uri.clone()]);
                listed.push((resource, self.owner.key(key)));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        let uris: Vec<(String, PageKey)> = listed
            .iter()
            .map(|(resource, key)| (resource.uri.clone(), key.clone()))
            .collect();
        self.listed.send_if_modified(|current| {
            let changed = **current != uris;
            if changed {
                *current = Arc::new(uris);
            }
            changed
        });
        Ok(listed)
    }

    async fn read(&self, resource: Resource, key: PageKey) -> Result<Option<Page>, String> {
        let mut params = ReadResourceRequestParams::new(resource.uri.clone());
        params.meta = self.meta();
        let result = self.client.read_resource(params).await.map_err(|error| {
            format!(
                "MCP server `{}`: reading `{}`: {error}",
                self.server, resource.uri
            )
        })?;
        if self.speaks_newt {
            match meta_field::<serde_json::Map<String, Value>>(
                result.meta.as_deref(),
                META_STATE_PATCH,
            ) {
                Ok(Some(patch)) => self.state.patch(&patch),
                Ok(None) => {}
                Err(why) => {
                    return Err(format!(
                        "MCP server `{}`, resource `{}`: {why}",
                        self.server, resource.uri
                    ));
                }
            }
        }
        let text = result
            .contents
            .into_iter()
            .find_map(|contents| match contents {
                ResourceContents::TextResourceContents { uri, text, .. } if uri == resource.uri => {
                    Some(text)
                }
                _ => None,
            });
        let Some(content) = text else {
            tracing::warn!(server = %self.server, uri = %resource.uri, "MCP resource is not text; left out of the workspace");
            return Ok(None);
        };
        Ok(Some(Page {
            key,
            path: self.owner.path(&resource.name),
            version: sha256_digest(content.as_bytes()),
            content,
        }))
    }

    /// Marks a page stale whenever the server reports its resource updated,
    /// and every page when the server's resource list changes. Follows the
    /// URIs of the latest listing until the workspace or the stream ends.
    async fn watch(self: Arc<Self>, invalidator: Invalidator) {
        let mut listed = self.listed.subscribe();
        loop {
            let uris = Arc::clone(&listed.borrow_and_update());
            if uris.is_empty() {
                if listed.changed().await.is_err() {
                    return;
                }
                continue;
            }
            let filter = SubscriptionFilter::builder()
                .resources_list_changed()
                .resource_subscriptions(uris.iter().map(|(uri, _)| uri.clone()))
                .build();
            let mut subscription = match self.client.peer().listen(filter).await {
                Ok(subscription)
                    if subscription
                        .acknowledged()
                        .resource_subscriptions
                        .as_ref()
                        .is_some_and(|accepted| !accepted.is_empty()) =>
                {
                    subscription
                }
                Ok(_) => {
                    tracing::debug!(server = %self.server, "MCP server does not announce resource updates");
                    return;
                }
                Err(error) => {
                    tracing::debug!(server = %self.server, %error, "MCP server offers no resource subscriptions");
                    return;
                }
            };
            loop {
                tokio::select! {
                    changed = listed.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break;
                    }
                    next = subscription.next() => {
                        let stale: Vec<PageKey> = match next {
                            Ok(Some(ServerNotification::ResourceUpdatedNotification(update))) => uris
                                .iter()
                                .filter(|(uri, _)| *uri == update.params.uri)
                                .map(|(_, key)| key.clone())
                                .collect(),
                            Ok(Some(ServerNotification::ResourceListChangedNotification(_))) => {
                                vec![self.owner.key(Vec::new())]
                            }
                            Ok(Some(_)) => continue,
                            Ok(None) | Err(_) => return,
                        };
                        if invalidator.invalidate(&stale).is_none() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

impl McpPages {
    async fn fetch_owned(&self, keys: &[PageKey]) -> Result<Vec<Page>, String> {
        let wanted = self
            .list()
            .await?
            .into_iter()
            .filter(|(_, key)| keys.iter().any(|prefix| key_starts_with(key, prefix)));
        let pages = try_join_all(wanted.map(|(resource, key)| self.read(resource, key))).await?;
        Ok(pages.into_iter().flatten().collect())
    }
}

/// The workspace pages of every server of an agent that offers resources,
/// for one session.
pub struct ServerPages {
    servers: Vec<Arc<McpPages>>,
}

impl std::fmt::Debug for ServerPages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerPages")
            .field("servers", &self.servers.len())
            .finish()
    }
}

impl ServerPages {
    pub(super) fn new(servers: Vec<McpPages>) -> Self {
        Self {
            servers: servers.into_iter().map(Arc::new).collect(),
        }
    }

    /// Marks pages stale as each server reports its resources updated, until
    /// the workspace or the streams end.
    pub async fn watch(self: Arc<Self>, invalidator: Invalidator) {
        join_all(
            self.servers
                .iter()
                .map(|server| Arc::clone(server).watch(invalidator.clone())),
        )
        .await;
    }
}

impl PageSource for ServerPages {
    fn fetch(&self, keys: Vec<PageKey>) -> PageFuture<'_> {
        Box::pin(async move {
            let fetches = self.servers.iter().filter_map(|server| {
                let owned: Vec<PageKey> = keys
                    .iter()
                    .filter(|key| server.owner.may_own(key))
                    .cloned()
                    .collect();
                (!owned.is_empty()).then_some(async move { server.fetch_owned(&owned).await })
            });
            let pages: Vec<Page> = try_join_all(fetches).await?.into_iter().flatten().collect();
            check_pages(&pages)?;
            Ok(pages)
        })
    }
}

/// Checks a list of pages: every page has a path and a key, and no path
/// appears twice.
pub fn check_pages(pages: &[Page]) -> Result<(), String> {
    let mut paths = HashSet::new();
    for page in pages {
        if page.path.trim().is_empty() || page.key.is_empty() {
            return Err("every page needs a path and a key".to_owned());
        }
        if !paths.insert(page.path.as_str()) {
            return Err(format!("page path `{}` appears twice", page.path));
        }
    }
    Ok(())
}
