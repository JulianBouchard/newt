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

use spans::{Assembler, TurnTrace};

const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
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
                .ok_or_else(|| "OTLP headers must look like name=value,name=value".to_owned())
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
        let exporter = tokio::spawn(run(config, hooks, receiver));
        (
            Self {
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            exporter,
        )
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl EventSink for OtlpSink {
    fn emit(&self, event: Event) {
        if self.sender.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn run(config: OtlpConfig, hooks: Hooks, mut receiver: mpsc::Receiver<Event>) {
    let client = reqwest::Client::new();
    let mut assembler = Assembler::default();
    while let Some(event) = receiver.recv().await {
        if let Some(trace) = assembler.apply(event) {
            export(&client, &config, &hooks, &[trace]).await;
        }
    }
    let unfinished = assembler.drain();
    if !unfinished.is_empty() {
        export(&client, &config, &hooks, &unfinished).await;
    }
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

async fn export(
    client: &reqwest::Client,
    config: &OtlpConfig,
    hooks: &Hooks,
    traces: &[TurnTrace],
) {
    let mut request = client
        .post(&config.endpoint)
        .timeout(EXPORT_TIMEOUT)
        .json(&request_body(config, hooks, traces));
    for (name, value) in &config.headers {
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
