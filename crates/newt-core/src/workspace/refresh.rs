use std::sync::Arc;

use futures_util::future::join_all;
use tokio::sync::Notify;

use super::{Claim, Listed, PageKey, State, Workspace, WorkspaceError};

impl Workspace {
    /// Refetches, in the background, the stale pages the turn has read.
    pub fn refresh_read_pages_in_background(&self) {
        let active: Vec<String> = {
            let state = self.lock();
            state
                .stale
                .iter()
                .filter(|path| state.read.contains(*path))
                .cloned()
                .collect()
        };
        if active.is_empty() || self.inner.source.is_none() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let task = tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if let Err(error) = (Self { inner }).refresh(active).await {
                tracing::warn!(%error, "background page refresh failed; reads will retry");
            }
        });
        self.track(task.abort_handle());
    }

    /// Fetches those of `paths` that are listed but not loaded, or stale,
    /// waiting for any fetch of them already in flight.
    pub async fn refresh(&self, paths: Vec<String>) -> Result<(), WorkspaceError> {
        loop {
            let (claimed, waits) = {
                let mut state = self.lock();
                let outdated: Vec<String> = paths
                    .iter()
                    .filter(|path| state.needs_fetch(path))
                    .cloned()
                    .collect();
                let mut waits = Vec::new();
                let mut free = Vec::new();
                for path in outdated {
                    match state.in_flight.get(&path) {
                        Some(notify) => {
                            let mut waiting = Box::pin(Arc::clone(notify).notified_owned());
                            waiting.as_mut().enable();
                            waits.push(waiting);
                        }
                        None => free.push(path),
                    }
                }
                (Self::claim(&mut state, free.into_iter()), waits)
            };
            if claimed.claims.is_empty() && waits.is_empty() {
                return Ok(());
            }
            if !claimed.claims.is_empty() {
                self.fetch_claimed(&claimed).await?;
            }
            for waiting in waits {
                waiting.await;
            }
        }
    }

    pub(super) fn claim(state: &mut State, paths: impl Iterator<Item = String>) -> Claimed {
        let notify = Arc::new(Notify::new());
        let claims = paths
            .map(|path| {
                state.in_flight.insert(path.clone(), Arc::clone(&notify));
                Claim {
                    generation: state.generation_of(&path),
                    path,
                }
            })
            .collect();
        Claimed { claims, notify }
    }

    /// Fetches the claimed pages one by one, so a page that fails leaves
    /// the others loaded; errs naming every page that failed.
    pub(super) async fn fetch_claimed(&self, claimed: &Claimed) -> Result<(), WorkspaceError> {
        let _release = Release {
            workspace: self,
            claimed,
        };
        let Some(source) = &self.inner.source else {
            return Err(WorkspaceError::Refresh(
                "this workspace has no page source".to_owned(),
            ));
        };
        let keys: Vec<Option<PageKey>> = {
            let state = self.lock();
            claimed
                .claims
                .iter()
                .map(|claim| state.key_of(&claim.path))
                .collect()
        };
        let reads = join_all(keys.into_iter().map(|key| async move {
            match key {
                Some(key) => source.read(vec![key]).await,
                None => Ok(Vec::new()),
            }
        }))
        .await;
        let mut state = self.lock();
        let mut failures = Vec::new();
        for (claim, read) in claimed.claims.iter().zip(reads) {
            let pages = match read {
                Ok(pages) => pages,
                Err(error) => {
                    failures.push(format!("{}: {error}", claim.path));
                    continue;
                }
            };
            let unchanged = state.generation_of(&claim.path) == claim.generation;
            if unchanged {
                state.stale.remove(&claim.path);
                if !pages.iter().any(|page| page.path == claim.path) {
                    state.pages.remove(&claim.path);
                    state.index.remove(&claim.path);
                }
            }
            for page in pages {
                let fresh = page.path != claim.path || unchanged;
                if fresh || !state.pages.contains_key(&page.path) {
                    state
                        .index
                        .entry(page.path.clone())
                        .or_insert_with(|| Listed::new(page.key.clone(), page.path.clone()));
                    state.pages.insert(page.path.clone(), page);
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceError::Refresh(failures.join("; ")))
        }
    }
}

pub(super) struct Claimed {
    claims: Vec<Claim>,
    notify: Arc<Notify>,
}

struct Release<'a> {
    workspace: &'a Workspace,
    claimed: &'a Claimed,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let mut state = self.workspace.lock();
        for claim in &self.claimed.claims {
            if state
                .in_flight
                .get(&claim.path)
                .is_some_and(|notify| Arc::ptr_eq(notify, &self.claimed.notify))
            {
                state.in_flight.remove(&claim.path);
            }
        }
        drop(state);
        self.claimed.notify.notify_waiters();
    }
}
