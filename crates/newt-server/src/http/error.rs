//! The one JSON shape every error of the HTTP API has:
//! `{"error": {"code": "...", "message": "..."}}`.

use axum::Json;
use axum::extract::Request;
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::store::AcceptError;

pub(super) const RETRY_AFTER_SECONDS: &str = "2";

#[derive(Debug)]
pub(super) enum ApiError {
    Unauthorized,
    NotFound,
    TurnNotRunning,
    Invalid(&'static str),
    Conflict {
        code: &'static str,
        message: &'static str,
    },
    InboxFull,
    Internal(sqlx::Error),
}

pub(super) fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let mut response = (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response();
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_SECONDS));
    }
    response
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or wrong token",
            ),
            Self::NotFound => error_response(StatusCode::NOT_FOUND, "not_found", "no such session"),
            Self::TurnNotRunning => error_response(
                StatusCode::CONFLICT,
                "turn_not_running",
                "the session is not running that turn",
            ),
            Self::Invalid(message) => {
                error_response(StatusCode::BAD_REQUEST, "invalid_request", message)
            }
            Self::Conflict { code, message } => error_response(StatusCode::CONFLICT, code, message),
            Self::InboxFull => error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "inbox_full",
                "the session holds too many unconsumed events; retry shortly",
            ),
            Self::Internal(error) => {
                tracing::error!(%error, "request failed");
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal error",
                )
            }
        }
    }
}

impl From<AcceptError> for ApiError {
    fn from(error: AcceptError) -> Self {
        match error {
            AcceptError::Full { .. } => Self::InboxFull,
            AcceptError::Conflict => Self::Conflict {
                code: "event_id_conflict",
                message: "an event with this id and different content was already accepted",
            },
            AcceptError::Database(error) => Self::Internal(error),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error)
    }
}

fn code_for(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "invalid_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        status if status.is_server_error() => "internal",
        _ => "error",
    }
}

/// Gives the errors the framework produces itself (unknown route, wrong
/// method, body too large) the same JSON shape as the handlers' own.
pub(super) async fn json_errors(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if (status.is_client_error() || status.is_server_error()) && !is_json {
        return error_response(
            status,
            code_for(status),
            status.canonical_reason().unwrap_or("error"),
        );
    }
    response
}
