//! A small reference app for newt: a context provider and an MCP tool server for a
//! to-do assistant, used by the quickstart and the integration tests.

mod flows;
mod provider;
mod resources;
mod tools;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Page {
    pub key: Vec<String>,
    pub path: String,
    pub version: String,
    pub content: String,
}

#[derive(Debug, Clone)]
struct Task {
    id: u32,
    text: String,
    done: bool,
}

#[derive(Debug, Default)]
struct Session {
    revision: u64,
    notes: Vec<String>,
    tasks: Vec<Task>,
    sent: Vec<String>,
    sent_keys: HashSet<String>,
}

impl Session {
    fn page(&self, name: &str) -> Option<Page> {
        let mut content = String::new();
        let path = match name {
            "notes" => {
                content.push_str("# Notes\n");
                if self.notes.is_empty() {
                    content.push_str("(none yet)\n");
                }
                for note in &self.notes {
                    let _ = writeln!(content, "- {note}");
                }
                "notes.md"
            }
            "tasks" => {
                content.push_str("# Tasks\n");
                if self.tasks.is_empty() {
                    content.push_str("(none yet)\n");
                }
                for task in &self.tasks {
                    let mark = if task.done { "x" } else { " " };
                    let _ = writeln!(content, "- [{mark}] #{} {}", task.id, task.text);
                }
                "tasks.md"
            }
            "sent" => {
                content.push_str("# Messages you sent\n");
                if self.sent.is_empty() {
                    content.push_str("(none yet)\n");
                }
                for message in &self.sent {
                    let _ = writeln!(content, "- {message}");
                }
                "sent.md"
            }
            _ => return None,
        };
        Some(Page {
            key: vec![name.to_owned()],
            path: path.to_owned(),
            version: self.revision.to_string(),
            content,
        })
    }

    fn pages(&self) -> Vec<Page> {
        resources::PAGES
            .iter()
            .filter_map(|name| self.page(name))
            .collect()
    }
}

#[derive(Debug)]
pub struct Demo {
    token: String,
    public_url: String,
    sessions: Mutex<HashMap<String, Session>>,
    listeners: Mutex<Vec<(u64, rmcp::service::SubscriptionSink)>>,
    next_listener: std::sync::atomic::AtomicU64,
    reviews: Mutex<HashMap<String, (std::time::Instant, String)>>,
}

impl Demo {
    pub fn new(token: String, public_url: &str) -> Arc<Self> {
        Arc::new(Self {
            token,
            public_url: public_url.trim_end_matches('/').to_owned(),
            sessions: Mutex::new(HashMap::new()),
            listeners: Mutex::new(Vec::new()),
            next_listener: std::sync::atomic::AtomicU64::new(0),
            reviews: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        match self.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn listeners(&self) -> MutexGuard<'_, Vec<(u64, rmcp::service::SubscriptionSink)>> {
        match self.listeners.lock() {
            Ok(listeners) => listeners,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn with<T>(&self, key: &str, change: impl FnOnce(&mut Session) -> T) -> T {
        let mut sessions = self.lock();
        change(sessions.entry(key.to_owned()).or_default())
    }

    fn reviews(&self) -> MutexGuard<'_, HashMap<String, (std::time::Instant, String)>> {
        match self.reviews.lock() {
            Ok(reviews) => reviews,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn start_review(&self, session: &str, at: std::time::Instant) -> String {
        let mut reviews = self.reviews();
        let id = format!("review-{}", reviews.len() + 1);
        reviews.insert(id.clone(), (at, session.to_owned()));
        id
    }

    fn review(&self, id: &str) -> Option<(std::time::Instant, String)> {
        self.reviews().get(id).cloned()
    }

    pub fn tasks(&self, key: &str) -> Vec<String> {
        self.with(key, |session| {
            session.tasks.iter().map(|task| task.text.clone()).collect()
        })
    }

    pub fn sent(&self, key: &str) -> Vec<String> {
        self.with(key, |session| session.sent.clone())
    }
}

async fn require_token(
    State(demo): State<Arc<Demo>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = format!("Bearer {}", demo.token);
    let presented = request
        .headers()
        .get("authorization")
        .map(axum::http::HeaderValue::as_bytes);
    if presented != Some(expected.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

/// rmcp only answers requests whose Host is local, against DNS rebinding.
/// The demo also answers to the host of its public URL, such as `demo` in
/// Docker Compose.
fn allowed_hosts(public_url: &str) -> Vec<String> {
    let host = public_url
        .split_once("://")
        .map_or(public_url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let mut hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    if !host.is_empty() {
        hosts.push(host.to_owned());
        if let Some((name, _port)) = host.rsplit_once(':') {
            hosts.push(name.to_owned());
        }
    }
    hosts
}

pub fn app(demo: Arc<Demo>) -> Router {
    let mcp_demo = Arc::clone(&demo);
    let mcp: StreamableHttpService<tools::Tools, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(tools::Tools(Arc::clone(&mcp_demo))),
        Arc::default(),
        StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts(&demo.public_url)),
    );
    Router::new()
        .merge(provider::routes())
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&demo),
            require_token,
        ))
        .with_state(demo)
}
