use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use futures_util::stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::live::{Channel, Notification};
use crate::store::{InboundEvent, LiveEvent, Store, live_topic};

const LIVE_PAGE: i64 = 100;
const LIVE_POLL: Duration = Duration::from_secs(5);
const MAX_NAME_BYTES: usize = 200;

#[derive(Debug, Clone)]
pub struct AppState {
    pub store: Store,
    pub tokens: Arc<HashMap<String, String>>,
    pub live: broadcast::Sender<Notification>,
    pub shutdown: CancellationToken,
}

pub fn router(state: AppState) -> axum::Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/agents/{agent}/sessions/{key}", get(session_status))
        .route("/v1/agents/{agent}/sessions/{key}/events", post(post_event))
        .route("/v1/agents/{agent}/sessions/{key}/live", get(live))
        .with_state(state)
}

#[derive(Debug)]
enum ApiError {
    Unauthorized,
    NotFound,
    Invalid(&'static str),
    Internal(sqlx::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "missing or wrong token"),
            Self::NotFound => (StatusCode::NOT_FOUND, "no such session"),
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, message),
            Self::Internal(error) => {
                tracing::error!(%error, "request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error)
    }
}

fn same_token(expected: &str, presented: &str) -> bool {
    expected.len() == presented.len()
        && expected
            .bytes()
            .zip(presented.bytes())
            .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
            == 0
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
    if key.is_empty() || key.len() > MAX_NAME_BYTES || key.contains('/') {
        return Err(ApiError::Invalid(
            "session key must be 1 to 200 bytes and contain no slash",
        ));
    }
    Ok(())
}

async fn healthz(State(state): State<AppState>) -> StatusCode {
    match sqlx::query("select 1").execute(state.store.pool()).await {
        Ok(_) => StatusCode::OK,
        Err(error) => {
            tracing::error!(%error, "health check could not reach postgres");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
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
    if event.id.trim().is_empty() || event.id.len() > MAX_NAME_BYTES {
        return Err(ApiError::Invalid("event id must be 1 to 200 bytes"));
    }
    if event.text.trim().is_empty() {
        return Err(ApiError::Invalid("an event needs text"));
    }
    let accepted = state.store.accept_event(&agent, &key, &event).await?;
    Ok((StatusCode::ACCEPTED, Json(accepted)).into_response())
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
    let feed = LiveFeed {
        topic: live_topic(&agent, &key),
        receiver: state.live.subscribe(),
        store: state.store,
        agent,
        key,
        cursor: query.cursor.max(resume_from).unwrap_or(0),
        buffer: VecDeque::new(),
        shutdown: state.shutdown,
    };
    Ok(Sse::new(stream::unfold(feed, LiveFeed::next)).keep_alive(KeepAlive::default()))
}

struct LiveFeed {
    store: Store,
    agent: String,
    key: String,
    topic: String,
    cursor: i64,
    receiver: broadcast::Receiver<Notification>,
    buffer: VecDeque<LiveEvent>,
    shutdown: CancellationToken,
}

impl LiveFeed {
    async fn next(mut self) -> Option<(Result<SseEvent, Infallible>, Self)> {
        loop {
            if self.shutdown.is_cancelled() {
                return None;
            }
            if let Some(event) = self.buffer.pop_front() {
                self.cursor = event.seq;
                let sse = SseEvent::default()
                    .id(event.seq.to_string())
                    .data(event.event.to_string());
                return Some((Ok(sse), self));
            }
            match self
                .store
                .live_since(&self.agent, &self.key, self.cursor, LIVE_PAGE)
                .await
            {
                Ok(events) if !events.is_empty() => {
                    self.buffer.extend(events);
                    continue;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(%error, "live feed could not read events, closing");
                    return None;
                }
            }
            self.wait_for_topic().await;
        }
    }

    async fn wait_for_topic(&mut self) {
        let deadline = tokio::time::sleep(LIVE_POLL);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return,
                () = self.shutdown.cancelled() => return,
                received = self.receiver.recv() => match received {
                    Ok(notification)
                        if notification.channel != Channel::Live || notification.topic != self.topic => {}
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => return,
                    Err(broadcast::error::RecvError::Closed) => {
                        deadline.as_mut().await;
                        return;
                    }
                },
            }
        }
    }
}
