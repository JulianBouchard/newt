use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

mod refresh;

use refresh::Claimed;

pub type PageKey = Vec<String>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub key: PageKey,
    pub path: String,
    pub version: String,
    pub content: String,
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

pub trait PageSource: Send + Sync {
    fn fetch(&self, keys: Vec<PageKey>) -> PageFuture<'_>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Default)]
struct State {
    pages: BTreeMap<String, Page>,
    stale: HashSet<String>,
    in_flight: HashMap<String, Arc<Notify>>,
    read: HashSet<String>,
    generation: HashMap<String, u64>,
    clock: u64,
    loading: bool,
    invalidated_while_loading: Vec<PageKey>,
}

impl State {
    fn touch(&mut self, path: &str) {
        self.clock += 1;
        self.generation.insert(path.to_owned(), self.clock);
    }

    fn generation_of(&self, path: &str) -> u64 {
        self.generation.get(path).copied().unwrap_or(0)
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
    initial: tokio::sync::OnceCell<Result<(), String>>,
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
        formatter
            .debug_struct("Workspace")
            .field("pages", &self.lock().pages.len())
            .finish_non_exhaustive()
    }
}

pub fn key_starts_with(key: &[String], prefix: &[String]) -> bool {
    key.len() >= prefix.len() && key.iter().zip(prefix).all(|(a, b)| a == b)
}

enum Next {
    Done(Option<String>),
    Wait(Pin<Box<tokio::sync::futures::OwnedNotified>>),
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
                initial: tokio::sync::OnceCell::new(),
            }),
        }
    }

    pub fn loading(source: Arc<dyn PageSource>) -> Self {
        let workspace = Self::from_pages(Vec::new(), Some(source));
        workspace.lock().loading = true;
        let weak = Arc::downgrade(&workspace.inner);
        let task = tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if let Err(error) = (Self { inner }).loaded().await {
                tracing::warn!(%error, "workspace pages did not load; reads will report it");
            }
        });
        workspace.track(task.abort_handle());
        workspace
    }

    fn track(&self, handle: tokio::task::AbortHandle) {
        let mut background = match self.inner.background.lock() {
            Ok(background) => background,
            Err(poisoned) => poisoned.into_inner(),
        };
        background.retain(|handle| !handle.is_finished());
        background.push(handle);
    }

    async fn loaded(&self) -> Result<(), WorkspaceError> {
        if !self.lock().loading {
            return Ok(());
        }
        self.inner
            .initial
            .get_or_init(|| self.load_all())
            .await
            .clone()
            .map_err(WorkspaceError::Refresh)
    }

    async fn load_all(&self) -> Result<(), String> {
        let source = self
            .inner
            .source
            .clone()
            .ok_or_else(|| "this workspace has no page source".to_owned())?;
        let pages = source.fetch(vec![Vec::new()]).await?;
        let pending = {
            let mut state = self.lock();
            for page in pages {
                if !state.pages.contains_key(&page.path) {
                    state.pages.insert(page.path.clone(), page);
                }
            }
            state.loading = false;
            std::mem::take(&mut state.invalidated_while_loading)
        };
        self.invalidate(&pending);
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.inner.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn paths(&self) -> Vec<String> {
        self.lock().pages.keys().cloned().collect()
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

    pub async fn read(&self, path: &str) -> Result<Option<String>, WorkspaceError> {
        self.loaded().await?;
        loop {
            let next = {
                let mut state = self.lock();
                if let Some(notify) = state.in_flight.get(path) {
                    let mut waiting = Box::pin(Arc::clone(notify).notified_owned());
                    waiting.as_mut().enable();
                    Next::Wait(waiting)
                } else if state.stale.contains(path) {
                    Next::Fetch(Self::claim(&mut state, std::iter::once(path.to_owned())))
                } else {
                    state.read.insert(path.to_owned());
                    Next::Done(state.pages.get(path).map(|page| page.content.clone()))
                }
            };
            match next {
                Next::Done(content) => return Ok(content),
                Next::Wait(waiting) => waiting.await,
                Next::Fetch(claimed) => self.fetch_claimed(&claimed).await?,
            }
        }
    }

    pub async fn glob(&self, pattern: &str) -> Result<Vec<String>, WorkspaceError> {
        let matcher = matcher(pattern)?;
        self.loaded().await?;
        Ok(self
            .lock()
            .pages
            .keys()
            .filter(|path| matcher.is_match(path.as_str()))
            .cloned()
            .collect())
    }

    pub async fn grep(
        &self,
        pattern: &str,
        within: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GrepMatch>, WorkspaceError> {
        let regex =
            Regex::new(pattern).map_err(|error| WorkspaceError::Regex(error.to_string()))?;
        let within = within.map(matcher).transpose()?;
        self.loaded().await?;
        let outdated: Vec<String> = self.lock().stale.iter().cloned().collect();
        self.refresh(outdated).await?;
        let state = self.lock();
        Ok(state
            .pages
            .iter()
            .filter(|(path, _)| {
                within
                    .as_ref()
                    .is_none_or(|glob| glob.is_match(path.as_str()))
            })
            .flat_map(|(path, page)| {
                page.content
                    .lines()
                    .enumerate()
                    .filter(|(_, text)| regex.is_match(text))
                    .map(|(index, text)| GrepMatch {
                        path: path.clone(),
                        line: index + 1,
                        text: text.to_owned(),
                    })
                    .collect::<Vec<_>>()
            })
            .take(limit)
            .collect())
    }

    pub fn put(&self, pages: Vec<Page>) -> Vec<String> {
        let mut state = self.lock();
        pages
            .into_iter()
            .map(|page| {
                let path = page.path.clone();
                state.touch(&path);
                state.stale.remove(&path);
                state.pages.insert(path.clone(), page);
                path
            })
            .collect()
    }

    pub fn invalidate(&self, prefixes: &[PageKey]) -> Vec<String> {
        let mut state = self.lock();
        let matched: Vec<String> = state
            .pages
            .values()
            .filter(|page| {
                prefixes
                    .iter()
                    .any(|prefix| key_starts_with(&page.key, prefix))
            })
            .map(|page| page.path.clone())
            .collect();
        for path in &matched {
            state.touch(path);
            state.stale.insert(path.clone());
        }
        if state.loading {
            state
                .invalidated_while_loading
                .extend(prefixes.iter().cloned());
        }
        matched
    }
}

fn matcher(pattern: &str) -> Result<GlobMatcher, WorkspaceError> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| WorkspaceError::Glob(error.to_string()))?
        .compile_matcher())
}
