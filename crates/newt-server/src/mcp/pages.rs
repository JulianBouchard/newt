//! Workspace pages served as MCP resources: listed and read for one session,
//! and marked stale when the server reports a resource updated. The home's
//! pages sit at the root of the workspace; each added server's sit under its
//! own folder, with keys scoped to it.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::try_join_all;
use newt_core::workspace::{Invalidator, Listed, Page, PageKey};
use rmcp::model::{
    PaginatedRequestParams, ReadResourceRequestParams, RequestMetaObject, ResourceContents,
    ServerNotification, SubscriptionFilter, sha256_digest,
};
use serde_json::Value;
use tokio::sync::watch;

use super::server_pages::{check_entries, check_pages};
use super::{Client, META_SESSION, META_STATE, META_STATE_PATCH, Owner, meta_field};
use crate::backend::SessionState;

/// A resource's page key, when it should not be its URI.
pub const META_KEY: &str = "newt/key";
const SKILL_SCHEME: &str = "skill://";

/// The resources one server lists for one session, as workspace pages.
pub(super) struct McpPages {
    client: Arc<Client>,
    pub(super) server: String,
    session: String,
    speaks_newt: bool,
    pub(super) owner: Owner,
    state: SessionState,
    /// The added servers' folders, which the home's pages may not use.
    folders: Arc<HashSet<String>>,
    listed: watch::Sender<Arc<Vec<Known>>>,
    relisting: tokio::sync::Mutex<()>,
}

/// A resource as the last listing named it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Known {
    uri: String,
    listed: Listed,
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
        folders: Arc<HashSet<String>>,
    ) -> Self {
        Self {
            client,
            server: server.to_owned(),
            session: session.to_owned(),
            speaks_newt,
            owner,
            state,
            folders,
            listed: watch::Sender::new(Arc::new(Vec::new())),
            relisting: tokio::sync::Mutex::new(()),
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

    async fn list(&self) -> Result<Vec<Known>, String> {
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
                if resource.name.trim().is_empty() {
                    return Err(format!(
                        "MCP server `{}`: resource `{}` has an empty name",
                        self.server, resource.uri
                    ));
                }
                if self.owner == Owner::HOME
                    && let Some((folder, _)) = resource.name.split_once('/')
                    && self.folders.contains(folder)
                {
                    return Err(format!(
                        "MCP server `{}`: resource `{}` is named `{}`, inside the folder of added server `{folder}`",
                        self.server, resource.uri, resource.name
                    ));
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
        let uris: Vec<Known> = listed
            .into_iter()
            .map(|(resource, key)| Known {
                listed: Listed {
                    path: self.owner.path(&resource.name),
                    key,
                    size: resource.size,
                    description: resource.description,
                    priority: resource
                        .annotations
                        .as_ref()
                        .and_then(|annotations| annotations.priority),
                },
                uri: resource.uri,
            })
            .collect();
        check_entries(
            uris.iter()
                .map(|known| (known.listed.path.as_str(), &known.listed.key)),
        )
        .map_err(|error| format!("MCP server `{}`: {error}", self.server))?;
        self.listed.send_if_modified(|current| {
            let changed = **current != uris;
            if changed {
                *current = Arc::new(uris.clone());
            }
            changed
        });
        Ok(uris)
    }

    async fn read(&self, known: &Known) -> Result<Option<Page>, String> {
        let mut params = ReadResourceRequestParams::new(known.uri.clone());
        params.meta = self.meta();
        let result = self.client.read_resource(params).await.map_err(|error| {
            format!(
                "MCP server `{}`: reading `{}`: {error}",
                self.server, known.uri
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
                        self.server, known.uri
                    ));
                }
            }
        }
        let text = result
            .contents
            .into_iter()
            .find_map(|contents| match contents {
                ResourceContents::TextResourceContents { uri, text, .. } if uri == known.uri => {
                    Some(text)
                }
                _ => None,
            });
        let Some(content) = text else {
            tracing::warn!(server = %self.server, uri = %known.uri, "MCP resource is not text; left out of the workspace");
            return Ok(None);
        };
        Ok(Some(Page {
            key: known.listed.key.clone(),
            path: known.listed.path.clone(),
            version: sha256_digest(content.as_bytes()),
            content,
        }))
    }

    /// Marks a page stale whenever the server reports its resource updated,
    /// and every page when the server's resource list changes. Follows the
    /// URIs of the latest listing until the workspace or the stream ends.
    pub(super) async fn watch(self: Arc<Self>, invalidator: Invalidator) {
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
                .resource_subscriptions(uris.iter().map(|known| known.uri.clone()))
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
                                .filter(|known| known.uri == update.params.uri)
                                .map(|known| known.listed.key.clone())
                                .collect(),
                            Ok(Some(ServerNotification::ResourceListChangedNotification(_))) => {
                                if invalidator.relist().is_none() {
                                    return;
                                }
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
    pub(super) async fn listing(&self) -> Result<Vec<Listed>, String> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .map(|known| known.listed)
            .collect())
    }

    /// The pages as the last listing that worked named them.
    pub(super) fn remembered(&self) -> Vec<Listed> {
        self.listed
            .borrow()
            .iter()
            .map(|known| known.listed.clone())
            .collect()
    }

    /// The pages for `keys`, read by the URIs the last listing gave them;
    /// lists again first, once for every caller waiting, when one of them is
    /// not known yet.
    pub(super) async fn read_owned(&self, keys: &[PageKey]) -> Result<Vec<Page>, String> {
        let unknown = |known: &[Known]| {
            keys.iter()
                .any(|key| !known.iter().any(|resource| &resource.listed.key == key))
        };
        let mut known = Arc::clone(&self.listed.borrow());
        if unknown(&known) {
            let _relisting = self.relisting.lock().await;
            known = Arc::clone(&self.listed.borrow());
            if unknown(&known) {
                self.list().await?;
                known = Arc::clone(&self.listed.borrow());
            }
        }
        let wanted = known
            .iter()
            .filter(|resource| keys.contains(&resource.listed.key));
        let pages = try_join_all(wanted.map(|resource| self.read(resource))).await?;
        let pages: Vec<Page> = pages.into_iter().flatten().collect();
        check_pages(&pages).map_err(|error| format!("MCP server `{}`: {error}", self.server))?;
        Ok(pages)
    }
}
