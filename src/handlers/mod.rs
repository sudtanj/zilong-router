//! HTTP routes.

pub mod chat;
pub mod models;
pub mod responses;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use crate::state::AppState;
use crate::translator::response::error_body;
use crate::translator::Format;

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .expose_headers(Any);

    Router::new()
        .route("/api/health", get(health))
        .route("/v1", get(models::list_openai))
        .route("/v1/models", get(models::list_openai))
        .route("/v1/models/:id", get(models::get_openai))
        .route("/v1/chat/completions", post(chat::chat_completions))
        .route("/v1/messages", post(chat::messages))
        .route("/v1/responses", post(responses::responses))
        .route("/v1/messages/count_tokens", post(chat::count_tokens))
        // Tools that hard-code a doubled prefix (`/v1/v1/...`) still work, like upstream.
        .route("/v1/v1/chat/completions", post(chat::chat_completions))
        .route("/v1/v1/messages", post(chat::messages))
        .route("/v1/v1/responses", post(responses::responses))
        .route("/mcp", post(crate::mcp::server::meta).get(crate::mcp::server::not_allowed))
        .route("/mcp/all", post(crate::mcp::server::all).get(crate::mcp::server::not_allowed))
        .merge(crate::mcp::oauth_server::routes())
        .merge(crate::dashboard::routes(state.clone()))
        .layer(DefaultBodyLimit::max(state.cfg.max_body_bytes))
        .layer(cors)
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSec": state.started_at.elapsed().as_secs(),
        "providers": state.registry.all().len(),
        "upstream": state.registry.meta,
    }))
}

/// A client-facing error in the dialect of the client's API.
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub format: Format,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(status: u16, format: Format, message: impl Into<String>) -> ApiError {
        ApiError {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message: message.into(),
            format,
            retry_after: None,
        }
    }
    pub fn retry_after(mut self, secs: u64) -> ApiError {
        self.retry_after = Some(secs);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body: Value = error_body(self.format, self.status.as_u16(), &self.message);
        let mut resp = Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        if let Some(s) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&s.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}
