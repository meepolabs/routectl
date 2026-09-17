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
//! Why walk to a capable provider (not strictly the first target): the
//! walk admits only seats that count in the caller's own Anthropic
//! tokenizer family, so a count served by a fallback still reflects the
//! caller's tokenizer. `Router::count_tokens` decides capability per
//! seat from its egress kind and upstream model id
//! (`seat_can_count_tokens`): `anthropic-api` is admitted unconditionally
//! (Claude-only), and a `bedrock` seat only when its upstream model id is
//! an Anthropic-family id -- a non-Anthropic Bedrock model, or an opaque
//! inference-profile ARN, is skipped before dispatch. It also walks past
//! a capable-by-kind seat that returns a capability error at runtime -- a
//! local NotImplemented or a wire 501 (e.g. a remote anthropic-api
//! base_url whose own upstream cannot count). It 501s only when no
//! capable seat yields a count. See `Router::count_tokens`.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::handlers::ingress_handle::{
    is_json_content_type, map_error, render_body_rejection, render_malformed_body,
    render_unsupported_media_type,
};
use crate::handlers::usage_capture::drain_capability_events;
use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::AppState;

#[tracing::instrument(skip_all, fields(ingress = "anthropic", op = "count_tokens"))]
pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let adapter = AnthropicIngress;
    let envelope = adapter.error_envelope_shape();
    // `Bytes` + `DefaultBodyLimit` surfaces an oversized body as a 413
    // rejection (untouched); content-type is enforced explicitly since
    // `Bytes` does not gate on it, and a top-level JSON syntax failure
    // maps to `Error::Json` below. Mirrors `ingress_handle`; count_tokens
    // is the one inference endpoint that does not funnel through it.
    let raw_body = match body {
        Ok(b) => b,
        Err(rej) => return render_body_rejection(envelope, rej),
    };
    if !is_json_content_type(&headers) {
        return render_unsupported_media_type(envelope);
    }

    let req = match adapter.parse_request(&headers, raw_body.as_ref()) {
        Ok(r) => r,
        Err(routectl_core::Error::Json(_)) => return render_malformed_body(envelope),
        Err(e) => return map_error(envelope, e),
    };

    // Compiled-pin drift observation, after a successful parse and before
    // dispatch. count_tokens is the one inference endpoint that does not
    // funnel through `ingress_handle`, so without this call a base-url
    // client sizing its context window would produce no drift signal.
    state.cc_pin_drift.observe_headers(&headers);

    // Snapshot the live Router once. Hot-reload-safe: if a swap
    // happens between the snapshot and `count_tokens`, the request
    // still uses the snapshot's routing surface, not a half-applied
    // mix.
    let router = state.router.load_full();
    // `count_tokens_with_meta`, not `count_tokens`: this walk can settle an
    // envelope-field verdict, and a settlement's event row has to reach the
    // capability-event ledger. Dropping the meta would leave the shared
    // learned registry mutated with no ledger record of it -- the state a warm
    // rebuild resurrects a cleared verdict from. The drain is the SAME mapping
    // the messages handlers use (no usage row is recorded here: this endpoint
    // has none, and the capability rows do not need one).
    let counted = router.count_tokens_with_meta(req).await;
    drain_capability_events(
        &state.usage,
        &counted.meta,
        router.catalog_version(),
        router.overlay_revision(),
    );
    match counted.result {
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
