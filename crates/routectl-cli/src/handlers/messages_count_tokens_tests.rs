//! Pre-change rejection-contract + forward-compat extras pins for the
//! `/v1/messages/count_tokens` handler. Loaded via
//! `#[cfg(test)] #[path = "messages_count_tokens_tests.rs"] mod tests;`
//! from `messages_count_tokens.rs`.
//!
//! count_tokens is the one inference endpoint that does NOT funnel through
//! `ingress_handle`; it renders its own JSON-rejection / parse-error
//! responses via the shared `render_json_rejection` / `map_error` helpers.
//! The rejection test drives the REAL handler mounted behind the same
//! `DefaultBodyLimit` layer `server::serve::build_axum_router` installs,
//! so the axum `Json` extractor and the body-size layer are exercised
//! exactly as in production. The extras test pins that an unknown
//! top-level field round-trips into `provider_extras` (Anthropic dialect).

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::AppState;

const REJECT_BODY_LIMIT: usize = 1024;

fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let router = Arc::new(routectl_router::Router::new(Arc::new(
        routectl_router::Config::default(),
    )));
    let swap = Arc::new(arc_swap::ArcSwap::from(router));
    AppState::for_test(swap)
}

fn app(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/v1/messages/count_tokens", post(super::count_tokens))
        .layer(DefaultBodyLimit::max(REJECT_BODY_LIMIT))
        .with_state(state)
}

fn post_req(content_type: Option<&str>, body: impl Into<Body>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens");
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    builder.body(body.into()).expect("request builds")
}

fn oversized_body() -> String {
    format!(
        "{{\"model\":\"m\",\"pad\":\"{}\"}}",
        "a".repeat(REJECT_BODY_LIMIT * 2)
    )
}

async fn body_to_value(resp: Response) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn drive(state: Arc<AppState>, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app(state).oneshot(req).await.expect("router is infallible");
    let status = resp.status();
    (status, body_to_value(resp).await)
}

/// count_tokens is Anthropic-dialect: the routectl-owned envelope shape and
/// classifier are pinned byte-for-byte; the human message string is
/// axum-owned today and spliced back in so the pin does not force a later
/// hand-rolled renderer to reproduce axum's exact wording.
fn assert_anthropic_reject(status: StatusCode, body: &Value, expected: StatusCode) {
    assert_eq!(status, expected, "count_tokens rejection status");
    let msg = body["error"]["message"]
        .as_str()
        .expect("anthropic envelope carries error.message string");
    assert!(!msg.is_empty(), "rejection message must be non-empty");
    assert_eq!(
        *body,
        json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": msg }
        }),
        "exact Anthropic rejection envelope"
    );
}

#[tokio::test]
async fn count_tokens_rejection_contract_pins_status_and_envelope() {
    // JSON syntax error -> 400 + Anthropic envelope.
    let (state, _dir) = test_state();
    let (status, body) = drive(
        state,
        post_req(Some("application/json"), "{ not valid json"),
    )
    .await;
    assert_anthropic_reject(status, &body, StatusCode::BAD_REQUEST);

    // Wrong content-type -> 415.
    let (state, _dir) = test_state();
    let (status, body) = drive(state, post_req(Some("text/plain"), "{}")).await;
    assert_anthropic_reject(status, &body, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Oversized body -> 413 (DefaultBodyLimit layer).
    let (state, _dir) = test_state();
    let (status, body) = drive(state, post_req(Some("application/json"), oversized_body())).await;
    assert_anthropic_reject(status, &body, StatusCode::PAYLOAD_TOO_LARGE);
}

#[test]
fn count_tokens_preserves_unknown_top_level_field_into_provider_extras() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "future_unknown_knob": {"nested": [1, 2, 3]}
    });
    let req = AnthropicIngress
        .parse_request(&HeaderMap::new(), body)
        .expect("valid Anthropic body parses");
    let extras = req
        .provider_extras
        .expect("unknown top-level field must round-trip into provider_extras");
    assert_eq!(extras["future_unknown_knob"], json!({"nested": [1, 2, 3]}));
}
