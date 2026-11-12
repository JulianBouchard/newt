use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use sqlx::PgPool;

pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(pool)
}

async fn healthz(State(pool): State<PgPool>) -> StatusCode {
    match sqlx::query("select 1").execute(&pool).await {
        Ok(_) => StatusCode::OK,
        Err(error) => {
            tracing::error!(%error, "health check could not reach postgres");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}
