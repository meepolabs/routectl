//! anthropic-beta and user-agent header wiring, decoupled from auth_kind.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn oauth_bearer_does_not_auto_inject_beta_gate() {
    let mock_server = MockServer::start().await;

    // Set up a mock that will ONLY match if anthropic-beta equals
    // the value we explicitly put in extra_headers (NOT the old
    // auto-injected oauth-2025-04-20).
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer test-key"))
        .and(header("anthropic-beta", "context-1m-2025-08-07"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response_body()))
        .mount(&mock_server)
        .await;

    // No wiremock match will succeed if anthropic-beta is missing
    // or set to oauth-2025-04-20 -- the provider should hit timeout.
    let cfg = AnthropicApiConfig {
        id: "oauth-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: mock_server.uri(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras: vec![("anthropic-beta".into(), "context-1m-2025-08-07".into())],
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);
    let resp = provider.complete(req).await.unwrap();
    assert_eq!(resp.id, "msg_check");
}

#[tokio::test]
async fn api_key_auth_can_set_beta_via_extra_headers() {
    let mock_server = MockServer::start().await;

    // Match the anthropic-beta extra header. Use a single flag
    // (no comma) because wiremock's `header(name, value)` matcher
    // compares against parsed comma-split values; a comma-joined
    // flag list is exposed as multiple values, not one.
    let expected_beta = "context-1m-2025-08-07";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-beta", expected_beta))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response_body()))
        .mount(&mock_server)
        .await;

    let cfg = AnthropicApiConfig {
        id: "apikey-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: mock_server.uri(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: vec![("anthropic-beta".into(), expected_beta.into())],
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);
    let resp = provider.complete(req).await.unwrap();
    assert_eq!(resp.id, "msg_check");
}

#[tokio::test]
async fn user_agent_override_reaches_outbound() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("user-agent", "claude-code/1.2.3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response_body()))
        .mount(&mock_server)
        .await;

    let cfg = AnthropicApiConfig {
        id: "ua-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: mock_server.uri(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: Some("claude-code/1.2.3".into()),
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);
    let resp = provider.complete(req).await.unwrap();
    assert_eq!(resp.id, "msg_check");
}
