//! `POST /v1/chat/completions` handler. Thin wrapper around the
//! generic `ingress_handle` driver with `OpenAiIngress`.
//!
//! All ingress-specific logic (request parsing, response/chunk
//! rendering, end-of-stream marker) lives in the adapter; this file
//! is just the route binding.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::openai::OpenAiIngress;
use crate::server::AppState;

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<axum::Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    ingress_handle(state, headers, body, OpenAiIngress).await
}
