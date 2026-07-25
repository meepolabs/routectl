//! `POST /v1/messages/count_tokens` handler.
//!
//! claude-code calls this endpoint to size context-window budgets
//! (and to render token counts in the UI). routectl historically
//! 404'd here; this handler proxies the request to the first
//! count_tokens-capable provider in the configured dispatch chain.
//!
//! Why proxy and not compute locally: the count_tokens result is
//! tokenizer-specific. Anthropic's tokenizer is not stable across
//! model versions and is not published as a public library. The
//! upstream's `/v1/messages/count_tokens` is the source of truth.
//!
//! Why walk to a capable provider (not strictly the first target): only
//! the anthropic-api egress kind implements count_tokens, and it is
//! Claude-only, so every capable target shares the same Anthropic
//! tokenizer family. `Router::count_tokens` skips count_tokens-incapable
//! KINDS (e.g. Bedrock, which has no count_tokens endpoint) before
//! dispatch, AND walks past a capable-by-kind seat that returns a
//! capability error at runtime -- a local NotImplemented or a wire 501
//! (e.g. an anthropic-api base_url that back-hops to a Bedrock egress).
//! It 501s only when no capable seat yields a count. Walking never
//! crosses tokenizer families, so a count served by a fallback still
//! reflects the caller's tokenizer. See `Router::count_tokens`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::handlers::ingress_handle::{map_error, render_json_rejection};
use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
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

    // Snapshot the live Router once. Hot-reload-safe: if a swap
    // happens between the snapshot and `count_tokens`, the request
    // still uses the snapshot's routing surface, not a half-applied
    // mix.
    let router = state.router.load_full();
    match router.count_tokens(req).await {
        Ok(tc) => {
            // Anthropic's wire shape is `{"input_tokens": N, ...}`.
            // `TokenCount` serializes back to that exact shape with
            // its `extras` flatten, so a Json wrap is sufficient.
            Json(tc).into_response()
        }
        Err(e) => map_error(envelope, e),
    }
}

#[cfg(test)]
#[path = "messages_count_tokens_tests.rs"]
mod tests;
