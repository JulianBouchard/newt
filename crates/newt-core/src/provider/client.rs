use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use reqwest::header::HeaderMap;
use tokio_util::sync::CancellationToken;

use super::capture::Capture;
use super::stream::run_stream;
use super::{Model, ModelResponse, Protocol, ProviderError, StreamEvent};
use super::{anthropic, chat, openai};
use crate::prompt::ModelRequest;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CALL_DEADLINE: Duration = Duration::from_mins(10);

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
    headers: HeaderMap,
    capture: Option<Capture>,
    deadline: Duration,
    replay_reasoning: bool,
}

impl fmt::Debug for ProviderClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderClient")
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("auth", &self.auth)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("capture", &self.capture)
            .field("deadline", &self.deadline)
            .field("replay_reasoning", &self.replay_reasoning)
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
            Protocol::AnthropicMessages => Auth::Header(protocol.key_header().to_owned()),
            Protocol::OpenaiResponses | Protocol::OpenaiChat => Auth::Bearer,
        };
        Ok(Self {
            http,
            protocol,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
            auth,
            headers: HeaderMap::new(),
            capture: None,
            deadline: CALL_DEADLINE,
            replay_reasoning: true,
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
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    #[must_use]
    pub fn without_reasoning_replay(mut self) -> Self {
        self.replay_reasoning = false;
        self
    }

    #[must_use]
    pub fn with_capture_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.capture = Some(Capture::new(dir.into()));
        self
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.headers(self.headers.clone());
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
                openai::request_body(request, self.replay_reasoning),
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

#[cfg(test)]
mod tests {
    use super::*;

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
