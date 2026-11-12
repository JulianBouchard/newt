mod provider;
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
        ["notes", "tasks", "sent"]
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
}

impl Demo {
    pub fn new(token: String, public_url: &str) -> Arc<Self> {
        Arc::new(Self {
            token,
            public_url: public_url.trim_end_matches('/').to_owned(),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        match self.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn with<T>(&self, key: &str, change: impl FnOnce(&mut Session) -> T) -> T {
        let mut sessions = self.lock();
        change(sessions.entry(key.to_owned()).or_default())
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

pub fn app(demo: Arc<Demo>) -> Router {
    let mcp_demo = Arc::clone(&demo);
    let mcp: StreamableHttpService<tools::Tools, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(tools::Tools(Arc::clone(&mcp_demo))),
        Arc::default(),
        StreamableHttpServerConfig::default(),
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
