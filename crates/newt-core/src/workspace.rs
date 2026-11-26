use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

mod listing;
mod preload;
mod refresh;
mod search;
mod template;

pub use listing::Index;
use refresh::Claimed;
pub use search::GrepMatch;

pub type PageKey = Vec<String>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub key: PageKey,
    pub path: String,
    pub version: String,
    pub content: String,
}

/// A page as its source lists it, before anyone reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listed {
    pub key: PageKey,
    pub path: String,
    pub size: Option<u64>,
    pub description: Option<String>,
    pub priority: Option<f32>,
}

impl Listed {
    /// A listed page with nothing known about it but its key and path.
    pub fn new(key: PageKey, path: impl Into<String>) -> Self {
        Self {
            key,
            path: path.into(),
            size: None,
            description: None,
            priority: None,
        }
    }
}

/// A collection a source serves by pattern, such as `roles/{id}`: each
/// path it matches is a page, keyed by `key` and the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub path: String,
    pub description: Option<String>,
    pub key: PageKey,
}

/// What a source lists: its pages and its collections, no contents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listing {
    pub pages: Vec<Listed>,
    pub templates: Vec<Template>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum WorkspaceError {
    #[error("invalid glob pattern: {0}")]
    Glob(String),
    #[error("invalid regular expression: {0}")]
    Regex(String),
    #[error("could not refresh workspace pages: {0}")]
    Refresh(String),
}

pub type PageFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Page>, String>> + Send + 'a>>;
pub type ListingFuture<'a> = Pin<Box<dyn Future<Output = Result<Listing, String>> + Send + 'a>>;

/// Where a workspace's pages come from: a listing, and the contents of the
/// pages it names.
pub trait PageSource: Send + Sync {
    fn list(&self) -> ListingFuture<'_>;
    /// The pages for these keys. A key the source no longer has is left out.
    fn read(&self, keys: Vec<PageKey>) -> PageFuture<'_>;
}

#[derive(Default)]
struct State {
    pages: BTreeMap<String, Page>,
    index: BTreeMap<String, Listed>,
    templates: Vec<Template>,
    list_wanted: bool,
    list_epoch: u64,
    list_attempt: u64,
    list_failure: Option<(u64, String)>,
    listing: Option<Arc<Notify>>,
    stale: HashSet<String>,
    in_flight: HashMap<String, (Arc<Notify>, u64)>,
    claims: u64,
    failed: HashMap<String, (u64, String)>,
    read: HashSet<String>,
    generation: HashMap<String, u64>,
    clock: u64,
}

impl State {
    fn touch(&mut self, path: &str) {
        self.clock += 1;
        self.generation.insert(path.to_owned(), self.clock);
    }

    fn generation_of(&self, path: &str) -> u64 {
        self.generation.get(path).copied().unwrap_or(0)
    }

    fn key_of(&self, path: &str) -> Option<PageKey> {
        self.pages
            .get(path)
            .map(|page| page.key.clone())
            .or_else(|| self.index.get(path).map(|listed| listed.key.clone()))
    }

    fn knows(&self, path: &str) -> bool {
        self.pages.contains_key(path) || self.index.contains_key(path)
    }

    fn needs_fetch(&self, path: &str) -> bool {
        self.stale.contains(path)
            || (!self.pages.contains_key(path) && self.index.contains_key(path))
    }
}

struct Claim {
    path: String,
    generation: u64,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    source: Option<Arc<dyn PageSource>>,
    background: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let background = match self.background.get_mut() {
            Ok(background) => background,
            Err(poisoned) => poisoned.into_inner(),
        };
        for handle in background.drain(..) {
            handle.abort();
        }
    }
}

#[derive(Clone, Default)]
pub struct Workspace {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        formatter
            .debug_struct("Workspace")
            .field("listed", &state.index.len())
            .field("loaded", &state.pages.len())
            .finish_non_exhaustive()
    }
}

/// Marks pages of a workspace stale from outside the turn, such as a
/// subscription to the pages' source.
#[derive(Clone)]
pub struct Invalidator(std::sync::Weak<Inner>);

impl std::fmt::Debug for Invalidator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Invalidator")
            .finish_non_exhaustive()
    }
}

impl Invalidator {
    /// The paths marked stale, or `None` once the workspace is gone.
    pub fn invalidate(&self, prefixes: &[PageKey]) -> Option<Vec<String>> {
        let inner = self.0.upgrade()?;
        Some(Workspace { inner }.invalidate(prefixes))
    }

    /// Asks for a fresh listing before the index is next used. `None` once
    /// the workspace is gone.
    pub fn relist(&self) -> Option<()> {
        let inner = self.0.upgrade()?;
        Workspace { inner }.relist();
        Some(())
    }
}

pub fn key_starts_with(key: &[String], prefix: &[String]) -> bool {
    key.len() >= prefix.len() && key.iter().zip(prefix).all(|(a, b)| a == b)
}

enum Next {
    Done(Option<String>),
    Wait(Pin<Box<tokio::sync::futures::OwnedNotified>>, u64),
    Fetch(Claimed),
}

impl Workspace {
    pub fn new(pages: BTreeMap<String, String>) -> Self {
        let pages = pages
            .into_iter()
            .map(|(path, content)| Page {
                key: vec![path.clone()],
                path,
                version: "0".to_owned(),
                content,
            })
            .collect();
        Self::from_pages(pages, None)
    }

    pub fn with_source(pages: Vec<Page>, source: Arc<dyn PageSource>) -> Self {
        Self::from_pages(pages, Some(source))
    }

    fn from_pages(pages: Vec<Page>, source: Option<Arc<dyn PageSource>>) -> Self {
        let state = State {
            index: pages
                .iter()
                .map(|page| {
                    (
                        page.path.clone(),
                        Listed::new(page.key.clone(), page.path.clone()),
                    )
                })
                .collect(),
            pages: pages
                .into_iter()
                .map(|page| (page.path.clone(), page))
                .collect(),
            ..State::default()
        };
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                source,
                background: Mutex::new(Vec::new()),
            }),
        }
    }

    /// A workspace whose pages are listed in the background as it starts,
    /// and read only when asked for.
    pub fn loading(source: Arc<dyn PageSource>) -> Self {
        let workspace = Self::from_pages(Vec::new(), Some(source));
        workspace.relist();
        let weak = Arc::downgrade(&workspace.inner);
        let task = tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if let Err(error) = (Self { inner }).listed().await {
                tracing::warn!(%error, "workspace pages could not be listed; reads will report it");
            }
        });
        workspace.track(task.abort_handle());
        workspace
    }

    /// Runs `watch` for as long as this workspace lives. It gets an
    /// [`Invalidator`], which marks pages stale without keeping the
    /// workspace alive.
    pub fn watch<F>(&self, watch: impl FnOnce(Invalidator) -> F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task = tokio::spawn(watch(Invalidator(Arc::downgrade(&self.inner))));
        self.track(task.abort_handle());
    }

    fn track(&self, handle: tokio::task::AbortHandle) {
        let mut background = match self.inner.background.lock() {
            Ok(background) => background,
            Err(poisoned) => poisoned.into_inner(),
        };
        background.retain(|handle| !handle.is_finished());
        background.push(handle);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.inner.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn pages_read(&self) -> Vec<crate::tools::PageRead> {
        let state = self.lock();
        let mut read: Vec<crate::tools::PageRead> = state
            .read
            .iter()
            .filter_map(|path| state.pages.get(path))
            .map(|page| crate::tools::PageRead {
                path: page.path.clone(),
                version: page.version.clone(),
            })
            .collect();
        read.sort_by(|a, b| a.path.cmp(&b.path));
        read
    }

    pub fn is_stale(&self, path: &str) -> bool {
        self.lock().stale.contains(path)
    }

    /// The page's content, fetched first when it is not loaded or is stale.
    /// Waits only for this page, never for the rest of the workspace.
    pub async fn read(&self, path: &str) -> Result<Option<String>, WorkspaceError> {
        self.listed().await?;
        self.lock().list_from_template(path);
        loop {
            let next = {
                let mut state = self.lock();
                if let Some((notify, claim)) = state.in_flight.get(path) {
                    let mut waiting = Box::pin(Arc::clone(notify).notified_owned());
                    waiting.as_mut().enable();
                    Next::Wait(waiting, *claim)
                } else if state.needs_fetch(path) {
                    Next::Fetch(Self::claim(&mut state, std::iter::once(path.to_owned())))
                } else {
                    if state.knows(path) {
                        state.read.insert(path.to_owned());
                    }
                    Next::Done(state.pages.get(path).map(|page| page.content.clone()))
                }
            };
            match next {
                Next::Done(content) => return Ok(content),
                Next::Wait(waiting, claim) => {
                    waiting.await;
                    if let Some(error) = self.failure_of(path, claim) {
                        return Err(WorkspaceError::Refresh(error));
                    }
                }
                Next::Fetch(claimed) => self.fetch_claimed(&claimed).await?,
            }
        }
    }

    /// Marks every page whose key starts with one of `prefixes` stale,
    /// listed pages not read yet and reads in flight included.
    pub fn invalidate(&self, prefixes: &[PageKey]) -> Vec<String> {
        let mut state = self.lock();
        let under_prefix =
            |key: &PageKey| prefixes.iter().any(|prefix| key_starts_with(key, prefix));
        let mut matched: Vec<String> = state
            .pages
            .values()
            .filter(|page| under_prefix(&page.key))
            .map(|page| page.path.clone())
            .chain(
                state
                    .index
                    .values()
                    .filter(|listed| under_prefix(&listed.key))
                    .map(|listed| listed.path.clone()),
            )
            .collect();
        matched.sort();
        matched.dedup();
        for path in &matched {
            state.touch(path);
            state.stale.insert(path.clone());
        }
        matched.retain(|path| state.pages.contains_key(path) || state.in_flight.contains_key(path));
        matched
    }
}
