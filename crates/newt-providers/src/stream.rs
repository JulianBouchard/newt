use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::capture::CaptureStream;
use crate::sse::{SseEvent, SseParser};
use newt_core::message::{Message, Part, Role};
use newt_core::provider::{
    CallTiming, ModelResponse, ProviderError, StopReason, StreamEvent, Usage,
};

const ERROR_BODY_LIMIT: usize = 2000;
const DECODE_SNIPPET_LIMIT: usize = 200;
const REQUEST_ID_HEADERS: [&str; 5] = [
    "x-request-id",
    "request-id",
    "apim-request-id",
    "x-ms-request-id",
    "cf-ray",
];

pub(super) struct Decoded {
    pub(super) parts: Vec<Part>,
    pub(super) usage: Usage,
    pub(super) stop: StopReason,
    pub(super) response_id: Option<String>,
}

pub(super) trait Decoder: Default + Send {
    fn handle(&mut self, event: &SseEvent, emit: &mut Emitter<'_>) -> Result<bool, ProviderError>;
    fn finish(self) -> Result<Decoded, ProviderError>;
}

pub(super) struct Emitter<'a> {
    started: Instant,
    headers: Option<Duration>,
    first_event: Option<Duration>,
    first_output: Option<Duration>,
    first_tool: Option<Duration>,
    on_event: &'a mut (dyn FnMut(StreamEvent) + Send),
}

impl Emitter<'_> {
    pub(super) fn headers_received(&mut self) {
        self.headers = Some(self.started.elapsed());
    }

    pub(super) fn event_received(&mut self) {
        if self.first_event.is_none() {
            self.first_event = Some(self.started.elapsed());
        }
    }

    pub(super) fn output(&mut self) {
        if self.first_output.is_none() {
            self.first_output = Some(self.started.elapsed());
            (self.on_event)(StreamEvent::OutputStarted);
        }
    }

    pub(super) fn text(&mut self, text: &str) {
        self.output();
        (self.on_event)(StreamEvent::TextDelta(text.to_owned()));
    }

    pub(super) fn tool_started(&mut self, id: &str, name: &str) {
        self.output();
        if self.first_tool.is_none() {
            self.first_tool = Some(self.started.elapsed());
        }
        (self.on_event)(StreamEvent::ToolCallStarted {
            id: id.to_owned(),
            name: name.to_owned(),
        });
    }

    pub(super) fn tool_input_delta(&mut self, id: &str, delta: &str) {
        (self.on_event)(StreamEvent::ToolInputDelta {
            id: id.to_owned(),
            delta: delta.to_owned(),
        });
    }

    pub(super) fn tool_ready(&mut self, call: &newt_core::message::ToolCall) {
        (self.on_event)(StreamEvent::ToolCallReady(call.clone()));
    }

    pub(super) fn timing(&self) -> CallTiming {
        CallTiming {
            headers_ms: self.headers.map(millis),
            first_event_ms: self.first_event.map(millis),
            first_output_ms: self.first_output.map(millis),
            first_tool_ms: self.first_tool.map(millis),
            total_ms: millis(self.started.elapsed()),
        }
    }
}

pub(super) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) async fn run_stream<D: Decoder>(
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
        response = request.send() => response.map_err(crate::http)?,
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
        let chunk = chunk.map_err(crate::http)?;
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

pub(super) fn request_ids(headers: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    REQUEST_ID_HEADERS
        .iter()
        .filter_map(|name| {
            let value = headers.get(*name)?.to_str().ok()?;
            Some(((*name).to_owned(), value.to_owned()))
        })
        .collect()
}

pub(super) async fn read_error_body(response: reqwest::Response) -> String {
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

pub(super) fn decode<T: DeserializeOwned>(event: &SseEvent) -> Result<T, ProviderError> {
    serde_json::from_str(&event.data).map_err(|source| ProviderError::Decode {
        event: event.event.clone().unwrap_or_else(|| "message".to_owned()),
        snippet: event.data.chars().take(DECODE_SNIPPET_LIMIT).collect(),
        source,
    })
}
