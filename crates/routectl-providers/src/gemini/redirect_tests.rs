//! Gemini lane redirect behavior: a 3xx from the configured host is
//! surfaced as an upstream failure rather than followed to a different
//! host, so neither `x-goog-api-key` (direct) nor the Cloud Code
//! `Authorization: Bearer` can reach an unintended server.

use super::*;
use routectl_core::{ChatRequest, MessageContent};
use routectl_testkit::redirect_pin::CrossHostRedirect;

fn make_provider(base_url: &str) -> GeminiProvider {
    let mut cfg = GeminiConfig::new("gemini:test", "test-api-key");
    cfg.base_url = base_url.to_string();
    GeminiProvider::new(cfg)
}

fn base_req() -> ChatRequest {
    ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

/// Two separate mock servers stand in for two distinct hosts: the origin
/// answers the real request with a 302 pointing at the target, and the
/// target answers 200 -- so a followed hop would look like a successful
/// call. Only this lane's own no-redirect client keeps the target
/// untouched. Both auth modes share the single client built in
/// `GeminiProvider::new`, so the direct-key provider pins the policy for
/// the Cloud Code mode too.
#[tokio::test]
async fn lane_does_not_follow_cross_host_redirect() {
    let pin = CrossHostRedirect::start().await;

    let provider = make_provider(&pin.origin_uri());
    let err = provider.complete(base_req()).await.unwrap_err();

    pin.assert_not_followed(&err, "gemini").await;

    let origin_hits = pin.origin.received_requests().await.unwrap();
    assert_eq!(
        origin_hits[0]
            .headers
            .get("x-goog-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("test-api-key"),
        "the real request to the configured host must still carry x-goog-api-key"
    );
}
