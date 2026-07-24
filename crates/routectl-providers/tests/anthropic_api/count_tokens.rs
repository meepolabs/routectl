//! count_tokens proxying and upstream-error classification.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn count_tokens_proxies_to_v1_messages_count_tokens_endpoint() {
    // Arrange
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "input_tokens": 123
        })))
        .mount(&mock_server)
        .await;

    let cfg = AnthropicApiConfig {
        id: "count-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: mock_server.uri(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        // anthropic-beta on the provider's header_extras so we
        // can verify it rides on the HTTP header (not in the
        // body) for count_tokens.
        header_extras: vec![("anthropic-beta".into(), "context-1m-2025-08-07".into())],
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
    let provider = AnthropicApiProvider::new(cfg);
    let mut req = base_req("claude-3-opus", vec![user_msg("hello")]);
    // Body-side anthropic_beta should be stripped before posting.
    req.anthropic_beta = vec!["context-1m-2025-08-07".into()];

    // Act
    let result = provider.count_tokens(req).await.unwrap();

    // Assert: canonical TokenCount carries the wire field.
    assert_eq!(result.input_tokens, 123);

    // Inspect the wiremock-captured request: body must NOT
    // contain stream / max_tokens / anthropic_beta; headers MUST
    // carry anthropic-beta + anthropic-version.
    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let captured: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(
        captured.get("stream").is_none(),
        "stream must be stripped before count_tokens POST: {captured}"
    );
    assert!(
        captured.get("max_tokens").is_none(),
        "max_tokens must be stripped before count_tokens POST: {captured}"
    );
    assert!(
        captured.get("anthropic_beta").is_none(),
        "anthropic_beta must travel on the header, not body: {captured}"
    );

    // The count_tokens body assembly is allowlist-based. Pin that
    // ONLY the documented allowlist fields can appear, so a future
    // addition to normalize_request can't silently leak into
    // /v1/messages/count_tokens. See `build_count_tokens_body` and
    // <https://docs.anthropic.com/en/api/messages-count-tokens>.
    let allowed: &[&str] = &[
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        "mcp_servers",
        "metadata",
    ];
    let obj = captured
        .as_object()
        .expect("count_tokens body must be a JSON object");
    for k in obj.keys() {
        assert!(
            allowed.contains(&k.as_str()),
            "non-allowlist field `{k}` reached count_tokens body: {captured}"
        );
    }

    let beta_header = received[0]
        .headers
        .get("anthropic-beta")
        .expect("anthropic-beta header missing");
    let beta_value = beta_header.to_str().unwrap();
    assert!(
        beta_value.contains("context-1m-2025-08-07"),
        "anthropic-beta header missing context-1m: {beta_value}"
    );
    assert!(
        received[0].headers.get("anthropic-version").is_some(),
        "anthropic-version header must be present"
    );
}

#[tokio::test]
async fn count_tokens_4xx_surfaces_as_upstream_error() {
    // Arrange
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "bad request"
            }
        })))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);

    // Act
    let err = provider.count_tokens(req).await.unwrap_err();

    // Assert: routectl_core::Error::Upstream { status: 400, body: "bad request" }
    match &err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(*status, 400);
            assert!(
                body.contains("bad request"),
                "upstream body should carry the error.message: {body}"
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_403_populates_upstream_type_permission_error() {
    // Arrange
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "type": "error",
            "error": {
                "type": "permission_error",
                "message": "you do not have permission"
            }
        })))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);

    // Act
    let err = provider.complete(req).await.unwrap_err();

    // Assert: the upstream classifier is lifted into upstream_type.
    match &err {
        routectl_core::Error::Upstream {
            status,
            upstream_type,
            ..
        } => {
            assert_eq!(*status, 403);
            assert_eq!(upstream_type.as_deref(), Some("permission_error"));
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}
