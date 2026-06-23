//! Tracing coverage for the anthropic-api overage-flip log. Drives the
//! provider's `complete()` path under a captured `tracing` subscriber and
//! asserts the once-per-flip contract:
//!
//!   - A flip INTO overage emits exactly one WARN carrying the non-secret
//!     quota strings (provider, representative_claim, overage_status,
//!     utilization, overage_utilization, reset).
//!   - Steady state (overage -> overage) is silent: no second WARN.
//!   - A flip back OUT of overage emits exactly one INFO ("recovered").
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

use std::sync::{Arc, Mutex};

use routectl_core::Provider;
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use serde_json::json;
use tracing::field::{Field, Visit};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: tracing::Level,
    message: String,
    fields: Vec<(String, String)>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().into(), value.into()));
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s.trim_matches('"').to_string();
        } else {
            self.fields.push((field.name().into(), s));
        }
    }
}

#[derive(Default)]
struct CaptureSubscriber {
    captured: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let meta = event.metadata();
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);
        let captured = CapturedEvent {
            level: *meta.level(),
            message: visitor.message,
            fields: visitor.fields,
        };
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(captured);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

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
    // Arrange: capture subscriber active for this thread.
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let _guard = tracing::subscriber::set_default(CaptureSubscriber {
        captured: captured.clone(),
    });
    let server = MockServer::start().await;
    mount_with_claim(&server, "overage").await;
    let provider = make_provider(&server.uri());

    // Act: two requests, both reporting overage.
    provider.complete(base_req()).await.unwrap();
    provider.complete(base_req()).await.unwrap();

    // Assert: exactly ONE flip log (the entry into overage); the second
    // request is steady state and emits nothing.
    let events = captured.lock().unwrap().clone();
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
    // Arrange
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let _guard = tracing::subscriber::set_default(CaptureSubscriber {
        captured: captured.clone(),
    });

    // One server, one provider instance. The overage mock is scoped to a
    // single match (`up_to_n_times(1)`); the five_hour mock serves every
    // subsequent request. So request 1 observes overage (one WARN) and
    // request 2 observes five_hour and flips back OUT (one INFO). The flip
    // state lives on the provider instance, so reusing the same provider
    // across both requests is what exercises the recovery.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claim_response("overage"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_with_claim(&server, "five_hour").await;
    let provider = make_provider(&server.uri());

    provider.complete(base_req()).await.unwrap();
    provider.complete(base_req()).await.unwrap();

    // Assert: one WARN (entry) then one INFO (recovery).
    let events = captured.lock().unwrap().clone();
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

    // Arrange: capture subscriber active for this thread.
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let _guard = tracing::subscriber::set_default(CaptureSubscriber {
        captured: captured.clone(),
    });
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(stream_claim_response("overage"))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act: drive one stream to completion so the provider observes the
    // unified-quota headers and fires the overage-flip WARN.
    let mut req = base_req();
    req.stream = Some(true);
    let mut stream = provider.stream(req).await.unwrap();
    while let Some(result) = stream.next().await {
        result.unwrap();
    }

    // Assert: exactly one overage-flip WARN.
    let events = captured.lock().unwrap().clone();
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
