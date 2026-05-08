//! `POST /v1/messages` handler. Thin wrapper around the generic
//! ingress driver with `AnthropicIngress`. The ingress's alias map
//! comes from `AppState::anthropic_aliases` (loaded from
//! `[ingress.anthropic].aliases` in TOML).
//!
//! See `crate::ingress::anthropic` for the body translation,
//! response rendering, and SSE state machine.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::AppState;

#[tracing::instrument(skip_all, fields(ingress = "anthropic"))]
pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<axum::Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let ingress = AnthropicIngress::new(state.anthropic_aliases.clone());
    ingress_handle(state, headers, body, ingress).await
}
