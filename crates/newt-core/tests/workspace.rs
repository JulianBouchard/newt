#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests fail loudly on setup errors"
)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newt_core::journal::{JournalEntry, NoJournal};
use newt_core::message::{Message, Part, Role, ToolCall};
use newt_core::prompt::{Lane, ModelRequest, ToolSpec};
use newt_core::provider::{
    CallTiming, Model, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};
use newt_core::sink::NoSink;
use newt_core::steering::NoSteering;
use newt_core::tools::{CallContext, Retention, ToolDef, ToolHost, ToolOutput};
use newt_core::turn::{TurnDeps, TurnInput, TurnSettings, run_turn};
use newt_core::workspace::{Page, PageFuture, PageKey, PageSource, Workspace};
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

fn key(parts: &[&str]) -> PageKey {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

#[derive(Default)]
struct Backing {
    pages: Mutex<BTreeMap<String, (PageKey, String, u32)>>,
    fetches: AtomicUsize,
    fail: AtomicBool,
    gate: Option<Arc<Semaphore>>,
}

impl Backing {
    fn gated() -> Self {
        Self {
            gate: Some(Arc::new(Semaphore::new(0))),
            ..Self::default()
        }
    }

    fn insert(&self, path: &str, page_key: PageKey, content: &str) {
        self.pages
            .lock()
            .unwrap()
            .insert(path.to_owned(), (page_key, content.to_owned(), 1));
    }

    fn write(&self, path: &str, content: &str) {
        let mut pages = self.pages.lock().unwrap();
        let entry = pages.get_mut(path).unwrap();
        content.clone_into(&mut entry.1);
        entry.2 += 1;
    }

    fn page(&self, path: &str) -> Page {
        let pages = self.pages.lock().unwrap();
        let (page_key, content, version) = pages.get(path).unwrap().clone();
        Page {
            key: page_key,
            path: path.to_owned(),
            version: version.to_string(),
            content,
        }
    }

    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }

    fn release(&self) {
        if let Some(gate) = &self.gate {
            gate.add_permits(1);
        }
    }
}

struct Source(Arc<Backing>);

impl PageSource for Source {
    fn fetch(&self, keys: Vec<PageKey>) -> PageFuture<'_> {
        let backing = Arc::clone(&self.0);
        Box::pin(async move {
            backing.fetches.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &backing.gate {
                gate.acquire().await.unwrap().forget();
            }
            if backing.fail.load(Ordering::SeqCst) {
                return Err("context provider unavailable".to_owned());
            }
            let paths: Vec<String> = backing
                .pages
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, (page_key, _, _))| {
                    keys.iter().any(|key| key.is_empty() || key == page_key)
                })
                .map(|(path, _)| path.clone())
                .collect();
            Ok(paths.iter().map(|path| backing.page(path)).collect())
        })
    }
}

fn workspace(backing: &Arc<Backing>) -> Workspace {
    let pages = ["profile.md", "roles/acme.md"]
        .iter()
        .map(|path| backing.page(path))
        .collect();
    Workspace::with_source(pages, Arc::new(Source(Arc::clone(backing))))
}

fn seeded(backing: Backing) -> Arc<Backing> {
    backing.insert("profile.md", key(&["customer", "profile"]), "Remote only");
    backing.insert("roles/acme.md", key(&["roles", "acme"]), "Acme: backend");
    Arc::new(backing)
}

async fn until_fetches(backing: &Backing, count: usize) {
    for _ in 0..200 {
        if backing.fetches() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("expected {count} fetches, saw {}", backing.fetches());
}

#[tokio::test]
async fn an_invalidated_page_is_fetched_only_when_read() {
    let backing = seeded(Backing::default());
    let workspace = workspace(&backing);
    backing.write("profile.md", "Remote or Berlin");

    assert_eq!(
        workspace.invalidate(&[key(&["customer"])]),
        vec!["profile.md"]
    );
    assert_eq!(backing.fetches(), 0, "invalidation alone must not fetch");

    let content = workspace.read("profile.md").await.unwrap();
    assert_eq!(content.as_deref(), Some("Remote or Berlin"));
    assert_eq!(backing.fetches(), 1);
    workspace.read("profile.md").await.unwrap();
    assert_eq!(backing.fetches(), 1, "a fresh page is served from memory");
}

#[tokio::test]
async fn a_read_waits_for_the_refetch_already_in_flight() {
    let backing = seeded(Backing::gated());
    let workspace = workspace(&backing);
    workspace.read("profile.md").await.unwrap();
    backing.write("profile.md", "Remote or Berlin");
    workspace.invalidate(&[key(&["customer", "profile"])]);
    workspace.refresh_read_pages_in_background();
    until_fetches(&backing, 1).await;

    let reader = tokio::spawn({
        let workspace = workspace.clone();
        async move { workspace.read("profile.md").await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!reader.is_finished(), "the read must wait for the refetch");
    backing.release();

    let content = reader.await.unwrap().unwrap();
    assert_eq!(content.as_deref(), Some("Remote or Berlin"));
    assert_eq!(
        backing.fetches(),
        1,
        "the waiting read must reuse the refetch"
    );
}

#[tokio::test]
async fn an_invalidation_during_a_refetch_triggers_another_fetch() {
    let backing = seeded(Backing::gated());
    let workspace = workspace(&backing);
    workspace.invalidate(&[key(&["customer"])]);
    let refresh = tokio::spawn({
        let workspace = workspace.clone();
        async move { workspace.refresh(vec!["profile.md".to_owned()]).await }
    });
    until_fetches(&backing, 1).await;
    backing.write("profile.md", "Remote or Berlin");
    workspace.invalidate(&[key(&["customer"])]);
    backing.release();
    until_fetches(&backing, 2).await;
    assert!(
        workspace.is_stale("profile.md"),
        "the first result predates the invalidation"
    );
    backing.release();
    refresh.await.unwrap().unwrap();

    assert!(!workspace.is_stale("profile.md"));
    let content = workspace.read("profile.md").await.unwrap();
    assert_eq!(content.as_deref(), Some("Remote or Berlin"));
    assert_eq!(backing.fetches(), 2);
}

#[tokio::test]
async fn data_put_during_a_refetch_is_not_overwritten_by_it() {
    let backing = seeded(Backing::gated());
    let workspace = workspace(&backing);
    workspace.invalidate(&[key(&["customer"])]);
    let refresh = tokio::spawn({
        let workspace = workspace.clone();
        async move { workspace.refresh(vec!["profile.md".to_owned()]).await }
    });
    until_fetches(&backing, 1).await;
    backing.write("profile.md", "Remote or Berlin");
    workspace.put(vec![backing.page("profile.md")]);
    backing.write("profile.md", "stale snapshot the fetch must not apply");
    backing.release();
    refresh.await.unwrap().unwrap();

    let content = workspace.read("profile.md").await.unwrap();
    assert_eq!(content.as_deref(), Some("Remote or Berlin"));
}

#[tokio::test]
async fn a_failed_refetch_errors_and_the_next_read_retries() {
    let backing = seeded(Backing::default());
    let workspace = workspace(&backing);
    workspace.invalidate(&[key(&["customer"])]);
    backing.fail.store(true, Ordering::SeqCst);
    assert!(workspace.read("profile.md").await.is_err());
    assert!(workspace.is_stale("profile.md"));

    backing.fail.store(false, Ordering::SeqCst);
    assert!(workspace.read("profile.md").await.unwrap().is_some());
    assert_eq!(backing.fetches(), 2);
}

#[tokio::test]
async fn a_page_the_provider_no_longer_returns_is_removed() {
    let backing = seeded(Backing::default());
    let workspace = workspace(&backing);
    backing.pages.lock().unwrap().remove("roles/acme.md");
    workspace.invalidate(&[key(&["roles"])]);
    assert_eq!(workspace.read("roles/acme.md").await.unwrap(), None);
    assert_eq!(workspace.paths(), vec!["profile.md"]);
}

#[tokio::test]
async fn grep_refreshes_stale_pages_before_searching() {
    let backing = seeded(Backing::default());
    let workspace = workspace(&backing);
    backing.write("roles/acme.md", "Acme: staff engineer");
    workspace.invalidate(&[key(&["roles"])]);
    let found = workspace.grep("staff", None, 10).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, "roles/acme.md");
}

struct Scripted(Mutex<VecDeque<Message>>);

impl Model for Scripted {
    fn stream(
        &self,
        _request: &ModelRequest<'_>,
        _cancel: &CancellationToken,
        _on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send {
        let next = self.0.lock().unwrap().pop_front();
        std::future::ready(
            next.map(|message| ModelResponse {
                message,
                usage: Usage::default(),
                stop: StopReason::ToolUse,
                timing: CallTiming::default(),
                response_id: None,
                request_ids: BTreeMap::new(),
            })
            .ok_or_else(|| ProviderError::Protocol("script exhausted".into())),
        )
    }
}

struct WritingHost {
    backing: Arc<Backing>,
    return_pages: bool,
}

impl ToolHost for WritingHost {
    fn call(
        &self,
        call: &ToolCall,
        _context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        let text = call.arguments["text"].as_str().unwrap_or_default();
        self.backing.write("profile.md", text);
        let mut output = ToolOutput::ok("{\"saved\":true}");
        output.summary = Some(format!("Saved: {text}"));
        if self.return_pages {
            output.pages = vec![self.backing.page("profile.md")];
        }
        std::future::ready(output)
    }
}

fn assistant(id: &str, name: &str, arguments: serde_json::Value) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        })],
    }
}

async fn write_then_read(return_pages: bool) -> (Vec<Message>, Arc<Backing>) {
    write_then_read_with(return_pages, Vec::new()).await
}

async fn write_then_read_with(
    return_pages: bool,
    replay: Vec<JournalEntry>,
) -> (Vec<Message>, Arc<Backing>) {
    let backing = seeded(Backing::default());
    if !replay.is_empty() {
        backing.write("profile.md", "Remote or Berlin");
    }
    let workspace = workspace(&backing);
    let script = vec![
        assistant("r1", "read_file", json!({ "paths": ["profile.md"] })),
        assistant(
            "w1",
            "save_preference",
            json!({ "text": "Remote or Berlin" }),
        ),
        assistant("r2", "read_file", json!({ "paths": ["profile.md"] })),
        assistant("e1", "end_turn", json!({ "recorded": ["Berlin is fine"] })),
    ];
    let model = Scripted(Mutex::new(script.into()));
    let mut save = ToolDef::new(
        ToolSpec {
            name: "save_preference".into(),
            description: "Save a preference.".into(),
            input_schema: json!({ "type": "object" }),
        },
        Retention::Durable,
    );
    save.invalidates = vec![key(&["customer"])];
    let host = WritingHost {
        backing: Arc::clone(&backing),
        return_pages,
    };
    let outcome = run_turn(
        &TurnDeps {
            model: &model,
            host: &host,
            steering: &NoSteering,
            journal: &NoJournal,
            sink: &NoSink,
            cancel: &CancellationToken::new(),
        },
        TurnInput {
            session: "customer-1",
            cache_key: "customer-1",
            turn: "turn-1",
            system: "You are a helpful agent.",
            external_tools: &[save],
            history: &[],
            context: None,
            skills: &[],
            events: vec![Message::user_text("Berlin works too.")],
            workspace: &workspace,
            replay,
        },
        &TurnSettings {
            model: "scripted",
            first_call_effort: None,
            effort: None,
            max_output_tokens: 1024,
            max_model_calls: 6,
            max_turn_tokens: None,
            recite: true,
            lane: Lane::Interactive,
            compaction: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.end.recorded.len(), 1);
    let seen = workspace.read("profile.md").await.unwrap();
    assert_eq!(seen.as_deref(), Some("Remote or Berlin"));
    (outcome.committed, backing)
}

#[tokio::test]
async fn a_write_tool_invalidates_a_page_and_the_next_read_sees_the_refetch() {
    let (committed, backing) = write_then_read(false).await;
    assert_eq!(backing.fetches(), 1, "one refetch serves the re-read");
    let text = format!("{committed:?}");
    assert!(
        !text.contains("--- profile.md ---"),
        "reads never reach history"
    );
    assert!(text.contains("Saved: Remote or Berlin"));
}

#[tokio::test]
async fn pages_returned_by_a_tool_are_used_without_a_fetch() {
    let (_, backing) = write_then_read(true).await;
    assert_eq!(backing.fetches(), 0);
}

#[tokio::test]
async fn grep_waits_for_a_refetch_already_in_flight() {
    let backing = seeded(Backing::gated());
    let workspace = workspace(&backing);
    workspace.read("roles/acme.md").await.unwrap();
    backing.write("roles/acme.md", "Acme: staff engineer");
    workspace.invalidate(&[key(&["roles"])]);
    workspace.refresh_read_pages_in_background();
    until_fetches(&backing, 1).await;

    let search = tokio::spawn({
        let workspace = workspace.clone();
        async move { workspace.grep("staff", None, 10).await }
    });
    backing.release();
    let found = search.await.unwrap().unwrap();
    assert_eq!(found.len(), 1, "grep must search the refetched page");
    assert_eq!(backing.fetches(), 1);
}

#[tokio::test]
async fn a_cancelled_refetch_releases_the_page_for_the_next_read() {
    let backing = seeded(Backing::gated());
    let workspace = workspace(&backing);
    workspace.invalidate(&[key(&["customer"])]);
    let stuck = tokio::spawn({
        let workspace = workspace.clone();
        async move { workspace.read("profile.md").await }
    });
    until_fetches(&backing, 1).await;
    stuck.abort();
    assert!(stuck.await.unwrap_err().is_cancelled());

    backing.release();
    let content = tokio::time::timeout(Duration::from_secs(2), workspace.read("profile.md"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(content.as_deref(), Some("Remote only"));
    assert_eq!(backing.fetches(), 2);
}

#[tokio::test]
async fn a_replayed_write_never_restores_its_old_page_data() {
    let mut stale = ToolOutput::ok("{\"saved\":true}");
    stale.pages = vec![Page {
        key: key(&["customer", "profile"]),
        path: "profile.md".into(),
        version: "0".into(),
        content: "written before the crash".into(),
    }];
    let replay = vec![JournalEntry::ToolOutput {
        call_id: "w1".into(),
        output: stale,
    }];
    let (committed, _) = write_then_read_with(false, replay).await;
    assert!(!format!("{committed:?}").contains("written before the crash"));
}

fn loading(backing: &Arc<Backing>) -> Workspace {
    Workspace::loading(Arc::new(Source(Arc::clone(backing))))
}

#[tokio::test]
async fn a_loading_workspace_serves_reads_after_one_shared_load() {
    let backing = seeded(Backing::gated());
    let workspace = loading(&backing);
    until_fetches(&backing, 1).await;
    let reads: Vec<_> = (0..3)
        .map(|_| {
            let workspace = workspace.clone();
            tokio::spawn(async move { workspace.read("profile.md").await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        reads.iter().all(|read| !read.is_finished()),
        "reads wait for the load"
    );
    backing.release();
    for read in reads {
        assert_eq!(read.await.unwrap().unwrap().as_deref(), Some("Remote only"));
    }
    assert_eq!(backing.fetches(), 1, "one load serves every read");
    assert_eq!(workspace.glob("*.md").await.unwrap(), vec!["profile.md"]);
}

#[tokio::test]
async fn an_invalidation_during_the_load_is_applied_when_it_lands() {
    let backing = seeded(Backing::gated());
    let workspace = loading(&backing);
    until_fetches(&backing, 1).await;
    workspace.invalidate(&[key(&["customer"])]);
    backing.write("profile.md", "Remote or Berlin");
    backing.release();
    backing.release();
    let content = workspace.read("profile.md").await.unwrap();
    assert_eq!(content.as_deref(), Some("Remote or Berlin"));
    assert_eq!(backing.fetches(), 2);
}

#[tokio::test]
async fn data_put_during_the_load_is_not_overwritten_by_it() {
    let backing = seeded(Backing::gated());
    let workspace = loading(&backing);
    until_fetches(&backing, 1).await;
    let mut fresh = backing.page("profile.md");
    fresh.content = "Remote or Berlin".into();
    workspace.put(vec![fresh]);
    backing.release();
    let content = workspace.read("profile.md").await.unwrap();
    assert_eq!(content.as_deref(), Some("Remote or Berlin"));
}

#[tokio::test]
async fn a_failed_load_fails_reads_loudly() {
    let backing = seeded(Backing::default());
    backing.fail.store(true, Ordering::SeqCst);
    let workspace = loading(&backing);
    assert!(workspace.read("profile.md").await.is_err());
    assert!(workspace.glob("*").await.is_err());
}
