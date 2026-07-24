//! Bedrock mantle lane wire behavior: SigV4/bearer-signed egress, no
//! `x-api-key`, and a no-redirect client. These pin the runtime lane
//! against a mock upstream; the credential-scope and URL-builder units
//! live in `routectl-providers/src/mantle.rs`.

use super::*;
use routectl_providers::anthropic_api::MantleAuth;
use routectl_providers::bedrock::BedrockCreds;
use routectl_providers::bedrock::auth::resolve;

/// A mantle-lane provider posting to `base_url` with a resolved bearer
/// credential. `base_url` is the mock server (the factory derives the
/// real host from the region; here we point at wiremock while still
/// signing under the region scope).
async fn mantle_provider_bearer(base_url: &str) -> AnthropicApiProvider {
    let creds = resolve(
        &BedrockCreds::BearerKey {
            key: "mantle-bearer-key".into(),
        },
        "us-west-2",
    )
    .await
    .unwrap();
    let cfg = AnthropicApiConfig {
        id: "mantle-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("")),
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
        mantle: Some(MantleAuth {
            region: "us-west-2".into(),
            creds,
        }),
    };
    AnthropicApiProvider::new(cfg)
}

/// count_tokens on the mantle lane serializes to bytes and signs the
/// request: the wire carries `Authorization` (bearer) and NO
/// `x-api-key`, plus the anthropic-version header.
#[tokio::test]
async fn count_tokens_on_mantle_lane_is_signed_with_no_x_api_key() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "input_tokens": 42 })))
        .mount(&mock_server)
        .await;

    let provider = mantle_provider_bearer(&mock_server.uri()).await;
    let req = base_req("claude-3-opus", vec![user_msg("hello")]);

    let result = provider.count_tokens(req).await.unwrap();
    assert_eq!(result.input_tokens, 42);

    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let auth = received[0]
        .headers
        .get("authorization")
        .expect("mantle lane must attach Authorization")
        .to_str()
        .unwrap();
    assert!(
        auth.starts_with("Bearer "),
        "bearer creds must sign as Authorization: Bearer; got {auth}"
    );
    assert!(
        received[0].headers.get("x-api-key").is_none(),
        "mantle lane must never attach x-api-key"
    );
    assert!(
        received[0].headers.get("anthropic-version").is_some(),
        "anthropic-version must stamp on the mantle lane"
    );
    // The signed body is real JSON bytes (SigV4 requires a hashable body).
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(
        body.get("model").is_some(),
        "count_tokens body reached wire"
    );
}

/// stream() on the mantle lane is signed and never carries x-api-key.
/// The request is sent (and thus signed) by the time `stream()` returns
/// the response stream, so the wire assertion holds without draining it.
#[tokio::test]
async fn stream_on_mantle_lane_is_signed_with_no_x_api_key() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        )
        .mount(&mock_server)
        .await;

    let provider = mantle_provider_bearer(&mock_server.uri()).await;
    let req = base_req("claude-3-opus", vec![user_msg("hello")]);

    let _stream = provider.stream(req).await.unwrap();

    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(
        received[0]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.starts_with("Bearer ")),
        "mantle stream() must be signed"
    );
    assert!(
        received[0].headers.get("x-api-key").is_none(),
        "mantle lane must never attach x-api-key"
    );
}

/// complete() on the mantle lane is signed and never carries x-api-key.
#[tokio::test]
async fn complete_on_mantle_lane_is_signed_with_no_x_api_key() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "content": [{ "type": "text", "text": "hi" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })))
        .mount(&mock_server)
        .await;

    let provider = mantle_provider_bearer(&mock_server.uri()).await;
    let req = base_req("claude-3-opus", vec![user_msg("hello")]);

    provider.complete(req).await.unwrap();

    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(
        received[0]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.starts_with("Bearer ")),
        "mantle complete() must be signed"
    );
    assert!(
        received[0].headers.get("x-api-key").is_none(),
        "mantle lane must never attach x-api-key"
    );
}

/// The mantle lane uses a no-redirect client: a 3xx is surfaced as an
/// upstream failure and NEVER followed to its `Location` target.
#[tokio::test]
async fn mantle_lane_does_not_follow_redirects() {
    let mock_server = MockServer::start().await;
    let redirect_target = format!("{}/v1/redirected", mock_server.uri());
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", redirect_target.as_str()),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/redirected"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_x",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "content": [{ "type": "text", "text": "should never be reached" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })))
        .mount(&mock_server)
        .await;

    let provider = mantle_provider_bearer(&mock_server.uri()).await;
    let req = base_req("claude-3-opus", vec![user_msg("hello")]);

    // The 302 is not followed; the provider surfaces it as an error
    // rather than chasing the Location (which would replay the signature
    // cross-host).
    let err = provider.complete(req).await.unwrap_err();
    let _ = err;

    let received = mock_server.received_requests().await.unwrap();
    let followed = received
        .iter()
        .filter(|r| r.url.path() == "/v1/redirected")
        .count();
    assert_eq!(
        followed, 0,
        "no-redirect client must not follow the 302 to its Location target"
    );
}
