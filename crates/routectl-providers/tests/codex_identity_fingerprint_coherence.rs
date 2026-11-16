//! Fingerprint coherence on the wire: with a configured codex identity
//! installed, a provider presenting the codex client identity sends the SAME
//! version in its User-Agent and its `version` header, and that User-Agent is
//! byte-identical to the one the OAuth refresh client stamps
//! (`codex_user_agent()`, pinned on the refresh request by the auth crate's
//! `codex_identity_stamps_user_agent_originator_and_residency`). That
//! `build_resolved_models` installs the configured `codex_version` is pinned
//! by the router's `codex_identity_build` binary.
//!
//! `resolved_identity` is a set-once process-global, so this lives in its own
//! test binary with a SINGLE test.

#![cfg(feature = "openai-responses")]

use std::sync::Arc;

use routectl_core::identity::codex::{CodexIdentity, codex_user_agent, set_resolved};
use routectl_core::{ChatRequest, Message, MessageContent, Provider, Role, StaticToken};
use routectl_providers::openai_responses::{
    AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A version that could never be the pinned default, so finding it on the
/// wire proves the installed identity (not the baked-in pin) reached it.
const CODEX_VERSION: &str = "7.7.7-coherence";

fn completed_sse() -> String {
    let completed = serde_json::json!({
        "id": "resp_coherence",
        "object": "response",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "pong"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!("data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n")
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5-codex".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_installed_codex_version_pins_ua_version_header_and_refresh_ua() {
    // Arrange
    assert!(set_resolved(CodexIdentity::new(CODEX_VERSION)));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completed_sse()),
        )
        .mount(&server)
        .await;
    let mut cfg = OpenAiResponsesConfig::new_with_auth(
        "openai-responses:coherence",
        Arc::new(StaticToken::new("test-jwt")),
    );
    cfg.base_url = server.uri();
    cfg.auth_kind = AuthKind::ChatgptOauth;
    cfg.account_id = Some("acct-uuid".into());
    cfg.client_identity = true;
    let provider = OpenAiResponsesProvider::new(cfg);

    // Act
    provider
        .complete(request())
        .await
        .expect("complete against the mock upstream");

    // Assert
    let received = server.received_requests().await.expect("captured requests");
    assert_eq!(received.len(), 1, "exactly one egress request");
    let header = |name: &str| {
        received[0]
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let ua = header("user-agent").expect("egress User-Agent");
    assert!(ua.contains(CODEX_VERSION), "egress UA: {ua}");
    assert_eq!(header("version").as_deref(), Some(CODEX_VERSION));
    assert_eq!(
        ua,
        codex_user_agent(),
        "the egress UA and the refresh-client UA are one fingerprint"
    );
}
