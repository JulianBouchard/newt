use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use futures_util::stream;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use self::error::{ApiError, json_errors};
use crate::cancel::CancelRequest;
use crate::counters::Counter;
use crate::events::{Observers, ServerEventKind};
use crate::live::{Channel, Topics, Watch};
use crate::metrics::Metrics;
use crate::retention::Deleted;
use crate::store::{AcceptError, Accepted, InboundEvent, LiveEvent, Store, live_topic};

mod chat;
mod error;
mod fork;
mod input;
mod ui_stream;

const EVENT_BODY_LIMIT: usize = 2 * 1024 * 1024;

const LIVE_PAGE: i64 = 100;
const LIVE_POLL: Duration = Duration::from_secs(5);
const MAX_NAME_BYTES: usize = 200;
const KEEP_ALIVE: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub tokens: Arc<HashMap<String, String>>,
    pub live: Topics,
    pub shutdown: CancellationToken,
    pub observers: Observers,
    /// Agents whose live `received` frames carry the event's `meta`.
    pub live_meta: Arc<HashSet<String>>,
    /// Agents that stream text and tool input on the live feed, which `/chat` needs.
    pub live_stream: Arc<HashSet<String>>,
    /// The runner's load, reported by `/healthz`.
    pub metrics: Arc<Metrics>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppState")
            .field("agents", &self.tokens.len())
            .field("observers", &self.observers)
            .finish_non_exhaustive()
    }
}

pub fn router(state: AppState) -> axum::Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route(
            "/v1/agents/{agent}/sessions/{key}",
            get(session_status).delete(delete_session),
        )
        .route("/v1/agents/{agent}/sessions/{key}/events", post(post_event))
        .route(
            "/v1/agents/{agent}/sessions/{key}/cancel",
            post(cancel_turn),
        )
        .route("/v1/agents/{agent}/sessions/{key}/chat", post(chat::chat))
        .route("/v1/agents/{agent}/sessions/{key}/live", get(live))
        .route(
            "/v1/agents/{agent}/sessions/{key}/history",
            get(fork::history),
        )
        .route("/v1/agents/{agent}/sessions/{key}/fork", post(fork::fork))
        .layer(DefaultBodyLimit::max(EVENT_BODY_LIMIT))
        .layer(axum::middleware::from_fn(json_errors))
        .with_state(state)
}

fn same_token(expected: &str, presented: &str) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    Sha256::digest(expected.as_bytes())
        .ct_eq(&Sha256::digest(presented.as_bytes()))
        .into()
}

fn authorize(
    state: &AppState,
    agent: &str,
    key: &str,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let presented = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let allowed = state
        .tokens
        .get(agent)
        .is_some_and(|expected| !expected.is_empty() && same_token(expected, presented));
    if !allowed {
        return Err(ApiError::Unauthorized);
    }
    if key.is_empty() || key.len() > MAX_NAME_BYTES || key.contains(['/', '\0']) {
        return Err(ApiError::Invalid(
            "session key must be 1 to 200 bytes and contain no slash or NUL",
        ));
    }
    Ok(())
}

async fn healthz(State(state): State<AppState>) -> Response {
    let ready = match sqlx::query("select 1").execute(state.store.pool()).await {
        Ok(_) => true,
        Err(error) => {
            tracing::error!(%error, "health check could not reach postgres");
            false
        }
    };
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let load = state.metrics.load();
    let body = json!({
        "ready": ready,
        "running": load.running,
        "waiting": load.waiting,
        "max_sessions": load.max_sessions,
        "oldest_wait_ms": load.oldest_wait_ms,
    });
    let mut response = (status, Json(body)).into_response();
    if !ready {
        response.headers_mut().insert(
            RETRY_AFTER,
            HeaderValue::from_static(error::RETRY_AFTER_SECONDS),
        );
    }
    response
}

async fn metrics(State(state): State<AppState>) -> Response {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics.render(),
    )
        .into_response()
}

async fn post_event(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    authorize(&state, &agent, &key, &headers)?;
    let event: InboundEvent =
        serde_json::from_slice(&body).map_err(|_| ApiError::Invalid("invalid event body"))?;
    let accepted = accept_event(&state, &agent, &key, &event).await?;
    Ok((StatusCode::ACCEPTED, Json(accepted)).into_response())
}

async fn accept_event(
    state: &AppState,
    agent: &str,
    key: &str,
    event: &InboundEvent,
) -> Result<Accepted, ApiError> {
    if event.id.trim().is_empty() || event.id.len() > MAX_NAME_BYTES {
        return Err(ApiError::Invalid("event id must be 1 to 200 bytes"));
    }
    if event.text.trim().is_empty() {
        return Err(ApiError::Invalid("an event needs text"));
    }
    input::event_without_nul(event)?;
    let started = std::time::Instant::now();
    let accepted = state
        .store
        .accept_event(
            agent,
            key,
            event,
            crate::store::LiveDetail {
                meta: state.live_meta.contains(agent),
                text: state.live_stream.contains(agent),
            },
        )
        .await
        .inspect_err(|error| match error {
            AcceptError::Full { .. } => state.metrics.count(Counter::EventsLimited),
            AcceptError::Conflict => state.metrics.count(Counter::EventsConflicted),
            AcceptError::Database(_) => {}
        })?;
    state.metrics.count(if accepted.duplicate {
        Counter::EventsDuplicate
    } else {
        Counter::EventsAccepted
    });
    if !accepted.duplicate && !state.observers.is_empty() {
        state.observers.server(ServerEventKind::EventAccepted {
            session: format!("{agent}/{key}"),
            inbox_id: accepted.inbox_id,
            accept_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            pool_wait_ms: accepted.pool_wait_ms,
        });
    }
    Ok(accepted)
}

async fn cancel_turn(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    authorize(&state, &agent, &key, &headers)?;
    let request: CancelBody = if body.is_empty() {
        CancelBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| ApiError::Invalid("the body must be {} or {\"turn\": \"<turn id>\"}"))?
    };
    if let Some(turn) = &request.turn {
        input::no_nul(turn)?;
    }
    match state
        .store
        .request_cancel(&agent, &key, request.turn.as_deref())
        .await?
    {
        CancelRequest::SessionNotFound => Err(ApiError::NotFound),
        CancelRequest::NotRunning => Err(ApiError::TurnNotRunning),
        CancelRequest::Requested(turn) => {
            Ok((StatusCode::ACCEPTED, Json(json!({ "turn": turn }))).into_response())
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelBody {
    turn: Option<String>,
}

async fn session_status(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&state, &agent, &key, &headers)?;
    let status = state
        .store
        .session_status(&agent, &key)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(status).into_response())
}

async fn delete_session(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &agent, &key, &headers)?;
    match state.store.delete_session(&agent, &key).await? {
        Deleted::Deleted => Ok(StatusCode::NO_CONTENT),
        Deleted::NotFound => Err(ApiError::NotFound),
        Deleted::Running => Err(ApiError::Conflict {
            code: "session_running",
            message: "the session is running a turn; try again when it is idle",
        }),
    }
}

#[derive(Debug, Deserialize)]
struct LiveQuery {
    cursor: Option<i64>,
}

async fn live(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    Query(query): Query<LiveQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    authorize(&state, &agent, &key, &headers)?;
    let resume_from = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let cursor = query.cursor.max(resume_from).unwrap_or(0);
    let gap = state.store.live_gap(&agent, &key, cursor).await?;
    let feed = LiveFeed {
        receiver: state.live.watch(Channel::Live, live_topic(&agent, &key)),
        store: state.store,
        agent,
        key,
        cursor,
        buffer: VecDeque::new(),
        must_read: true,
        seen_remote: 0,
        shutdown: state.shutdown,
        gap,
    };
    Ok(Sse::new(stream::unfold(feed, LiveFeed::next))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
}

struct LiveFeed {
    store: Store,
    agent: String,
    key: String,
    cursor: i64,
    receiver: Watch,
    buffer: VecDeque<LiveEvent>,
    /// Whether the database may hold frames the feed has not served: at the
    /// start, after a full page, after a wait ran out, or when memory does not
    /// hold them (see [`Watch::live_after`]). A short page has caught up.
    must_read: bool,
    /// The count of other instances' notifications already acted on.
    seen_remote: u64,
    shutdown: CancellationToken,
    gap: Option<(i64, i64)>,
}

impl LiveFeed {
    async fn next(mut self) -> Option<(Result<SseEvent, Infallible>, Self)> {
        if let Some((from, to)) = self.gap.take() {
            let frame = SseEvent::default()
                .data(json!({ "type": "gap", "from": from, "to": to }).to_string());
            return Some((Ok(frame), self));
        }
        let event = self.next_event().await?;
        let sse = SseEvent::default()
            .id(event.seq.to_string())
            .data(event.event.to_string());
        Some((Ok(sse), self))
    }

    async fn next_event(&mut self) -> Option<LiveEvent> {
        loop {
            if self.shutdown.is_cancelled() {
                return None;
            }
            if let Some(event) = self.buffer.pop_front() {
                self.cursor = event.seq;
                return Some(event);
            }
            let (held, must_read) = self.receiver.live_after(self.cursor, &mut self.seen_remote);
            self.must_read |= must_read;
            if !held.is_empty() {
                self.buffer.extend(held);
                continue;
            }
            if self.must_read {
                match self
                    .store
                    .live_since(&self.agent, &self.key, self.cursor, LIVE_PAGE)
                    .await
                {
                    Ok(events) if !events.is_empty() => {
                        self.must_read =
                            i64::try_from(events.len()).unwrap_or(i64::MAX) >= LIVE_PAGE;
                        self.buffer.extend(events);
                        continue;
                    }
                    Ok(_) => self.must_read = false,
                    Err(error) => {
                        tracing::error!(%error, "live feed could not read events, closing");
                        return None;
                    }
                }
            }
            if self.wait_for_topic().await {
                self.must_read = true;
            }
        }
    }

    /// Waits for a wake, and says whether the wait ran out instead: the feed
    /// then reads the database in case a notification was lost.
    async fn wait_for_topic(&mut self) -> bool {
        tokio::select! {
            () = tokio::time::sleep(LIVE_POLL) => true,
            () = self.shutdown.cancelled() => false,
            () = self.receiver.changed() => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::same_token;

    #[test]
    fn tokens_match_only_when_identical() {
        assert!(same_token("secret-token", "secret-token"));
        assert!(!same_token("secret-token", "secret-tokem"));
        assert!(!same_token("secret-token", "secret"));
        assert!(!same_token("secret-token", ""));
    }
}
