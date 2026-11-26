#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newt_core::workspace::{
    Listed, Listing, ListingFuture, Page, PageFuture, PageKey, PageSource, Template, Workspace,
};

#[derive(Default)]
struct Gated {
    pages: Mutex<BTreeMap<String, String>>,
    templates: Mutex<Vec<Template>>,
    lists: AtomicUsize,
    reads: AtomicUsize,
    fail_list: AtomicBool,
    fail_reads: AtomicBool,
    hold_lists: AtomicBool,
    hold_reads: AtomicBool,
    gate: tokio::sync::Notify,
}

impl Gated {
    fn set(&self, path: &str, content: &str) {
        self.pages
            .lock()
            .unwrap()
            .insert(path.to_owned(), content.to_owned());
    }

    async fn hold(&self, held: &AtomicBool) {
        if held.load(Ordering::SeqCst) {
            self.gate.notified().await;
        }
    }
}

struct Source(Arc<Gated>);

impl PageSource for Source {
    fn list(&self) -> ListingFuture<'_> {
        let gated = Arc::clone(&self.0);
        Box::pin(async move {
            gated.lists.fetch_add(1, Ordering::SeqCst);
            let snapshot: Vec<String> = gated.pages.lock().unwrap().keys().cloned().collect();
            gated.hold(&gated.hold_lists).await;
            if gated.fail_list.load(Ordering::SeqCst) {
                return Err("listing failed".to_owned());
            }
            Ok(Listing {
                pages: snapshot
                    .into_iter()
                    .map(|path| Listed::new(vec![path.clone()], path))
                    .collect(),
                templates: gated.templates.lock().unwrap().clone(),
            })
        })
    }

    fn read(&self, keys: Vec<PageKey>) -> PageFuture<'_> {
        let gated = Arc::clone(&self.0);
        Box::pin(async move {
            gated.reads.fetch_add(1, Ordering::SeqCst);
            let snapshot = gated.pages.lock().unwrap().clone();
            gated.hold(&gated.hold_reads).await;
            if gated.fail_reads.load(Ordering::SeqCst) {
                return Err("the route timed out".to_owned());
            }
            Ok(snapshot
                .into_iter()
                .filter(|(path, _)| keys.contains(&vec![path.clone()]))
                .map(|(path, content)| Page {
                    key: vec![path.clone()],
                    path,
                    version: content.len().to_string(),
                    content,
                })
                .collect())
        })
    }
}

async fn until(count: &AtomicUsize, wanted: usize) {
    for _ in 0..400 {
        if count.load(Ordering::SeqCst) >= wanted {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        count.load(Ordering::SeqCst) >= wanted,
        "never reached {wanted}"
    );
}

#[tokio::test]
async fn an_invalidation_during_a_pages_first_read_is_not_lost() {
    let gated = Arc::new(Gated::default());
    gated.set("profile.md", "old");
    let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&gated))));
    workspace.glob("**").await.unwrap();
    gated.hold_reads.store(true, Ordering::SeqCst);
    let first = {
        let workspace = workspace.clone();
        tokio::spawn(async move { workspace.read("profile.md").await })
    };
    until(&gated.reads, 1).await;
    gated.set("profile.md", "new");
    workspace.invalidate(&[vec!["profile.md".to_owned()]]);
    gated.hold_reads.store(false, Ordering::SeqCst);
    gated.gate.notify_one();
    first.await.unwrap().unwrap();
    assert_eq!(
        workspace.read("profile.md").await.unwrap().as_deref(),
        Some("new"),
        "the invalidation made the next read fetch again"
    );
}

#[tokio::test]
async fn a_relist_asked_for_during_a_listing_lists_again() {
    let gated = Arc::new(Gated::default());
    gated.set("a.md", "a");
    gated.hold_lists.store(true, Ordering::SeqCst);
    let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&gated))));
    until(&gated.lists, 1).await;
    gated.set("b.md", "b");
    workspace.relist();
    gated.hold_lists.store(false, Ordering::SeqCst);
    gated.gate.notify_one();
    assert_eq!(workspace.glob("**").await.unwrap(), vec!["a.md", "b.md"]);
    assert_eq!(gated.lists.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn readers_waiting_on_a_failed_listing_all_get_its_error_without_listing_again() {
    let gated = Arc::new(Gated::default());
    gated.set("a.md", "a");
    gated.fail_list.store(true, Ordering::SeqCst);
    gated.hold_lists.store(true, Ordering::SeqCst);
    let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&gated))));
    until(&gated.lists, 1).await;
    let reads: Vec<_> = (0..5)
        .map(|_| {
            let workspace = workspace.clone();
            tokio::spawn(async move { workspace.read("a.md").await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(20)).await;
    gated.hold_lists.store(false, Ordering::SeqCst);
    gated.gate.notify_one();
    for read in reads {
        let read = tokio::time::timeout(Duration::from_secs(5), read).await;
        assert!(read.unwrap().unwrap().is_err());
    }
    assert_eq!(
        gated.lists.load(Ordering::SeqCst),
        1,
        "one failed listing, one call"
    );
}

#[tokio::test]
async fn readers_waiting_on_a_failed_page_read_all_get_its_error_without_reading_again() {
    let gated = Arc::new(Gated::default());
    gated.set("a.md", "a");
    let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&gated))));
    workspace.glob("**").await.unwrap();
    gated.fail_reads.store(true, Ordering::SeqCst);
    gated.hold_reads.store(true, Ordering::SeqCst);
    let reads: Vec<_> = (0..5)
        .map(|_| {
            let workspace = workspace.clone();
            tokio::spawn(async move { workspace.read("a.md").await })
        })
        .collect();
    until(&gated.reads, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    gated.hold_reads.store(false, Ordering::SeqCst);
    gated.gate.notify_one();
    for read in reads {
        let read = tokio::time::timeout(Duration::from_secs(5), read).await;
        assert!(read.unwrap().unwrap().is_err());
    }
    assert_eq!(
        gated.reads.load(Ordering::SeqCst),
        1,
        "one failed read, one call"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_template_path_its_source_does_not_return_reads_as_missing_once() {
    let gated = Arc::new(Gated::default());
    gated.templates.lock().unwrap().push(Template {
        path: "orders/{id}.md".to_owned(),
        description: None,
        key: vec!["orders".to_owned()],
    });
    let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&gated))));
    let reading = tokio::spawn(async move { workspace.read("orders/1.md").await });
    let read = tokio::time::timeout(Duration::from_secs(5), reading).await;
    assert_eq!(read.unwrap().unwrap().unwrap(), None);
    assert_eq!(gated.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_template_path_that_fails_to_read_does_not_stay_listed() {
    let gated = Arc::new(Gated::default());
    gated.set("a.md", "hello");
    gated.templates.lock().unwrap().push(Template {
        path: "orders/{id}.md".to_owned(),
        description: None,
        key: vec!["orders".to_owned()],
    });
    let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&gated))));
    workspace.glob("**").await.unwrap();
    gated.fail_reads.store(true, Ordering::SeqCst);
    assert!(workspace.read("orders/999.md").await.is_err());
    gated.fail_reads.store(false, Ordering::SeqCst);
    assert_eq!(workspace.glob("**").await.unwrap(), vec!["a.md"]);
    assert_eq!(workspace.grep("hello", None, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_relist_leaves_loaded_fresh_pages_readable() {
    let gated = Arc::new(Gated::default());
    gated.set("a.md", "hello");
    gated.set("b.md", "there");
    let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&gated))));
    workspace.read("a.md").await.unwrap();
    gated.fail_list.store(true, Ordering::SeqCst);
    workspace.relist();
    assert_eq!(
        workspace.read("a.md").await.unwrap().as_deref(),
        Some("hello")
    );
    assert!(
        workspace.read("b.md").await.is_err(),
        "an unloaded page reports the listing"
    );
}
