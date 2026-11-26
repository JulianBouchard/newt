#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newt_core::workspace::{
    Listed, Listing, ListingFuture, Page, PageFuture, PageKey, PageSource, Workspace,
};

#[derive(Default)]
struct Pages {
    priorities: BTreeMap<&'static str, Option<f32>>,
    read: Mutex<Vec<String>>,
    reads: AtomicUsize,
}

struct Source(Arc<Pages>);

impl PageSource for Source {
    fn list(&self) -> ListingFuture<'_> {
        let pages = Arc::clone(&self.0);
        Box::pin(async move {
            Ok(Listing {
                pages: pages
                    .priorities
                    .iter()
                    .map(|(path, priority)| {
                        let mut listed = Listed::new(vec![(*path).to_owned()], (*path).to_owned());
                        listed.priority = *priority;
                        listed
                    })
                    .collect(),
                templates: Vec::new(),
            })
        })
    }

    fn read(&self, keys: Vec<PageKey>) -> PageFuture<'_> {
        let pages = Arc::clone(&self.0);
        Box::pin(async move {
            pages.reads.fetch_add(1, Ordering::SeqCst);
            Ok(keys
                .into_iter()
                .map(|key| {
                    let path = key.join("/");
                    pages.read.lock().unwrap().push(path.clone());
                    Page {
                        content: format!("{path} content"),
                        version: "1".to_owned(),
                        key,
                        path,
                    }
                })
                .collect())
        })
    }
}

fn workspace() -> (Arc<Pages>, Workspace) {
    let pages = Arc::new(Pages {
        priorities: BTreeMap::from([
            ("profile.md", Some(1.0)),
            ("notes.md", Some(0.5)),
            ("archive.md", None),
        ]),
        ..Pages::default()
    });
    let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&pages))));
    (pages, workspace)
}

async fn until_read(pages: &Pages, count: usize) {
    for _ in 0..400 {
        if pages.read.lock().unwrap().len() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        pages.read.lock().unwrap().len() >= count,
        "only {:?} were read",
        pages.read.lock().unwrap()
    );
}

#[tokio::test]
async fn pages_at_or_above_the_priority_are_read_ahead_and_not_counted_as_read() {
    let (pages, workspace) = workspace();
    workspace.preload(1.0);
    until_read(&pages, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(*pages.read.lock().unwrap(), vec!["profile.md".to_owned()]);
    assert_eq!(workspace.pages_read(), Vec::new());
    assert_eq!(
        workspace.read("profile.md").await.unwrap().as_deref(),
        Some("profile.md content")
    );
    assert_eq!(
        pages.reads.load(Ordering::SeqCst),
        1,
        "the read found it loaded"
    );
}

#[tokio::test]
async fn a_lower_threshold_preloads_more_and_pages_without_a_priority_never() {
    let (pages, workspace) = workspace();
    workspace.preload(0.5);
    until_read(&pages, 2).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut read = pages.read.lock().unwrap().clone();
    read.sort();
    assert_eq!(read, vec!["notes.md".to_owned(), "profile.md".to_owned()]);
}

#[tokio::test]
async fn linked_pages_are_read_ahead_and_unknown_keys_are_ignored() {
    let (pages, workspace) = workspace();
    workspace.prefetch(vec![
        vec!["archive.md".to_owned()],
        vec!["missing.md".to_owned()],
    ]);
    until_read(&pages, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(*pages.read.lock().unwrap(), vec!["archive.md".to_owned()]);
    assert_eq!(workspace.pages_read(), Vec::new());
}
