use std::sync::Arc;

use super::Workspace;

impl Workspace {
    /// Reads, in the background, every listed page whose priority is at
    /// least `at_least`, so the turn finds the pages its server marked as
    /// needed already loaded. Preloaded pages do not count as read until
    /// the model reads them; a failed preload is left for that read to
    /// retry and report.
    pub fn preload(&self, at_least: f32) {
        let weak = Arc::downgrade(&self.inner);
        let task = tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let workspace = Self { inner };
            let Ok(index) = workspace.index().await else {
                return;
            };
            let wanted = index
                .pages
                .into_iter()
                .filter(|page| page.priority.is_some_and(|priority| priority >= at_least))
                .map(|page| page.path);
            let claimed = {
                let mut state = workspace.lock();
                let free: Vec<String> = wanted
                    .filter(|path| !state.in_flight.contains_key(path) && state.needs_fetch(path))
                    .collect();
                if free.is_empty() {
                    return;
                }
                Self::claim(&mut state, free.into_iter())
            };
            if let Err(error) = workspace.fetch_claimed(&claimed).await {
                tracing::debug!(%error, "a prioritised page did not preload; reading it will retry");
            }
        });
        self.track(task.abort_handle());
    }
}
