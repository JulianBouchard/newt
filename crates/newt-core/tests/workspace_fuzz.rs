#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::join_all;
use newt_core::workspace::{
    Listed, Listing, ListingFuture, Page, PageFuture, PageKey, PageSource, Workspace,
};
use proptest::prelude::*;

const BOUND: Duration = Duration::from_secs(10);

#[derive(Default)]
struct World {
    pages: Mutex<BTreeMap<String, String>>,
    history: Mutex<BTreeMap<String, HashSet<String>>>,
    reads: AtomicUsize,
    read_paths: Mutex<Vec<String>>,
    fail_list: AtomicBool,
    failing: Mutex<HashSet<String>>,
}

impl World {
    fn write(&self, path: &str, content: &str) {
        self.pages
            .lock()
            .unwrap()
            .insert(path.to_owned(), content.to_owned());
        self.history
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default()
            .insert(content.to_owned());
    }

    fn check_read(&self, path: &str, read: Option<&String>) {
        if let Some(content) = read {
            assert!(
                self.history
                    .lock()
                    .unwrap()
                    .get(path)
                    .is_some_and(|seen| seen.contains(content)),
                "{path} read {content:?}, which it never held"
            );
        }
    }
}

fn key_of(path: &str) -> PageKey {
    path.split('/').map(str::to_owned).collect()
}

struct Source(Arc<World>);

impl PageSource for Source {
    fn list(&self) -> ListingFuture<'_> {
        let world = Arc::clone(&self.0);
        Box::pin(async move {
            tokio::task::yield_now().await;
            if world.fail_list.load(Ordering::SeqCst) {
                return Err("listing failed".to_owned());
            }
            let pages = world
                .pages
                .lock()
                .unwrap()
                .keys()
                .map(|path| Listed::new(key_of(path), path.clone()))
                .collect();
            Ok(Listing {
                pages,
                templates: Vec::new(),
            })
        })
    }

    fn read(&self, keys: Vec<PageKey>) -> PageFuture<'_> {
        let world = Arc::clone(&self.0);
        Box::pin(async move {
            world.reads.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let pages = world.pages.lock().unwrap().clone();
            let wanted: Vec<(String, String)> = pages
                .into_iter()
                .filter(|(path, _)| keys.contains(&key_of(path)))
                .collect();
            world
                .read_paths
                .lock()
                .unwrap()
                .extend(wanted.iter().map(|(path, _)| path.clone()));
            if wanted
                .iter()
                .any(|(path, _)| world.failing.lock().unwrap().contains(path))
            {
                return Err("route failed".to_owned());
            }
            Ok(wanted
                .into_iter()
                .map(|(path, content)| Page {
                    key: key_of(&path),
                    version: content.len().to_string(),
                    path,
                    content,
                })
                .collect())
        })
    }
}

#[derive(Debug, Clone)]
enum Op {
    Read(usize),
    ReadMany(Vec<usize>),
    Glob(&'static str),
    Grep(&'static str),
    Invalidate(usize),
    InvalidateAll,
    Put(usize, String),
    Relist,
    Edit(usize, String),
    Remove(usize),
    Add(String),
    FailList(bool),
    FailPage(usize, bool),
}

const GLOBS: [&str; 5] = ["**", "*.md", "roles/*", "deep/**/*.md", "nothing/*"];

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => any::<usize>().prop_map(Op::Read),
        2 => proptest::collection::vec(any::<usize>(), 1..6).prop_map(Op::ReadMany),
        2 => proptest::sample::select(GLOBS.to_vec()).prop_map(Op::Glob),
        2 => proptest::sample::select(GLOBS.to_vec()).prop_map(Op::Grep),
        2 => any::<usize>().prop_map(Op::Invalidate),
        1 => Just(Op::InvalidateAll),
        1 => (any::<usize>(), "[a-z ]{0,12}").prop_map(|(index, text)| Op::Put(index, text)),
        2 => Just(Op::Relist),
        3 => (any::<usize>(), "[a-z ]{0,12}").prop_map(|(index, text)| Op::Edit(index, text)),
        1 => any::<usize>().prop_map(Op::Remove),
        1 => "(roles|deep/a/b|notes)/[a-z]{1,4}\\.md".prop_map(Op::Add),
        1 => any::<bool>().prop_map(Op::FailList),
        1 => (any::<usize>(), any::<bool>()).prop_map(|(index, fail)| Op::FailPage(index, fail)),
    ]
}

fn paths(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| match index % 4 {
            0 => format!("p{index}.md"),
            1 => format!("roles/r{index}.md"),
            2 => format!("deep/a/b/d{index}.md"),
            _ => format!("notes/n{index}.txt"),
        })
        .collect()
}

fn pick(world: &World, index: usize) -> Option<String> {
    let pages = world.pages.lock().unwrap();
    (!pages.is_empty()).then(|| pages.keys().nth(index % pages.len()).cloned())?
}

async fn apply(workspace: &Workspace, world: &World, op: Op) {
    match op {
        Op::Read(index) => {
            if let Some(path) = pick(world, index)
                && let Ok(read) = workspace.read(&path).await
            {
                world.check_read(&path, read.as_ref());
            }
        }
        Op::ReadMany(indexes) => {
            let wanted: Vec<String> = indexes
                .into_iter()
                .filter_map(|index| pick(world, index))
                .collect();
            let reads = join_all(wanted.iter().map(|path| workspace.read(path))).await;
            for (path, read) in wanted.iter().zip(reads) {
                if let Ok(read) = read {
                    world.check_read(path, read.as_ref());
                }
            }
        }
        Op::Glob(pattern) => {
            let _found = workspace.glob(pattern).await;
        }
        Op::Grep(within) => {
            if let Ok(found) = workspace.grep("a", Some(within), 1000).await {
                for line in found {
                    assert!(
                        world
                            .history
                            .lock()
                            .unwrap()
                            .get(&line.path)
                            .is_some_and(|seen| seen
                                .iter()
                                .any(|content| content.lines().any(|text| text == line.text))),
                        "grep found {line:?}, a line {} never held",
                        line.path
                    );
                }
            }
        }
        Op::Invalidate(index) => {
            if let Some(path) = pick(world, index) {
                workspace.invalidate(&[key_of(&path)]);
            }
        }
        Op::InvalidateAll => {
            workspace.invalidate(&[Vec::new()]);
        }
        Op::Put(index, text) => {
            if let Some(path) = pick(world, index) {
                world.write(&path, &text);
                workspace.put(vec![Page {
                    key: key_of(&path),
                    version: text.len().to_string(),
                    path,
                    content: text,
                }]);
            }
        }
        Op::Relist => workspace.relist(),
        Op::Edit(index, text) => {
            if let Some(path) = pick(world, index) {
                world.write(&path, &text);
            }
        }
        Op::Remove(index) => {
            if let Some(path) = pick(world, index) {
                world.pages.lock().unwrap().remove(&path);
            }
        }
        Op::Add(path) => {
            world.write(&path, "new page");
        }
        Op::FailList(fail) => world.fail_list.store(fail, Ordering::SeqCst),
        Op::FailPage(index, fail) => {
            if let Some(path) = pick(world, index) {
                let mut failing = world.failing.lock().unwrap();
                if fail {
                    failing.insert(path);
                } else {
                    failing.remove(&path);
                }
            }
        }
    }
}

async fn settle_and_check(workspace: &Workspace, world: &World) {
    world.fail_list.store(false, Ordering::SeqCst);
    world.failing.lock().unwrap().clear();
    workspace.relist();
    workspace.invalidate(&[Vec::new()]);
    let truth = world.pages.lock().unwrap().clone();
    let mut listed = workspace.glob("**").await.unwrap();
    listed.sort();
    let expected: Vec<String> = truth.keys().cloned().collect();
    assert_eq!(
        listed, expected,
        "the index matches the source after a relist"
    );
    for (path, content) in &truth {
        assert_eq!(
            workspace.read(path).await.unwrap().as_deref(),
            Some(content.as_str()),
            "{path} reads the source's content after a refresh"
        );
    }
}

fn run_case(count: usize, batches: Vec<Vec<Op>>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let world = Arc::new(World::default());
        for path in paths(count) {
            world.write(&path, &format!("content of {path}"));
        }
        let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&world))));
        let done = tokio::time::timeout(BOUND, async {
            for batch in batches {
                join_all(batch.into_iter().map(|op| apply(&workspace, &world, op))).await;
            }
            settle_and_check(&workspace, &world).await;
        })
        .await;
        assert!(done.is_ok(), "the workspace hung");
    });
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn concurrent_reads_edits_and_failures_settle_to_the_source(
        count in prop_oneof![4 => 0usize..12, 1 => 200usize..600],
        batches in proptest::collection::vec(proptest::collection::vec(op(), 1..5), 1..12),
    ) {
        run_case(count, batches);
    }
}

fn glob_and_grep_case(count: usize, within: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let world = Arc::new(World::default());
        *world.pages.lock().unwrap() = paths(count)
            .into_iter()
            .map(|path| (path, "a line".to_owned()))
            .collect();
        let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&world))));
        let matched = workspace.glob(within).await.unwrap();
        assert_eq!(world.reads.load(Ordering::SeqCst), 0, "glob read a page");
        workspace
            .grep("line", Some(within), usize::MAX)
            .await
            .unwrap();
        let mut read = world.read_paths.lock().unwrap().clone();
        read.sort();
        assert_eq!(
            read, matched,
            "grep read exactly the pages its glob matched"
        );
    });
}

proptest! {
    #[test]
    fn glob_reads_nothing_and_grep_reads_only_what_its_glob_matches(
        count in 0usize..300,
        within in prop_oneof![
            proptest::sample::select(GLOBS.to_vec()).prop_map(str::to_owned),
            "(roles|deep|notes|\\*\\*|\\*)(/[a-z*]{0,3}){0,3}(\\.md|\\.txt)?",
        ],
    ) {
        glob_and_grep_case(count, &within);
    }
}

#[test]
fn a_workspace_of_twenty_thousand_pages_lists_fast_and_reads_only_what_it_is_asked() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let world = Arc::new(World::default());
        *world.pages.lock().unwrap() = paths(20_000)
            .into_iter()
            .map(|path| (path, "x".to_owned()))
            .collect();
        let workspace = Workspace::loading(Arc::new(Source(Arc::clone(&world))));
        let started = std::time::Instant::now();
        assert_eq!(workspace.glob("**").await.unwrap().len(), 20_000);
        assert_eq!(workspace.glob("roles/*").await.unwrap().len(), 5_000);
        assert_eq!(
            workspace.read("roles/r1.md").await.unwrap().as_deref(),
            Some("x")
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(world.reads.load(Ordering::SeqCst), 1);
        let found = workspace
            .grep("x", Some("deep/a/b/d2*.md"), 10_000)
            .await
            .unwrap();
        assert_eq!(found.len(), world.read_paths.lock().unwrap().len() - 1);
        assert!(
            world.read_paths.lock().unwrap().len() < 2_000,
            "grep read only what its glob matched"
        );
    });
}
