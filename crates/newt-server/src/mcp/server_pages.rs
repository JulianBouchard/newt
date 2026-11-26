//! Every page an agent's servers offer one session, as one page source: the
//! home's pages at the root, each added server's under its folder.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::join_all;
use newt_core::workspace::{
    Invalidator, Listing, ListingFuture, Page, PageFuture, PageKey, PageSource,
};

use super::Owner;
use super::pages::McpPages;

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
    /// Every page the agent's servers list. The home's failure fails the
    /// listing; an added server's only leaves its own pages out.
    fn list(&self) -> ListingFuture<'_> {
        Box::pin(async move {
            let listings = join_all(
                self.servers
                    .iter()
                    .map(|server| async move { (server, server.listing().await) }),
            )
            .await;
            let mut pages = Vec::new();
            for (server, listed) in listings {
                match listed {
                    Ok(listed) => pages.extend(listed),
                    Err(error) if server.owner == Owner::HOME => return Err(error),
                    Err(error) => {
                        tracing::warn!(server = %server.server, %error, "MCP server could not list its pages; keeping its last listing");
                        pages.extend(server.remembered());
                    }
                }
            }
            check_entries(
                pages
                    .iter()
                    .map(|listed| (listed.path.as_str(), &listed.key)),
            )?;
            Ok(Listing {
                pages,
                templates: self
                    .servers
                    .iter()
                    .flat_map(|server| server.templates())
                    .collect(),
            })
        })
    }

    /// The pages for these keys from the server that owns each. A server's
    /// failure fails the read: the pages asked for are not gone, only
    /// unreadable for now.
    fn read(&self, keys: Vec<PageKey>) -> PageFuture<'_> {
        Box::pin(async move {
            let reads = self.servers.iter().filter_map(|server| {
                let owned: Vec<PageKey> = keys
                    .iter()
                    .filter(|key| server.owner.may_own(key))
                    .cloned()
                    .collect();
                (!owned.is_empty())
                    .then_some(async move { (server, server.read_owned(&owned).await) })
            });
            let mut pages = Vec::new();
            for (_, read) in join_all(reads).await {
                pages.extend(read?);
            }
            check_pages(&pages)?;
            Ok(pages)
        })
    }
}

/// Checks a list of pages: every page has a path and a key, and no path
/// appears twice.
pub fn check_pages(pages: &[Page]) -> Result<(), String> {
    check_entries(pages.iter().map(|page| (page.path.as_str(), &page.key)))
}

pub(super) fn check_entries<'a>(
    entries: impl Iterator<Item = (&'a str, &'a PageKey)>,
) -> Result<(), String> {
    let mut paths = HashSet::new();
    for (path, key) in entries {
        if path.trim().is_empty() || key.is_empty() {
            return Err("every page needs a path and a key".to_owned());
        }
        if !paths.insert(path) {
            return Err(format!("page path `{path}` appears twice"));
        }
    }
    Ok(())
}
