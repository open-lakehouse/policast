//! Axum server for the playground: a `POST /api/query` endpoint and a
//! self-contained SPA served at `/`.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use crate::engine::{PlaygroundRequest, PlaygroundService, QueryResult};
use crate::PlaygroundError;

/// The single-page playground UI, embedded so the binary is self-contained.
const INDEX_HTML: &str = include_str!("../static/index.html");

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    pub service: Arc<PlaygroundService>,
}

/// Build the playground router.
pub fn router(service: PlaygroundService) -> Router {
    let state = AppState {
        service: Arc::new(service),
    };
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/query", post(query))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn query(
    State(state): State<AppState>,
    Json(req): Json<PlaygroundRequest>,
) -> Result<Json<QueryResult>, ApiError> {
    let result = state.service.run(&req).await?;
    Ok(Json(result))
}

/// Wire error → JSON `{ "error": "..." }` with a sensible status code.
pub struct ApiError(PlaygroundError);

impl From<PlaygroundError> for ApiError {
    fn from(e: PlaygroundError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            // User-input problems (bad SQL, bad policy, bad function).
            PlaygroundError::Compile(_)
            | PlaygroundError::Invalid(_)
            | PlaygroundError::Function { .. }
            | PlaygroundError::Query(_) => StatusCode::BAD_REQUEST,
            // Upstream / infra problems.
            PlaygroundError::Uc(_) | PlaygroundError::Delta(_) => StatusCode::BAD_GATEWAY,
        };
        (status, Json(json!({ "error": self.0.to_string() }))).into_response()
    }
}
