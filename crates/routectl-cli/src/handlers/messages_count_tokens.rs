//! `POST /v1/messages/count_tokens` handler.
//!
//! claude-code calls this endpoint to size context-window budgets
//! (and to render token counts in the UI). routectl historically
//! 404'd here; this handler proxies the request to the FIRST
//! provider in the configured dispatch chain.
//!
//! Why proxy and not compute locally: the count_tokens result is
//! tokenizer-specific. Anthropic's tokenizer is not stable across
//! model versions and is not published as a public library. The
//! upstream's `/v1/messages/count_tokens` is the source of truth.
//!
//! Why first-only (no fallback chain walk): falling back to a
//! different model would return tokens for the WRONG tokenizer and
//! silently miscount the caller's budget. See
//! `Router::count_tokens`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::handlers::ingress_handle::{map_error, render_json_rejection};
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::IngressAdapter;
use crate::server::AppState;

#[tracing::instrument(skip_all, fields(ingress = "anthropic", op = "count_tokens"))]
pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let adapter = AnthropicIngress;
    let envelope = adapter.error_envelope_shape();
    let Json(raw_body) = match body {
        Ok(b) => b,
        Err(e) => {
            // JsonRejection surfaces 413 / 415 / 400 as appropriate;
            // shared helper preserves the status code and renders the
            // dialect-correct envelope. See `ingress_handle` for the
            // matching call site on /v1/messages.
            return render_json_rejection(envelope, e);
        }
    };

    let req = match adapter.parse_request(&headers, raw_body) {
        Ok(r) => r,
        Err(e) => return map_error(envelope, e),
    };

    match state.router.count_tokens(req).await {
        Ok(tc) => {
            // Anthropic's wire shape is `{"input_tokens": N, ...}`.
            // `TokenCount` serializes back to that exact shape with
            // its `extras` flatten, so a Json wrap is sufficient.
            Json(tc).into_response()
        }
        Err(e) => map_error(envelope, e),
    }
}
