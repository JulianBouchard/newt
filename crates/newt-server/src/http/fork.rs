//! `GET .../history` and `POST .../fork`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, AppState, authorize, input};
use crate::fork::{Fork, ForkError};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkRequest {
    into: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    through_turn: Option<String>,
}

pub(super) async fn history(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&state, &agent, &key, &headers)?;
    let entries = crate::fork::history(state.store.pool(), &agent, &key)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(json!({ "entries": entries })).into_response())
}

pub(super) async fn fork(
    State(state): State<AppState>,
    Path((agent, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    authorize(&state, &agent, &key, &headers)?;
    let request: ForkRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::Invalid("body must be { into, agent?, through_turn? }"))?;
    let into_agent = request.agent.as_deref().unwrap_or(&agent);
    authorize(&state, into_agent, &request.into, &headers)?;
    if let Some(turn) = &request.through_turn {
        input::no_nul(turn)?;
    }
    let copied = crate::fork::fork(
        state.store.pool(),
        Fork {
            agent: &agent,
            key: &key,
            into_agent,
            into_key: &request.into,
            through_turn: request.through_turn.as_deref(),
        },
    )
    .await
    .map_err(|error| match error {
        ForkError::NoSession => ApiError::NotFound,
        ForkError::NoTurn => ApiError::Invalid("the session has no turn with that id"),
        ForkError::Exists => ApiError::Conflict {
            code: "session_exists",
            message: "the target session already exists",
        },
        ForkError::Database(error) => ApiError::Internal(error),
    })?;
    tracing::info!(agent, key, into_agent, into = %request.into, copied, "forked a session");
    Ok((
        StatusCode::CREATED,
        Json(json!({ "agent": into_agent, "key": request.into, "history_entries": copied })),
    )
        .into_response())
}
