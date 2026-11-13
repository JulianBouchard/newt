mod anthropic;
mod capture;
mod chat;
mod openai;

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::message::{INVALID_ARGUMENTS_KEY, Message, Part, Role};
use crate::prompt::ModelRequest;
use crate::sse::{SseError, SseEvent, SseParser};
use capture::{Capture, CaptureStream};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CALL_DEADLINE: Duration = Duration::from_mins(10);
const ERROR_BODY_LIMIT: usize = 2000;
const DECODE_SNIPPET_LIMIT: usize = 200;
const REQUEST_ID_HEADERS: [&str; 5] = [
    "x-request-id",
    "request-id",
    "apim-request-id",
    "x-ms-request-id",
    "cf-ray",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    AnthropicMessages,
    OpenaiResponses,
    /// Chat Completions, which vLLM, Ollama and most other hosts speak.
    OpenaiChat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts are far below 2^52, a ratio needs no more precision"
    )]
    pub fn cache_hit_ratio(&self) -> f64 {
        if self.input_tokens == 0 {
            return 0.0;
        }
        self.cache_read_tokens as f64 / self.input_tokens as f64
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallTiming {
    pub headers_ms: Option<u64>,
    pub first_event_ms: Option<u64>,
    pub first_output_ms: Option<u64>,
    pub first_tool_ms: Option<u64>,
    pub total_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: Usage,
    pub stop: StopReason,
    pub timing: CallTiming,
    pub response_id: Option<String>,
    pub request_ids: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    OutputStarted,
    ToolCallStarted { id: String, name: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("request to model provider failed")]
    Http(#[source] reqwest::Error),
    #[error("model provider went silent past the idle timeout")]
    Timeout(#[source] reqwest::Error),
    #[error("model call ran past its {}s deadline", .0.as_secs())]
    Deadline(Duration),
    #[error("model provider returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error(transparent)]
    Sse(#[from] SseError),
    #[error("could not decode `{event}` from the model provider: {snippet}")]
    Decode {
        event: String,
        snippet: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("model provider reported {kind}: {message}")]
    Api { kind: String, message: String },
    #[error("model stream broke protocol: {0}")]
    Protocol(String),
    #[error("could not write the capture of a model call")]
    Capture(#[source] std::io::Error),
    #[error("no model route is configured for `{0}`")]
    UnknownModel(String),
    #[error("model call was cancelled")]
    Cancelled,
}

impl ProviderError {
    pub fn retryable(&self) -> bool {
        match self {
            Self::Http(_) | Self::Timeout(_) | Self::Deadline(_) => true,
            Self::Status { status, .. } => matches!(status, 408 | 409 | 429 | 500..=599),
            Self::Api { kind, .. } => matches!(
                kind.as_str(),
                "overloaded_error" | "api_error" | "rate_limit_error" | "server_error"
            ),
            Self::Sse(_)
            | Self::Decode { .. }
            | Self::Protocol(_)
            | Self::Capture(_)
            | Self::UnknownModel(_)
            | Self::Cancelled => false,
        }
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout(error)
        } else {
            Self::Http(error)
        }
    }
}

pub trait Model: Send + Sync {
    fn stream(
        &self,
        request: &ModelRequest<'_>,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Auth {
    Bearer,
    Header(String),
}

#[derive(Clone)]
pub struct ProviderClient {
    http: reqwest::Client,
    protocol: Protocol,
    base_url: String,
    api_key: String,
    auth: Auth,
    capture: Option<Capture>,
    deadline: Duration,
}

impl fmt::Debug for ProviderClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderClient")
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("auth", &self.auth)
            .field("capture", &self.capture)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl ProviderClient {
    pub fn new(
        protocol: Protocol,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(IDLE_TIMEOUT)
            .build()?;
        let auth = match protocol {
            Protocol::AnthropicMessages => Auth::Header("x-api-key".to_owned()),
            Protocol::OpenaiResponses | Protocol::OpenaiChat => Auth::Bearer,
        };
        Ok(Self {
            http,
            protocol,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
            auth,
            capture: None,
            deadline: CALL_DEADLINE,
        })
    }

    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    #[must_use]
    pub fn with_api_key_header(mut self, header: impl Into<String>) -> Self {
        self.auth = Auth::Header(header.into());
        self
    }

    #[must_use]
    pub fn with_capture_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.capture = Some(Capture::new(dir.into()));
        self
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::Bearer => request.bearer_auth(&self.api_key),
            Auth::Header(name) => request.header(name.as_str(), &self.api_key),
        }
    }
}

impl Model for ProviderClient {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        let (url, body) = match self.protocol {
            Protocol::AnthropicMessages => (
                format!("{}/v1/messages", self.base_url),
                anthropic::request_body(request),
            ),
            Protocol::OpenaiResponses => (
                format!("{}/responses", self.base_url),
                openai::request_body(request),
            ),
            Protocol::OpenaiChat => (
                format!("{}/chat/completions", self.base_url),
                chat::request_body(request),
            ),
        };
        let capture = match &self.capture {
            Some(capture) => Some(capture.begin(&body).await.map_err(ProviderError::Capture)?),
            None => None,
        };
        let mut http = self.authorize(self.http.post(url)).json(&body);
        if self.protocol == Protocol::AnthropicMessages {
            http = http.header("anthropic-version", "2023-06-01");
        }
        let call = async {
            match self.protocol {
                Protocol::AnthropicMessages => {
                    run_stream::<anthropic::Decoder>(http, capture, cancel, on_event).await
                }
                Protocol::OpenaiResponses => {
                    run_stream::<openai::Decoder>(http, capture, cancel, on_event).await
                }
                Protocol::OpenaiChat => {
                    run_stream::<chat::Decoder>(http, capture, cancel, on_event).await
                }
            }
        };
        tokio::time::timeout(self.deadline, call)
            .await
            .unwrap_or(Err(ProviderError::Deadline(self.deadline)))
    }
}

struct Decoded {
    parts: Vec<Part>,
    usage: Usage,
    stop: StopReason,
    response_id: Option<String>,
}

trait Decoder: Default + Send {
    fn handle(&mut self, event: &SseEvent, emit: &mut Emitter<'_>) -> Result<bool, ProviderError>;
    fn finish(self) -> Result<Decoded, ProviderError>;
}

struct Emitter<'a> {
    started: Instant,
    headers: Option<Duration>,
    first_event: Option<Duration>,
    first_output: Option<Duration>,
    first_tool: Option<Duration>,
    on_event: &'a mut (dyn FnMut(StreamEvent) + Send),
}

impl Emitter<'_> {
    fn headers_received(&mut self) {
        self.headers = Some(self.started.elapsed());
    }

    fn event_received(&mut self) {
        if self.first_event.is_none() {
            self.first_event = Some(self.started.elapsed());
        }
    }

    fn output(&mut self) {
        if self.first_output.is_none() {
            self.first_output = Some(self.started.elapsed());
            (self.on_event)(StreamEvent::OutputStarted);
        }
    }

    fn tool_started(&mut self, id: &str, name: &str) {
        self.output();
        if self.first_tool.is_none() {
            self.first_tool = Some(self.started.elapsed());
        }
        (self.on_event)(StreamEvent::ToolCallStarted {
            id: id.to_owned(),
            name: name.to_owned(),
        });
    }

    fn timing(&self) -> CallTiming {
        CallTiming {
            headers_ms: self.headers.map(millis),
            first_event_ms: self.first_event.map(millis),
            first_output_ms: self.first_output.map(millis),
            first_tool_ms: self.first_tool.map(millis),
            total_ms: millis(self.started.elapsed()),
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

async fn run_stream<D: Decoder>(
    request: reqwest::RequestBuilder,
    mut capture: Option<CaptureStream>,
    cancel: &CancellationToken,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<ModelResponse, ProviderError> {
    let mut emit = Emitter {
        started: Instant::now(),
        headers: None,
        first_event: None,
        first_output: None,
        first_tool: None,
        on_event,
    };
    let response = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(ProviderError::Cancelled),
        response = request.send() => response?,
    };
    emit.headers_received();
    let correlation_ids = request_ids(response.headers());
    let status = response.status();
    if let Some(capture) = capture.as_mut() {
        capture
            .write(format!("HTTP {status}\n\n").as_bytes())
            .await
            .map_err(ProviderError::Capture)?;
    }
    if !status.is_success() {
        let body = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ProviderError::Cancelled),
            body = read_error_body(response) => body,
        };
        if let Some(capture) = capture.as_mut() {
            capture
                .write(body.as_bytes())
                .await
                .map_err(ProviderError::Capture)?;
        }
        return Err(ProviderError::Status {
            status: status.as_u16(),
            body,
        });
    }

    let mut chunks = response.bytes_stream();
    let mut parser = SseParser::default();
    let mut decoder = D::default();
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ProviderError::Cancelled),
            next = chunks.next() => next,
        };
        let Some(chunk) = next else {
            return Err(ProviderError::Protocol(
                "stream ended before the response completed".to_owned(),
            ));
        };
        let chunk = chunk?;
        if let Some(capture) = capture.as_mut() {
            capture
                .write(&chunk)
                .await
                .map_err(ProviderError::Capture)?;
        }
        for event in parser.feed(&chunk)? {
            emit.event_received();
            if decoder.handle(&event, &mut emit)? {
                let finished = decoder.finish()?;
                return Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        parts: finished.parts,
                    },
                    usage: finished.usage,
                    stop: finished.stop,
                    timing: emit.timing(),
                    response_id: finished.response_id,
                    request_ids: correlation_ids,
                });
            }
        }
    }
}

fn request_ids(headers: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    REQUEST_ID_HEADERS
        .iter()
        .filter_map(|name| {
            let value = headers.get(*name)?.to_str().ok()?;
            Some(((*name).to_owned(), value.to_owned()))
        })
        .collect()
}

async fn read_error_body(response: reqwest::Response) -> String {
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while body.len() < ERROR_BODY_LIMIT {
        match chunks.next().await {
            Some(Ok(chunk)) => body.extend_from_slice(&chunk),
            Some(Err(error)) => return format!("unreadable error body: {error}"),
            None => break,
        }
    }
    body.truncate(ERROR_BODY_LIMIT);
    String::from_utf8_lossy(&body).into_owned()
}

fn decode<T: DeserializeOwned>(event: &SseEvent) -> Result<T, ProviderError> {
    serde_json::from_str(&event.data).map_err(|source| ProviderError::Decode {
        event: event.event.clone().unwrap_or_else(|| "message".to_owned()),
        snippet: event.data.chars().take(DECODE_SNIPPET_LIMIT).collect(),
        source,
    })
}

fn tool_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({ INVALID_ARGUMENTS_KEY: raw }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_arguments_keep_objects_and_flag_everything_else() {
        assert_eq!(tool_arguments(""), json!({}));
        assert_eq!(tool_arguments("{\"a\":1}"), json!({ "a": 1 }));
        assert_eq!(
            tool_arguments("{\"a\": [1, 2"),
            json!({ INVALID_ARGUMENTS_KEY: "{\"a\": [1, 2" })
        );
        assert_eq!(
            tool_arguments("[1]"),
            json!({ INVALID_ARGUMENTS_KEY: "[1]" })
        );
    }

    #[test]
    fn rate_limits_overloads_and_server_errors_are_retryable() {
        let status = |status| ProviderError::Status {
            status,
            body: String::new(),
        };
        assert!(status(429).retryable());
        assert!(status(529).retryable());
        assert!(!status(400).retryable());
        assert!(
            ProviderError::Api {
                kind: "overloaded_error".into(),
                message: "busy".into()
            }
            .retryable()
        );
        assert!(!ProviderError::Cancelled.retryable());
    }

    #[test]
    fn debug_output_never_shows_the_api_key() {
        let client = ProviderClient::new(Protocol::OpenaiResponses, "http://x", "sk-secret")
            .unwrap_or_else(|error| {
                unreachable!("building a client without network access cannot fail: {error}")
            })
            .with_api_key_header("api-key");
        assert!(!format!("{client:?}").contains("sk-secret"));
    }
}
