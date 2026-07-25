//! `POST /v1/messages` handler. Thin wrapper around the generic
//! ingress driver with `AnthropicIngress`.
//!
//! v0.6.0 collapsed per-ingress alias maps into the top-level
//! `[aliases]` table, so the ingress is alias-agnostic and stateless;
//! the router does all the alias resolution.
//!
//! See `crate::ingress::anthropic` for the body translation,
//! response rendering, and SSE state machine.

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::AppState;
use crate::server::request_id::RequestId;

#[tracing::instrument(skip_all, fields(ingress = "anthropic"))]
pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request_id: Option<Extension<RequestId>>,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let ingress = AnthropicIngress;
    ingress_handle(state, headers, request_id.map(|e| e.0), body, ingress).await
}
