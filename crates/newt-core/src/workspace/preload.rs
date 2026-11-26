use std::sync::Arc;

use super::{Listed, PageKey, Workspace};

impl Workspace {
    /// Reads, in the background, every listed page whose priority is at
    /// least `at_least`, so the turn finds the pages its server marked as
    /// needed already loaded.
    pub fn preload(&self, at_least: f32) {
        self.fetch_ahead(move |page| page.priority.is_some_and(|priority| priority >= at_least));
    }

    /// Reads, in the background, the listed pages with these keys: the
    /// pages a tool result linked to, which the model is likely to read
    /// next.
    pub fn prefetch(&self, keys: Vec<PageKey>) {
        if keys.is_empty() {
            return;
        }
        self.fetch_ahead(move |page| keys.contains(&page.key));
    }

    /// Fetches the listed pages `wanted` picks that are not loaded, as one
    /// fetch a read of any of them shares. They do not count as read until
    /// the model reads them, and a failure is left for that read to retry
    /// and report.
    fn fetch_ahead(&self, wanted: impl Fn(&Listed) -> bool + Send + 'static) {
        let weak = Arc::downgrade(&self.inner);
        let task = tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let workspace = Self { inner };
            let Ok(index) = workspace.index().await else {
                return;
            };
            let claimed = {
                let mut state = workspace.lock();
                let free: Vec<String> = index
                    .pages
                    .into_iter()
                    .filter(|page| wanted(page))
                    .map(|page| page.path)
                    .filter(|path| !state.in_flight.contains_key(path) && state.needs_fetch(path))
                    .collect();
                if free.is_empty() {
                    return;
                }
                Self::claim(&mut state, free.into_iter())
            };
            if let Err(error) = workspace.fetch_claimed(&claimed).await {
                tracing::debug!(%error, "a page did not load ahead; reading it will retry");
            }
        });
        self.track(task.abort_handle());
    }
}
