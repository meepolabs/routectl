//! Integration tests for the context_management emulation layer.
//!
//! Covers:
//!   - build_headers: context-management beta is stripped when
//!     context_management=true and forwarded when context_management=false.
//!   - E2E complete(): thinking injection + context_management body key strip
//!     + beta header strip together in a single round-trip against wiremock.

#![cfg(feature = "anthropic-api")]

// Local const mirrors: the canonical definitions live in
// routectl_providers::anthropic_api::context_management but that module is
// pub(crate). Redeclaring them here keeps all the hardcoded literals out of
// the test bodies while satisfying the "zero bare-literal sites" criterion.
const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
// Used only by the test-utils-gated E2E test below; the unconditional header
// tests do not reference it, so the constant is dead code without the feature.
#[cfg_attr(not(feature = "test-utils"), allow(dead_code))]
const CLEAR_THINKING_EDIT_TYPE: &str = "clear_thinking_20251015";

use routectl_core::{Message, MessageContent, Provider, Role};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn ok_response_body() -> serde_json::Value {
    json!({
        "id": "msg_cm_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "content": [{"type": "text", "text": "ok"}]
    })
}

fn make_provider(base_url: &str, context_management: bool) -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "cm-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: base_url.to_string(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
    })
}

fn user_msg(text: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

// -----------------------------------------------------------------------
// Test 13: build_headers strips context-management-2025-06-27 when
// context_management=true.
// -----------------------------------------------------------------------

#[tokio::test]
async fn build_headers_strips_context_management_beta_when_flag_true() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_body()))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri(), true);
    let req = routectl_core::ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![user_msg("hi")],
        anthropic_beta: vec![
            CONTEXT_MANAGEMENT_BETA.to_string(),
            "prompt-caching-2024-07-31".to_string(),
        ],
        ..Default::default()
    };

    provider.complete(req).await.expect("complete must succeed");

    let received = mock_server
        .received_requests()
        .await
        .expect("wiremock must capture requests");
    assert_eq!(received.len(), 1);

    let beta_header = received[0]
        .headers
        .get("anthropic-beta")
        .expect("anthropic-beta header must be present");
    let beta_value = beta_header
        .to_str()
        .expect("anthropic-beta must be valid UTF-8");

    assert!(
        !beta_value.contains(CONTEXT_MANAGEMENT_BETA),
        "context-management beta must be stripped when context_management=true; \
         got `{beta_value}`"
    );
    assert!(
        beta_value.contains("prompt-caching-2024-07-31"),
        "other betas must survive the strip; got `{beta_value}`"
    );
}

// -----------------------------------------------------------------------
// Test 14: build_headers forwards context-management-2025-06-27 when
// context_management=false.
// -----------------------------------------------------------------------

#[tokio::test]
async fn build_headers_keeps_context_management_beta_when_flag_false() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_body()))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri(), false);
    let req = routectl_core::ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![user_msg("hi")],
        anthropic_beta: vec![CONTEXT_MANAGEMENT_BETA.to_string()],
        ..Default::default()
    };

    provider.complete(req).await.expect("complete must succeed");

    let received = mock_server
        .received_requests()
        .await
        .expect("wiremock must capture requests");
    assert_eq!(received.len(), 1);

    let beta_header = received[0]
        .headers
        .get("anthropic-beta")
        .expect("anthropic-beta header must be present");
    let beta_value = beta_header
        .to_str()
        .expect("anthropic-beta must be valid UTF-8");

    assert!(
        beta_value.contains(CONTEXT_MANAGEMENT_BETA),
        "context-management beta must be forwarded when context_management=false; \
         got `{beta_value}`"
    );
}

// -----------------------------------------------------------------------
// Test 15: E2E -- context_management=true provider, populated cache.
// Verifies three invariants in a single complete() call:
//   1. `context_management` body key is stripped.
//   2. `context-management-2025-06-27` is absent from anthropic-beta header.
//   3. Injected Thinking block precedes ToolUse in outgoing assistant message.
// -----------------------------------------------------------------------

#[cfg(feature = "test-utils")]
#[tokio::test]
async fn e2e_context_management_strips_key_strips_beta_and_injects_thinking() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_body()))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri(), true);

    // Seed the cache for the tool_use id used in the request below.
    provider.seed_thinking_for_test(
        "cm-test",
        "toolu_t1",
        vec![routectl_core::ReasoningDetail {
            kind: routectl_core::ReasoningDetailKind::Text,
            id: Some("rd-e2e".into()),
            format: Some("anthropic-claude-v1".into()),
            index: Some(0),
            payload: json!({"text": "injected-thinking", "signature": "sig-e2e"}),
        }],
    );

    let req = routectl_core::ChatRequest {
        model: "claude-sonnet-4".into(),
        max_tokens: Some(4096),
        messages: vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("use calc".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("calling calc".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "toolu_t1",
                    "type": "function",
                    "function": {"name": "calc", "arguments": "{}"}
                })]),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::Text("42".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("toolu_t1".into()),
                tool_calls: None,
            },
        ],
        anthropic_beta: vec![
            CONTEXT_MANAGEMENT_BETA.to_string(),
            "prompt-caching-2024-07-31".to_string(),
        ],
        provider_extras: Some(json!({
            "context_management": {
                "edits": [{"type": (CLEAR_THINKING_EDIT_TYPE), "keep": "all"}]
            }
        })),
        ..Default::default()
    };

    provider.complete(req).await.expect("complete must succeed");

    let received = mock_server
        .received_requests()
        .await
        .expect("wiremock must capture requests");
    assert_eq!(received.len(), 1, "expected exactly one outbound request");

    // ----- Invariant 1: context_management body key is stripped -----
    let body: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("body must be valid JSON");
    assert!(
        body.get("context_management").is_none(),
        "context_management body key must be stripped; got keys: {:?}",
        body.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );

    // ----- Invariant 2: context-management-2025-06-27 absent from header -----
    let beta_header = received[0]
        .headers
        .get("anthropic-beta")
        .expect("anthropic-beta header must be present");
    let beta_value = beta_header
        .to_str()
        .expect("anthropic-beta must be valid UTF-8");
    assert!(
        !beta_value.contains(CONTEXT_MANAGEMENT_BETA),
        "context-management beta must be stripped from header; got `{beta_value}`"
    );
    assert!(
        beta_value.contains("prompt-caching-2024-07-31"),
        "other betas must survive; got `{beta_value}`"
    );

    // ----- Invariant 3: Thinking block injected before ToolUse -----
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages must be an array");
    let assistant_msg = messages
        .iter()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        .expect("assistant message must be present");
    let blocks = assistant_msg
        .get("content")
        .and_then(|v| v.as_array())
        .expect("assistant content must be an array of blocks");

    // Find the index of Thinking and ToolUse blocks.
    let thinking_idx = blocks
        .iter()
        .position(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
        .expect("Thinking block must be present in assistant message");
    let tool_use_idx = blocks
        .iter()
        .position(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
        .expect("ToolUse block must be present in assistant message");

    assert!(
        thinking_idx < tool_use_idx,
        "Thinking block (idx {thinking_idx}) must precede ToolUse block (idx {tool_use_idx})"
    );
    assert_eq!(
        blocks[thinking_idx]["thinking"], "injected-thinking",
        "injected thinking text must match the cached value"
    );
}
