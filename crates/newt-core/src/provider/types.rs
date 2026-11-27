use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::message::Message;
use crate::prompt::ModelRequest;

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

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    OutputStarted,
    TextDelta(String),
    ToolCallStarted {
        id: String,
        name: String,
    },
    /// A chunk of a tool call's arguments, as the model writes them.
    ToolInputDelta {
        id: String,
        delta: String,
    },
    /// A tool call whose arguments have finished streaming, before the response ends.
    ToolCallReady(crate::message::ToolCall),
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("request to model provider failed")]
    Http(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("model provider went silent past the idle timeout")]
    Timeout(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("model call ran past its {}s deadline", .0.as_secs())]
    Deadline(Duration),
    #[error("model provider returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("{0}")]
    Sse(String),
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
                "overloaded_error"
                    | "api_error"
                    | "rate_limit_error"
                    | "server_error"
                    | "service_unavailable_error"
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

pub trait Model: Send + Sync {
    fn stream(
        &self,
        request: &ModelRequest<'_>,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> impl Future<Output = Result<ModelResponse, ProviderError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
