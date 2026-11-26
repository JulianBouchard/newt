use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Notify;

use super::{Listed, Listing, Template, Workspace, WorkspaceError};

/// A workspace's pages and collections as last listed, without contents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Index {
    pub pages: Vec<Listed>,
    pub templates: Vec<Template>,
}

enum Step {
    Ready,
    Wait(Pin<Box<tokio::sync::futures::OwnedNotified>>, u64),
    List(Arc<Notify>, Attempt),
}

#[derive(Clone, Copy)]
struct Attempt {
    number: u64,
    clock: u64,
    epoch: u64,
}

struct ListingDone<'a> {
    workspace: &'a Workspace,
    notify: Arc<Notify>,
}

impl Drop for ListingDone<'_> {
    fn drop(&mut self) {
        let mut state = self.workspace.lock();
        if state
            .listing
            .as_ref()
            .is_some_and(|listing| Arc::ptr_eq(listing, &self.notify))
        {
            state.listing = None;
        }
        drop(state);
        self.notify.notify_waiters();
    }
}

impl Workspace {
    /// Asks for a fresh listing before the index is next used.
    pub fn relist(&self) {
        if self.inner.source.is_some() {
            let mut state = self.lock();
            state.list_wanted = true;
            state.list_epoch += 1;
        }
    }

    /// The pages and collections as last listed, listing first when a
    /// fresh listing was asked for.
    pub async fn index(&self) -> Result<Index, WorkspaceError> {
        self.listed().await?;
        let state = self.lock();
        Ok(Index {
            pages: state.index.values().cloned().collect(),
            templates: state.templates.clone(),
        })
    }

    pub(super) async fn listed(&self) -> Result<(), WorkspaceError> {
        loop {
            let step = {
                let mut state = self.lock();
                if !state.list_wanted {
                    Step::Ready
                } else if let Some(listing) = &state.listing {
                    let mut waiting = Box::pin(Arc::clone(listing).notified_owned());
                    waiting.as_mut().enable();
                    Step::Wait(waiting, state.list_attempt)
                } else {
                    let notify = Arc::new(Notify::new());
                    state.listing = Some(Arc::clone(&notify));
                    state.list_attempt += 1;
                    Step::List(
                        notify,
                        Attempt {
                            number: state.list_attempt,
                            clock: state.clock,
                            epoch: state.list_epoch,
                        },
                    )
                }
            };
            match step {
                Step::Ready => return Ok(()),
                Step::Wait(waiting, attempt) => {
                    waiting.await;
                    if let Some((failed, error)) = &self.lock().list_failure
                        && *failed == attempt
                    {
                        return Err(WorkspaceError::Refresh(error.clone()));
                    }
                }
                Step::List(notify, attempt) => {
                    let _done = ListingDone {
                        workspace: self,
                        notify,
                    };
                    let listed = match &self.inner.source {
                        Some(source) => source.list().await,
                        None => Err("this workspace has no page source".to_owned()),
                    };
                    return match listed {
                        Ok(listing) => {
                            self.apply_listing(listing, attempt);
                            Ok(())
                        }
                        Err(error) => {
                            self.lock().list_failure = Some((attempt.number, error.clone()));
                            Err(WorkspaceError::Refresh(error))
                        }
                    };
                }
            }
        }
    }

    fn apply_listing(&self, listing: Listing, attempt: Attempt) {
        let started = attempt.clock;
        let mut state = self.lock();
        let mut index: BTreeMap<String, Listed> = listing
            .pages
            .into_iter()
            .map(|listed| (listed.path.clone(), listed))
            .collect();
        let gone: Vec<String> = state
            .pages
            .keys()
            .filter(|path| !index.contains_key(*path) && state.generation_of(path) <= started)
            .cloned()
            .collect();
        for path in &gone {
            state.pages.remove(path);
        }
        let unlisted: Vec<String> = state
            .stale
            .iter()
            .filter(|path| !index.contains_key(*path) && !state.pages.contains_key(*path))
            .cloned()
            .collect();
        for path in &unlisted {
            state.stale.remove(path);
        }
        for (path, listed) in &state.index {
            if !index.contains_key(path) && state.generation_of(path) > started {
                index.insert(path.clone(), listed.clone());
            }
        }
        state.index = index;
        state.templates = listing.templates;
        state.list_failure = None;
        if state.list_epoch == attempt.epoch {
            state.list_wanted = false;
        }
    }
}
