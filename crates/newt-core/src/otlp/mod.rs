mod assemble;
mod spans;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::Event;
use crate::sink::EventSink;

use assemble::{Assembler, TurnTrace};
pub use spans::tool_traceparent;

const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EXPORTS_IN_FLIGHT: usize = 4;
const UNFINISHED_AFTER: Duration = Duration::from_mins(30);
const EVICT_EVERY: Duration = Duration::from_mins(1);
const ERROR_BODY_LIMIT: usize = 300;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Generic,
    Langfuse,
}

#[derive(Clone)]
pub struct OtlpConfig {
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub profile: Profile,
    pub capture_content: bool,
    pub service_name: String,
}

impl std::fmt::Debug for OtlpConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OtlpConfig")
            .field("endpoint", &self.endpoint)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("profile", &self.profile)
            .field("capture_content", &self.capture_content)
            .field("service_name", &self.service_name)
            .finish()
    }
}

pub trait Enricher: Send + Sync {
    fn attributes(&self, session: &str) -> Vec<(String, String)>;
}

pub trait Redactor: Send + Sync {
    fn redact(&self, content: String) -> String;
}

#[derive(Clone, Default)]
pub struct Hooks {
    pub enricher: Option<Arc<dyn Enricher>>,
    pub redactor: Option<Arc<dyn Redactor>>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hooks")
            .field("enricher", &self.enricher.is_some())
            .field("redactor", &self.redactor.is_some())
            .finish()
    }
}

pub fn parse_headers(raw: &str) -> Result<Vec<(String, String)>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            pair.split_once('=')
                .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
                .filter(|(name, _)| !name.is_empty())
                .ok_or_else(|| "headers must look like name=value,name=value".to_owned())
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct OtlpSink {
    sender: mpsc::Sender<Event>,
    dropped: Arc<AtomicU64>,
}

impl OtlpSink {
    pub fn spawn(config: OtlpConfig, hooks: Hooks, capacity: usize) -> (Self, JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let exporter = tokio::spawn(run(
            Arc::new(Exporter {
                client: reqwest::Client::new(),
                config,
                hooks,
            }),
            receiver,
            Arc::clone(&dropped),
        ));
        (Self { sender, dropped }, exporter)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl EventSink for OtlpSink {
    fn emit(&self, event: Event) {
        if event.kind.is_live_only() {
            return;
        }
        if self.sender.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct Exporter {
    client: reqwest::Client,
    config: OtlpConfig,
    hooks: Hooks,
}

async fn run(
    exporter: Arc<Exporter>,
    mut receiver: mpsc::Receiver<Event>,
    dropped: Arc<AtomicU64>,
) {
    let mut assembler = Assembler::default();
    let mut exports = tokio::task::JoinSet::new();
    let mut last_eviction = std::time::Instant::now();
    let mut reported_drops = 0;
    while let Some(event) = receiver.recv().await {
        let mut finished: Vec<TurnTrace> = assembler.apply(event).into_iter().collect();
        if last_eviction.elapsed() >= EVICT_EVERY {
            last_eviction = std::time::Instant::now();
            finished.extend(
                assembler.evict_older_than(time::OffsetDateTime::now_utc() - UNFINISHED_AFTER),
            );
        }
        if finished.is_empty() {
            continue;
        }
        let drops = dropped.load(Ordering::Relaxed);
        if drops > reported_drops {
            tracing::warn!(
                dropped = drops - reported_drops,
                "trace events dropped: the exporter is behind"
            );
            reported_drops = drops;
        }
        while exports.len() >= MAX_EXPORTS_IN_FLIGHT {
            exports.join_next().await;
        }
        let exporter = Arc::clone(&exporter);
        exports.spawn(async move { exporter.export(&finished).await });
    }
    let unfinished = assembler.drain();
    if !unfinished.is_empty() {
        exporter.export(&unfinished).await;
    }
    while exports.join_next().await.is_some() {}
}

pub(crate) fn request_body(
    config: &OtlpConfig,
    hooks: &Hooks,
    traces: &[TurnTrace],
) -> serde_json::Value {
    let spans: Vec<serde_json::Value> = traces
        .iter()
        .flat_map(|trace| spans::spans(trace, config, hooks))
        .collect();
    json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                { "key": "service.name", "value": { "stringValue": config.service_name } },
            ] },
            "scopeSpans": [{
                "scope": { "name": "newt", "version": env!("CARGO_PKG_VERSION") },
                "spans": spans,
            }],
        }],
    })
}

impl Exporter {
    async fn export(&self, traces: &[TurnTrace]) {
        let mut request = self
            .client
            .post(&self.config.endpoint)
            .timeout(EXPORT_TIMEOUT)
            .json(&request_body(&self.config, &self.hooks, traces));
        for (name, value) in &self.config.headers {
            request = request.header(name, value);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                let status = response.status();
                let body: String = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(ERROR_BODY_LIMIT)
                    .collect();
                tracing::warn!(%status, %body, "OTLP export rejected");
            }
            Err(error) => tracing::warn!(error = %error.without_url(), "OTLP export failed"),
        }
    }
}
