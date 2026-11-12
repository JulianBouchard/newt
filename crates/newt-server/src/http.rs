use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

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

use crate::store::{InboundEvent, InboundKind, LiveEvent, Store, live_topic};

const LIVE_PAGE: i64 = 100;
const LIVE_POLL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct AppState {
    pub store: Store,
    pub tokens: Arc<HashMap<String, String>>,
    pub live: broadcast::Sender<String>,
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
    UnknownAgent,
    Unauthorized,
    NotFound,
    Invalid(&'static str),
    Internal(sqlx::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::UnknownAgent => (StatusCode::NOT_FOUND, "unknown agent"),
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

fn authorize(state: &AppState, agent: &str, headers: &HeaderMap) -> Result<(), ApiError> {
    let expected = state.tokens.get(agent).ok_or(ApiError::UnknownAgent)?;
    let presented = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
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
    Json(event): Json<InboundEvent>,
) -> Result<Response, ApiError> {
    authorize(&state, &agent, &headers)?;
    if event.id.trim().is_empty() {
        return Err(ApiError::Invalid("event id must not be empty"));
    }
    if event.kind == InboundKind::Message && event.text.trim().is_empty() {
        return Err(ApiError::Invalid("a message event needs text"));
    }
    let accepted = state.store.accept_event(&agent, &key, &event).await?;
    Ok((StatusCode::ACCEPTED, Json(accepted)).into_response())
}

async fn session_status(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&state, &agent, &headers)?;
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
    authorize(&state, &agent, &headers)?;
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
        cursor: query.cursor.or(resume_from).unwrap_or(0),
        buffer: VecDeque::new(),
    };
    Ok(Sse::new(stream::unfold(feed, LiveFeed::next)).keep_alive(KeepAlive::default()))
}

struct LiveFeed {
    store: Store,
    agent: String,
    key: String,
    topic: String,
    cursor: i64,
    receiver: broadcast::Receiver<String>,
    buffer: VecDeque<LiveEvent>,
}

impl LiveFeed {
    async fn next(mut self) -> Option<(Result<SseEvent, Infallible>, Self)> {
        loop {
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
                received = self.receiver.recv() => match received {
                    Ok(topic) if topic != self.topic => {}
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
