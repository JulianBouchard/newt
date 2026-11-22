//! `POST .../chat`: the AI SDK's `DefaultChatTransport` endpoint.

use std::collections::VecDeque;
use std::convert::Infallible;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde::Deserialize;
use serde_json::Value;

use super::ui_stream::UiStream;
use super::{ApiError, AppState, KEEP_ALIVE, LiveFeed, accept_event, authorize};
use crate::live::Channel;
use crate::store::{InboundEvent, InboundKind, Policy, Priority, live_topic};

const UI_STREAM_HEADER: &str = "x-vercel-ai-ui-message-stream";
const DONE: &str = "[DONE]";

#[derive(Debug, Deserialize)]
struct ChatRequest {
    messages: Vec<UiMessage>,
}

#[derive(Debug, Deserialize)]
struct UiMessage {
    id: String,
    role: String,
    #[serde(default)]
    parts: Vec<UiPart>,
}

#[derive(Debug, Deserialize)]
struct UiPart {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

fn last_user_event(body: &[u8]) -> Result<InboundEvent, ApiError> {
    let request: ChatRequest =
        serde_json::from_slice(body).map_err(|_| ApiError::Invalid("invalid chat body"))?;
    let message = request
        .messages
        .into_iter()
        .rev()
        .find(|message| message.role == "user")
        .ok_or(ApiError::Invalid("the chat has no user message"))?;
    let text: Vec<String> = message
        .parts
        .into_iter()
        .filter(|part| part.kind == "text")
        .map(|part| part.text)
        .collect();
    Ok(InboundEvent {
        id: message.id,
        kind: InboundKind::default(),
        policy: Policy::default(),
        priority: Priority::default(),
        text: text.join("\n\n"),
        meta: None,
    })
}

pub(super) async fn chat(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    authorize(&state, &agent, &key, &headers)?;
    if !state.live_stream.contains(&agent) {
        return Err(ApiError::Invalid(
            "chat needs the agent's live_stream = true",
        ));
    }
    let event = last_user_event(&body)?;
    accept_event(&state, &agent, &key, &event).await?;
    let cursor = state
        .store
        .received_seq(&agent, &key, &event.id)
        .await?
        .ok_or(ApiError::Conflict {
            code: "live_frames_gone",
            message: "the message was accepted but its live frames are gone",
        })?;
    let feed = LiveFeed {
        receiver: state.live.watch(Channel::Live, live_topic(&agent, &key)),
        store: state.store,
        agent,
        key,
        cursor,
        buffer: VecDeque::new(),
        must_read: true,
        shutdown: state.shutdown,
        gap: None,
    };
    let (ui, first) = UiStream::start();
    let chat = ChatFeed {
        feed,
        ui,
        queue: first.into_iter().map(|chunk| chunk.to_string()).collect(),
        done_sent: false,
    };
    let sse = Sse::new(stream::unfold(chat, ChatFeed::next))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE));
    Ok((
        [(
            HeaderName::from_static(UI_STREAM_HEADER),
            HeaderValue::from_static("v1"),
        )],
        sse,
    )
        .into_response())
}

struct ChatFeed {
    feed: LiveFeed,
    ui: UiStream,
    queue: VecDeque<String>,
    done_sent: bool,
}

impl ChatFeed {
    async fn next(mut self) -> Option<(Result<SseEvent, Infallible>, Self)> {
        loop {
            if let Some(data) = self.queue.pop_front() {
                return Some((Ok(SseEvent::default().data(data)), self));
            }
            if self.ui.finished() {
                if self.done_sent {
                    return None;
                }
                self.done_sent = true;
                return Some((Ok(SseEvent::default().data(DONE)), self));
            }
            let frame: Value = self.feed.next_event().await?.event;
            self.queue.extend(
                self.ui
                    .push(&frame)
                    .into_iter()
                    .map(|chunk| chunk.to_string()),
            );
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::last_user_event;

    #[test]
    fn the_last_user_message_becomes_the_event_with_its_text_parts_joined() {
        let body = br#"{"id":"chat","trigger":"submit-message","messages":[
            {"id":"u1","role":"user","parts":[{"type":"text","text":"first"}]},
            {"id":"a1","role":"assistant","parts":[{"type":"text","text":"reply"}]},
            {"id":"u2","role":"user","parts":[
                {"type":"text","text":"one"},{"type":"file","url":"x"},{"type":"text","text":"two"}]}]}"#;

        let event = last_user_event(body).unwrap();

        assert_eq!(event.id, "u2");
        assert_eq!(event.text, "one\n\ntwo");
    }

    #[test]
    fn a_chat_without_a_user_message_is_refused() {
        let body = br#"{"messages":[{"id":"a1","role":"assistant","parts":[]}]}"#;
        assert!(last_user_event(body).is_err());
    }
}
