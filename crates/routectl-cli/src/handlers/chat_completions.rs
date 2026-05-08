//! `POST /v1/chat/completions` handler. Thin wrapper around the
//! generic `ingress_handle` driver with `OpenAiIngress`. The ingress's
//! alias map comes from `AppState::openai_aliases` (loaded from
//! `[ingress.openai].aliases` in TOML).

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::openai::OpenAiIngress;
use crate::server::AppState;

#[tracing::instrument(skip_all, fields(ingress = "openai_compat"))]
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<axum::Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let ingress = OpenAiIngress::new(state.openai_aliases.clone());
    ingress_handle(state, headers, body, ingress).await
}
