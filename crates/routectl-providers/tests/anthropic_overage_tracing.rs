//! Tracing coverage for anthropic-api provider log-LEVEL contracts.
//!
//! Overage-flip log (drives `complete()` / `stream()`):
//!
//!   - A flip INTO overage emits exactly one WARN carrying the non-secret
//!     quota strings (provider, representative_claim, overage_status,
//!     utilization, overage_utilization, reset).
//!   - Steady state (overage -> overage) is silent: no second WARN.
//!   - A flip back OUT of overage emits exactly one INFO ("recovered").
//!
//! count_tokens capability-vs-health split (drives `count_tokens()`):
//!
//!   - A 501 (upstream does not implement count_tokens) logs at DEBUG as a
//!     capability note; it must NOT emit the shared "upstream error" WARN.
//!   - A non-501 error (e.g. 500) keeps the "upstream error" WARN.
//!
//! NEVER asserted / NEVER present: any token or credential value. The
//! unified-quota family carries only non-secret quota strings, so the log
//! fields are safe; this test pins that contract by asserting the exact
//! field set rather than dumping the body.
//!
//! A dedicated integration-test binary so the thread-local capture
//! subscriber (installed via `tracing::subscriber::set_default` on a
//! `current_thread` runtime) does not leak into the other anthropic-api
//! tests.

#![cfg(feature = "anthropic-api")]

use std::sync::Arc;

use routectl_core::Provider;
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_testkit::{CapturedEvent, with_capture};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn user_msg(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn base_req() -> ChatRequest {
    ChatRequest {
        model: "claude-3-opus".into(),
        messages: vec![user_msg("hi")],
        max_tokens: Some(2048),
        ..Default::default()
    }
}

fn make_provider(base_url: &str) -> AnthropicApiProvider {
    let cfg = AnthropicApiConfig {
        id: "overage-test".into(),
        auth: Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: base_url.to_string(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    AnthropicApiProvider::new(cfg)
}

fn response_body() -> serde_json::Value {
    json!({
        "id": "msg_overage",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3},
        "content": [{"type": "text", "text": "ok"}]
    })
}

/// Build a 200 response whose headers carry the given representative-claim
/// plus the supporting non-secret quota strings.
fn claim_response(claim: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(response_body())
        .append_header("anthropic-ratelimit-unified-status", "allowed")
        .append_header("anthropic-ratelimit-unified-overage-status", "allowed")
        .append_header("anthropic-ratelimit-unified-5h-utilization", "0.91")
        .append_header("anthropic-ratelimit-unified-overage-utilization", "0.05")
        .append_header("anthropic-ratelimit-unified-representative-claim", claim)
        .append_header("anthropic-ratelimit-unified-reset", "2026-06-09T12:00:00Z")
}

/// Mount a 200 response with the given representative-claim on every match.
async fn mount_with_claim(server: &MockServer, claim: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claim_response(claim))
        .mount(server)
        .await;
}

fn flip_events(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| {
            e.message
                .contains("anthropic subscription billing flipped to overage")
                || e.message
                    .contains("anthropic subscription billing recovered from overage")
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn flip_into_overage_warns_once_then_steady_state_is_silent() {
    // Arrange
    let server = MockServer::start().await;
    mount_with_claim(&server, "overage").await;
    let provider = make_provider(&server.uri());

    // Act: two requests, both reporting overage, under the capture
    // subscriber.
    let ((), events) = with_capture(async {
        provider.complete(base_req()).await.unwrap();
        provider.complete(base_req()).await.unwrap();
    })
    .await;

    // Assert: exactly ONE flip log (the entry into overage); the second
    // request is steady state and emits nothing.
    let flips = flip_events(&events);
    assert_eq!(
        flips.len(),
        1,
        "exactly one overage flip log expected, got: {events:#?}"
    );
    let warn = flips[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("provider"), Some("overage-test"));
    assert_eq!(warn.field("representative_claim"), Some("overage"));
    assert_eq!(warn.field("overage_status"), Some("allowed"));
    assert_eq!(warn.field("utilization"), Some("0.91"));
    assert_eq!(warn.field("overage_utilization"), Some("0.05"));
    assert_eq!(warn.field("reset"), Some("2026-06-09T12:00:00Z"));
    // No token/credential value may appear in any field.
    for ev in &events {
        for (_, v) in &ev.fields {
            assert!(
                !v.contains("test-key"),
                "credential leaked into event field: {ev:#?}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_out_of_overage_emits_info_once() {
    // Arrange: one server, one provider instance. The overage mock is
    // scoped to a single match (`up_to_n_times(1)`); the five_hour mock
    // serves every subsequent request. So request 1 observes overage
    // (one WARN) and request 2 observes five_hour and flips back OUT
    // (one INFO). The flip state lives on the provider instance, so
    // reusing the same provider across both requests is what exercises
    // the recovery.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claim_response("overage"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_with_claim(&server, "five_hour").await;
    let provider = make_provider(&server.uri());

    let ((), events) = with_capture(async {
        provider.complete(base_req()).await.unwrap();
        provider.complete(base_req()).await.unwrap();
    })
    .await;

    // Assert: one WARN (entry) then one INFO (recovery).
    let flips = flip_events(&events);
    assert_eq!(
        flips.len(),
        2,
        "expected one entry WARN + one recovery INFO, got: {events:#?}"
    );
    assert_eq!(flips[0].level, tracing::Level::WARN);
    assert_eq!(flips[1].level, tracing::Level::INFO);
    assert_eq!(flips[1].field("representative_claim"), Some("five_hour"));
}

/// Build a minimal SSE body carrying one text delta. The stream() path
/// yields at least one canonical chunk so the stream terminates cleanly.
fn stream_sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s01\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

/// Build a stream ResponseTemplate with unified headers carrying the given
/// representative-claim value plus the supporting quota strings.
fn stream_claim_response(claim: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_string(stream_sse_body())
        .append_header("content-type", "text/event-stream")
        .append_header("anthropic-ratelimit-unified-status", "allowed")
        .append_header("anthropic-ratelimit-unified-overage-status", "allowed")
        .append_header("anthropic-ratelimit-unified-5h-utilization", "0.91")
        .append_header("anthropic-ratelimit-unified-overage-utilization", "0.05")
        .append_header("anthropic-ratelimit-unified-representative-claim", claim)
        .append_header("anthropic-ratelimit-unified-reset", "2026-06-09T12:00:00Z")
}

#[tokio::test(flavor = "current_thread")]
async fn stream_path_flip_into_overage_warns_once() {
    use futures::StreamExt;

    // Arrange
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(stream_claim_response("overage"))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act: drive one stream to completion under the capture subscriber
    // so the provider observes the unified-quota headers and fires the
    // overage-flip WARN.
    let mut req = base_req();
    req.stream = Some(true);
    let ((), events) = with_capture(async {
        let mut stream = provider.stream(req).await.unwrap();
        while let Some(result) = stream.next().await {
            result.unwrap();
        }
    })
    .await;

    // Assert: exactly one overage-flip WARN.
    let flips = flip_events(&events);
    assert_eq!(
        flips.len(),
        1,
        "expected one overage flip WARN on stream path, got: {events:#?}"
    );
    assert_eq!(flips[0].level, tracing::Level::WARN);
    assert_eq!(flips[0].field("provider"), Some("overage-test"));
    assert_eq!(flips[0].field("representative_claim"), Some("overage"));
}

/// Drive a count_tokens request whose upstream returns `status` with an
/// Anthropic-shape error body, under the capture subscriber. Asserts the
/// upstream status is still surfaced to the router and returns the
/// captured events for log-level assertions.
async fn count_tokens_error_events(status: u16, message: &str) -> Vec<CapturedEvent> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "type": "error",
            "error": { "type": "some_error", "message": message }
        })))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    let (err, events) = with_capture(provider.count_tokens(base_req())).await;
    let err = err.unwrap_err();
    assert!(
        matches!(&err, routectl_core::Error::Upstream { status: got, .. } if *got == status),
        "count_tokens must still surface the upstream {status} to the router; got {err:?}",
    );
    events
}

#[tokio::test(flavor = "current_thread")]
async fn count_tokens_501_logs_capability_debug_not_health_warn() {
    // A 501 on count_tokens means the upstream does not implement the
    // endpoint -- a CAPABILITY signal the router handles by walking to the
    // next capable seat, not a health failure. It must log at DEBUG as a
    // capability note and must NOT emit the shared "upstream error" WARN
    // (which would flood operator logs on every client poll). "upstream
    // error" (WARN) is matched by EXACT message so it is not confused with
    // the distinct "upstream error body" DEBUG line on the same path.
    let events = count_tokens_error_events(501, "count_tokens not supported here").await;

    assert!(
        events.iter().any(|e| e.level == tracing::Level::DEBUG
            && e.message
                .contains("count_tokens unsupported by upstream (501)")),
        "expected the capability DEBUG note; got: {events:#?}",
    );
    assert!(
        !events
            .iter()
            .any(|e| e.level == tracing::Level::WARN && e.message == "upstream error"),
        "a capability 501 must NOT emit the health-failure WARN; got: {events:#?}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn count_tokens_500_still_logs_health_warn() {
    // Scope guard: a 500 is a genuine upstream health failure and must
    // keep the shared "upstream error" WARN. Only 501 is reclassified as a
    // capability signal.
    let events = count_tokens_error_events(500, "internal boom").await;

    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::WARN && e.message == "upstream error"),
        "a 500 health failure must keep the WARN; got: {events:#?}",
    );
    assert!(
        !events
            .iter()
            .any(|e| e.message.contains("count_tokens unsupported by upstream")),
        "the capability DEBUG note is 501-only; got: {events:#?}",
    );
}

// -----------------------------------------------------------------------
// Response-body cap trip WARN. A body over the 16 MiB non-stream cap
// emits exactly one WARN carrying the settled field set. http_client is
// crate-private, so the cap value is spelled literally here (it is a
// stability contract, not an implementation detail).
// -----------------------------------------------------------------------

const RESPONSE_BODY_CAP: usize = 16 * 1024 * 1024;

/// A body one byte over the cap. Honest wiremock Content-Length trips the
/// provider fast-reject.
fn over_cap_body() -> String {
    "a".repeat(RESPONSE_BODY_CAP + 1)
}

fn cap_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN
                && e.message
                    .contains("upstream response body exceeded cap; truncated")
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn complete_success_body_over_cap_warns_once_with_settled_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(over_cap_body()))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    let (result, events) = with_capture(provider.complete(base_req())).await;
    assert!(result.is_err(), "an over-cap 2xx must surface an error");

    let warns = cap_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "exactly one cap-trip WARN; got: {events:#?}"
    );
    let w = warns[0];
    assert_eq!(w.field("provider"), Some("overage-test"));
    assert_eq!(w.field("status"), Some("200"));
    assert_eq!(
        w.field("body_cap_bytes"),
        Some(RESPONSE_BODY_CAP.to_string().as_str())
    );
    assert_eq!(w.field("body_truncated"), Some("true"));
    assert_eq!(w.field("path"), Some("complete_success_body"));
    assert!(
        w.field("content_length")
            .is_some_and(|v| v.starts_with("Some(")),
        "honest Content-Length must be recorded: {:?}",
        w.field("content_length")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn complete_error_body_over_cap_warns_once_with_error_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string(over_cap_body()))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    let (result, events) = with_capture(provider.complete(base_req())).await;
    assert!(result.is_err(), "an over-cap >=400 must surface an error");

    let warns = cap_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "exactly one cap-trip WARN; got: {events:#?}"
    );
    let w = warns[0];
    assert_eq!(w.field("provider"), Some("overage-test"));
    assert_eq!(w.field("status"), Some("500"));
    assert_eq!(w.field("body_truncated"), Some("true"));
    assert_eq!(w.field("path"), Some("error_body"));
}

#[tokio::test(flavor = "current_thread")]
async fn count_tokens_success_body_over_cap_maps_to_502_and_warns() {
    // The count_tokens() success read shares the cap with complete() but
    // carries a distinct WARN path. An over-cap 2xx count_tokens body must
    // still map to a 502 and emit one cap-trip WARN tagged for this site.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_string(over_cap_body()))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    let (result, events) = with_capture(provider.count_tokens(base_req())).await;
    let err = result.unwrap_err();
    assert!(
        matches!(&err, routectl_core::Error::Upstream { status, .. } if *status == 502),
        "over-cap count_tokens 2xx must map to 502; got {err:?}",
    );

    let warns = cap_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "exactly one cap-trip WARN; got: {events:#?}"
    );
    let w = warns[0];
    assert_eq!(w.field("provider"), Some("overage-test"));
    assert_eq!(w.field("status"), Some("200"));
    assert_eq!(w.field("body_truncated"), Some("true"));
    assert_eq!(w.field("path"), Some("count_tokens_success_body"));
}
