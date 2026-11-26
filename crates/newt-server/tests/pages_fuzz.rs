#![expect(
    clippy::unwrap_used,
    clippy::unused_async_trait_impl,
    reason = "tests fail loudly on setup errors; the test server's handlers read like real async ones"
)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use newt_core::workspace::{PageSource, Workspace};
use newt_server::backend::SessionState;
use newt_server::mcp::{McpAuth, McpServer, Toolset};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;
use rmcp::model::{
    Annotations, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use tokio_util::sync::CancellationToken;

const BOUND: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
enum Body {
    Text(String),
    Blob,
    WrongUri,
    Missing,
    Fails,
}

#[derive(Debug, Clone)]
struct Weird {
    name: String,
    size: Option<u64>,
    description: Option<String>,
    priority: Option<f32>,
    body: Body,
}

#[derive(Clone)]
struct Server {
    resources: Arc<Vec<Weird>>,
    page_size: usize,
    list_fails: bool,
    reads: Arc<AtomicUsize>,
}

fn uri(index: usize) -> String {
    format!("weird://r/{index}")
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_resources().build())
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        if self.list_fails {
            return Err(ErrorData::internal_error("listing is down", None));
        }
        let start = request
            .and_then(|params| params.cursor)
            .and_then(|cursor| cursor.parse::<usize>().ok())
            .unwrap_or(0);
        let end = (start + self.page_size).min(self.resources.len());
        let resources = self
            .resources
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .zip(start..)
            .map(|(weird, index)| {
                let mut resource = Resource::new(uri(index), weird.name.clone());
                resource.size = weird.size;
                resource.description.clone_from(&weird.description);
                if let Some(priority) = weird.priority {
                    let mut annotations = Annotations::default();
                    annotations.priority = Some(priority);
                    resource.annotations = Some(annotations);
                }
                resource
            })
            .collect();
        let mut result = ListResourcesResult::with_all_items(resources);
        result.next_cursor = (end < self.resources.len()).then(|| end.to_string());
        Ok(result)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let index = request
            .uri
            .rsplit('/')
            .next()
            .and_then(|tail| tail.parse::<usize>().ok());
        let weird = index
            .and_then(|index| self.resources.get(index))
            .ok_or_else(|| ErrorData::invalid_params("no such resource", None))?;
        let contents = match &weird.body {
            Body::Text(text) => ResourceContents::text(text.clone(), request.uri),
            Body::Blob => ResourceContents::blob("AAAA", request.uri),
            Body::WrongUri => ResourceContents::text("elsewhere", "weird://other"),
            Body::Missing => return Err(ErrorData::invalid_params("no such resource", None)),
            Body::Fails => return Err(ErrorData::internal_error("the route timed out", None)),
        };
        Ok(ReadResourceResult::new(vec![contents]).into())
    }
}

async fn spawn(server: Server) -> (String, CancellationToken) {
    let stop = CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = stop.clone();
    let service: StreamableHttpService<Server, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Arc::default(), config);
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = stop.clone();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    (format!("http://{address}/mcp"), stop)
}

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => Just(String::new()).prop_map(|_| "unique".to_owned()),
        1 => Just("same.md".to_owned()),
        1 => Just(String::new()),
        1 => Just("   ".to_owned()),
        1 => "(\\.\\./|/|é|\u{202e}|[a-z]){1,12}",
        1 => Just("a/".repeat(200)),
    ]
}

fn priority() -> impl Strategy<Value = Option<f32>> {
    prop_oneof![
        3 => Just(None),
        3 => (0.0f32..=1.0).prop_map(Some),
        1 => Just(Some(f32::NAN)),
        1 => Just(Some(f32::INFINITY)),
        1 => Just(Some(-1.0)),
        1 => Just(Some(7.5)),
    ]
}

fn body() -> impl Strategy<Value = Body> {
    prop_oneof![
        6 => "[a-z ]{0,40}".prop_map(Body::Text),
        1 => Just(Body::Blob),
        1 => Just(Body::WrongUri),
        1 => Just(Body::Missing),
        1 => Just(Body::Fails),
    ]
}

fn weird() -> impl Strategy<Value = Weird> {
    (
        name(),
        prop_oneof![
            Just(None),
            any::<u64>().prop_map(Some),
            Just(Some(u64::MAX))
        ],
        prop_oneof![Just(None), ".{0,30}".prop_map(Some)],
        priority(),
        body(),
    )
        .prop_map(|(name, size, description, priority, body)| Weird {
            name,
            size,
            description,
            priority,
            body,
        })
}

fn shape_names(resources: &mut [Weird], messy: bool) {
    for (index, weird) in resources.iter_mut().enumerate() {
        if weird.name == "unique" {
            weird.name = format!("pages/p{index}.md");
        } else if !messy {
            weird.name = format!("{}{index}", weird.name.trim());
        }
    }
}

fn valid(resources: &[Weird]) -> bool {
    let mut seen = BTreeSet::new();
    resources
        .iter()
        .all(|weird| !weird.name.trim().is_empty() && seen.insert(weird.name.as_str()))
}

#[derive(Debug, Clone)]
struct Case {
    home: Vec<Weird>,
    extra: Option<Vec<Weird>>,
    page_size: usize,
    home_fails: bool,
    extra_fails: bool,
    sample: Vec<usize>,
}

fn resources() -> impl Strategy<Value = (Vec<Weird>, bool)> {
    (
        prop_oneof![
            6 => proptest::collection::vec(weird(), 1..12),
            1 => proptest::collection::vec(weird(), 1500..3000),
        ],
        proptest::bool::weighted(0.25),
    )
        .prop_map(|(mut resources, messy)| {
            shape_names(&mut resources, messy);
            (resources, messy)
        })
}

fn case() -> impl Strategy<Value = Case> {
    (
        resources(),
        proptest::option::weighted(0.4, resources()),
        1usize..400,
        proptest::bool::weighted(0.1),
        proptest::bool::weighted(0.2),
        proptest::collection::vec(any::<usize>(), 1..6),
    )
        .prop_map(
            |((home, _), extra, page_size, home_fails, extra_fails, sample)| Case {
                page_size: page_size.max(home.len() / 50).max(1),
                home,
                extra: extra.map(|(resources, _)| resources),
                home_fails,
                extra_fails,
                sample,
            },
        )
}

async fn serve(
    resources: &[Weird],
    page_size: usize,
    list_fails: bool,
) -> (String, Arc<AtomicUsize>, CancellationToken) {
    let reads = Arc::new(AtomicUsize::new(0));
    let (url, stop) = spawn(Server {
        resources: Arc::new(resources.to_vec()),
        page_size: page_size.max(resources.len() / 50).max(1),
        list_fails,
        reads: Arc::clone(&reads),
    })
    .await;
    (url, reads, stop)
}

fn server(name: &str, url: String) -> McpServer {
    McpServer {
        name: name.into(),
        url,
        auth: McpAuth::Bearer("agent-token".into()),
    }
}

async fn expect_read(workspace: &Workspace, path: &str, body: &Body) {
    let read = workspace.read(path).await;
    match body {
        Body::Text(text) => assert_eq!(read.unwrap().as_deref(), Some(text.as_str()), "{path}"),
        Body::Blob | Body::WrongUri => assert_eq!(read.unwrap(), None, "{path}"),
        Body::Missing | Body::Fails => assert!(read.is_err(), "{path}: {body:?}"),
    }
}

/// Runs one case; true when it got past the listing to the reads.
async fn check(case: Case) -> bool {
    let (home_url, home_reads, home_stop) =
        serve(&case.home, case.page_size, case.home_fails).await;
    let mut stops = vec![home_stop];
    let mut added = Vec::new();
    if let Some(extra) = &case.extra {
        let (url, _reads, stop) = serve(extra, case.page_size, case.extra_fails).await;
        stops.push(stop);
        added.push(server("partner", url));
    }
    let toolset = Toolset::connect(&server("app", home_url), &added)
        .await
        .unwrap();
    let pages = toolset.pages("s", &SessionState::default()).unwrap();
    let workspace = Workspace::loading(pages as Arc<dyn PageSource>);
    let reached = run_reads(&case, &workspace, &home_reads).await;
    for stop in stops {
        stop.cancel();
    }
    reached
}

async fn run_reads(case: &Case, workspace: &Workspace, home_reads: &AtomicUsize) -> bool {
    if case.home_fails || !valid(&case.home) {
        assert!(
            workspace.glob("**").await.is_err(),
            "a bad home listing fails loudly"
        );
        return false;
    }
    let extra_listed = case
        .extra
        .as_ref()
        .filter(|extra| !case.extra_fails && valid(extra));
    let index = workspace.index().await.unwrap();
    let listed: BTreeSet<String> = index.pages.iter().map(|page| page.path.clone()).collect();
    let mut expected: BTreeSet<String> = case.home.iter().map(|weird| weird.name.clone()).collect();
    for weird in extra_listed.into_iter().flatten() {
        expected.insert(format!("partner/{}", weird.name));
    }
    assert_eq!(
        listed, expected,
        "the index is the home's listing and the partner's"
    );
    for weird in &case.home {
        let entry = index
            .pages
            .iter()
            .find(|page| page.path == weird.name)
            .unwrap();
        assert_eq!(entry.size, weird.size);
        assert_eq!(entry.description, weird.description);
        assert_eq!(
            entry.priority,
            weird.priority.filter(|priority| priority.is_finite())
        );
    }
    assert_eq!(
        home_reads.load(Ordering::SeqCst),
        0,
        "listing reads no page"
    );

    let picked: Vec<&Weird> = case
        .sample
        .iter()
        .filter_map(|index| case.home.get(index % case.home.len()))
        .collect();
    for weird in &picked {
        expect_read(workspace, &weird.name, &weird.body).await;
    }
    assert!(
        home_reads.load(Ordering::SeqCst) <= picked.len(),
        "only the pages asked for were read"
    );
    if let Some(weird) = extra_listed.and_then(|extra| extra.first()) {
        expect_read(workspace, &format!("partner/{}", weird.name), &weird.body).await;
        if let Some(good) = case
            .home
            .iter()
            .find(|weird| matches!(weird.body, Body::Text(_)))
        {
            expect_read(workspace, &good.name, &good.body).await;
        }
    }
    true
}

#[test]
fn weird_mcp_servers_list_and_read_page_by_page() {
    let reached = AtomicUsize::new(0);
    let cases = AtomicUsize::new(0);
    let mut runner = TestRunner::new(ProptestConfig {
        cases: 40,
        ..ProptestConfig::default()
    });
    runner
        .run(&case(), |case| {
            cases.fetch_add(1, Ordering::SeqCst);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let done = runtime.block_on(async { tokio::time::timeout(BOUND, check(case)).await });
            prop_assert!(done.is_ok(), "the case hung");
            if done.unwrap_or(false) {
                reached.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
        .unwrap();
    let (reached, cases) = (reached.load(Ordering::SeqCst), cases.load(Ordering::SeqCst));
    assert!(
        reached * 2 > cases,
        "{reached} of {cases} cases reached the reads"
    );
}
