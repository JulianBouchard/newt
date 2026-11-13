//! Workspace pages served as MCP resources: listed and read for one session,
//! and marked stale when the server reports a resource updated.

use std::sync::Arc;

use futures_util::future::try_join_all;
use newt_core::workspace::{Invalidator, Page, PageFuture, PageKey, PageSource, key_starts_with};
use rmcp::model::{
    PaginatedRequestParams, ReadResourceRequestParams, RequestMetaObject, Resource,
    ResourceContents, ServerNotification, SubscriptionFilter, sha256_digest,
};
use serde_json::Value;
use tokio::sync::watch;

use super::{Client, META_SESSION, meta_field};

/// A resource's page key, when it should not be its URI.
pub const META_KEY: &str = "newt/key";
const SKILL_SCHEME: &str = "skill://";

/// The resources one server lists for one session, as workspace pages.
pub struct McpPages {
    client: Arc<Client>,
    server: String,
    session: String,
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
    pub(super) fn new(client: Arc<Client>, server: &str, session: &str) -> Self {
        Self {
            client,
            server: server.to_owned(),
            session: session.to_owned(),
            listed: watch::Sender::new(Arc::new(Vec::new())),
        }
    }

    fn meta(&self) -> RequestMetaObject {
        let mut meta = RequestMetaObject::new();
        meta.insert(META_SESSION.to_owned(), Value::from(self.session.as_str()));
        meta
    }

    async fn list(&self) -> Result<Vec<(Resource, PageKey)>, String> {
        let mut listed = Vec::new();
        let mut cursor = None;
        loop {
            let mut params = PaginatedRequestParams::default().with_cursor(cursor);
            params.meta = Some(self.meta());
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
                listed.push((resource, key));
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
        params.meta = Some(self.meta());
        let result = self.client.read_resource(params).await.map_err(|error| {
            format!(
                "MCP server `{}`: reading `{}`: {error}",
                self.server, resource.uri
            )
        })?;
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
            path: resource.name,
            version: sha256_digest(content.as_bytes()),
            content,
        }))
    }

    /// Marks a page stale whenever the server reports its resource updated,
    /// and every page when the server's resource list changes. Follows the
    /// URIs of the latest listing until the workspace or the stream ends.
    pub(crate) async fn watch(self: Arc<Self>, invalidator: Invalidator) {
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
                                vec![Vec::new()]
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

impl PageSource for McpPages {
    fn fetch(&self, keys: Vec<PageKey>) -> PageFuture<'_> {
        Box::pin(async move {
            let wanted = self
                .list()
                .await?
                .into_iter()
                .filter(|(_, key)| keys.iter().any(|prefix| key_starts_with(key, prefix)));
            let pages =
                try_join_all(wanted.map(|(resource, key)| self.read(resource, key))).await?;
            let pages: Vec<Page> = pages.into_iter().flatten().collect();
            crate::context::check_pages(&pages)?;
            Ok(pages)
        })
    }
}
