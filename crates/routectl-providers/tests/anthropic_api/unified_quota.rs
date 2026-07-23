//! anthropic-ratelimit-unified-* quota/overage carrier wire-in.

use super::*;
use pretty_assertions::assert_eq;

/// The unified-quota response headers a subscription response carries.
/// Mounted on the wiremock so both complete() and stream() observe
/// the same family.
const UNIFIED_HEADERS: &[(&str, &str)] = &[
    ("anthropic-ratelimit-unified-status", "allowed"),
    ("anthropic-ratelimit-unified-overage-status", "allowed"),
    ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
    ("anthropic-ratelimit-unified-overage-utilization", "0.00"),
    (
        "anthropic-ratelimit-unified-representative-claim",
        "five_hour",
    ),
    ("anthropic-ratelimit-unified-reset", "2026-06-09T12:00:00Z"),
];

fn with_unified_headers(mut tmpl: ResponseTemplate) -> ResponseTemplate {
    for (k, v) in UNIFIED_HEADERS {
        tmpl = tmpl.append_header(*k, *v);
    }
    tmpl
}

#[tokio::test]
async fn complete_populates_upstream_meta_from_unified_headers() {
    // Arrange
    let mock_server = MockServer::start().await;
    let body = make_response_body();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(with_unified_headers(
            ResponseTemplate::new(200).set_body_json(body),
        ))
        .mount(&mock_server)
        .await;
    let provider = make_provider(&mock_server.uri());
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);

    // Act
    let resp = provider.complete(req).await.unwrap();

    // Assert: the unified-quota carrier is populated with the parsed
    // values, and the carrier never serialized onto the wire body.
    let meta = resp
        .upstream_meta
        .as_ref()
        .expect("upstream_meta must be populated from unified headers");
    let quota = meta
        .anthropic_unified
        .as_ref()
        .expect("anthropic_unified present");
    assert_eq!(quota.status.as_deref(), Some("allowed"));
    assert_eq!(quota.representative_claim.as_deref(), Some("five_hour"));
    assert_eq!(quota.utilization.as_deref(), Some("0.42"));
    assert_eq!(quota.reset.as_deref(), Some("2026-06-09T12:00:00Z"));
    assert!(!quota.is_overage());
}

#[tokio::test]
async fn complete_upstream_meta_is_none_without_unified_headers() {
    // Arrange: a normal 200 with no unified family on the response.
    let mock_server = MockServer::start().await;
    let body = make_response_body();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&mock_server)
        .await;
    let provider = make_provider(&mock_server.uri());
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);

    // Act
    let resp = provider.complete(req).await.unwrap();

    // Assert
    assert!(
        resp.upstream_meta.is_none(),
        "no unified family means no upstream_meta carrier"
    );
}

#[tokio::test]
async fn stream_carries_upstream_meta_on_first_chunk_only() {
    // Arrange
    let mock_server = MockServer::start().await;
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_um01\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi!\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(with_unified_headers(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .append_header("content-type", "text/event-stream"),
        ))
        .mount(&mock_server)
        .await;
    let provider = make_provider(&mock_server.uri());
    let mut req = base_req("claude-3-opus", vec![user_msg("stream test")]);
    req.stream = Some(true);

    // Act
    use futures::StreamExt;
    let mut stream = provider.stream(req).await.unwrap();
    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.unwrap());
    }

    // Assert: exactly the FIRST chunk carries the carrier; no later
    // chunk does.
    assert!(!chunks.is_empty(), "stream must yield at least one chunk");
    let first_meta = chunks[0]
        .upstream_meta
        .as_ref()
        .expect("first chunk must carry upstream_meta");
    let quota = first_meta
        .anthropic_unified
        .as_ref()
        .expect("anthropic_unified present on first chunk");
    assert_eq!(quota.representative_claim.as_deref(), Some("five_hour"));
    for (i, c) in chunks.iter().enumerate().skip(1) {
        assert!(
            c.upstream_meta.is_none(),
            "chunk {i} must NOT carry upstream_meta (first-chunk-only contract)"
        );
    }
}

#[tokio::test]
async fn stream_upstream_meta_is_none_without_unified_headers() {
    // Arrange
    let mock_server = MockServer::start().await;
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_um02\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi!\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;
    let provider = make_provider(&mock_server.uri());
    let mut req = base_req("claude-3-opus", vec![user_msg("stream test")]);
    req.stream = Some(true);

    // Act
    use futures::StreamExt;
    let mut stream = provider.stream(req).await.unwrap();
    let mut any_meta = false;
    while let Some(result) = stream.next().await {
        if result.unwrap().upstream_meta.is_some() {
            any_meta = true;
        }
    }

    // Assert
    assert!(
        !any_meta,
        "no unified family means no chunk carries upstream_meta"
    );
}

#[tokio::test]
async fn stream_silently_drops_upstream_meta_when_no_canonical_chunk_yields() {
    // Arrange: an SSE body of only message_start + message_stop, with
    // no content_block / delta events. Both arms return Ok(None) (the
    // pending_opaque safety-net flush does not fire because no unknown
    // block was opened), so the stream yields ZERO canonical chunks.
    // The unified-header carrier has no first chunk to attach to and is
    // intentionally dropped -- this pins the documented contract.
    let mock_server = MockServer::start().await;
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_um03\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(with_unified_headers(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .append_header("content-type", "text/event-stream"),
        ))
        .mount(&mock_server)
        .await;
    let provider = make_provider(&mock_server.uri());
    let mut req = base_req("claude-3-opus", vec![user_msg("stream test")]);
    req.stream = Some(true);

    // Act
    use futures::StreamExt;
    let mut stream = provider.stream(req).await.unwrap();
    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.unwrap());
    }

    // Assert: zero canonical chunks yielded, so no chunk carries the
    // unified-quota carrier -- it is silently dropped.
    assert!(
        chunks.is_empty(),
        "message_start + message_stop with no content must yield zero canonical chunks, got: {chunks:?}"
    );
    assert!(
        chunks.iter().all(|c| c.upstream_meta.is_none()),
        "upstream_meta must be silently dropped when no chunk exists to carry it"
    );
}

#[tokio::test]
async fn oauth_bearer_populates_upstream_meta_from_unified_headers() {
    // The unified-header parse + upstream_meta attach is not auth-gated:
    // the OauthBearer path must populate the carrier identically to the
    // ApiKey path. This pins that both AuthKind paths observe the family.
    let mock_server = MockServer::start().await;
    let body = make_response_body();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer oat-token"))
        .respond_with(with_unified_headers(
            ResponseTemplate::new(200).set_body_json(body),
        ))
        .mount(&mock_server)
        .await;
    let cfg = AnthropicApiConfig {
        id: "oauth-unified-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("oat-token")),
        base_url: mock_server.uri(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras: Vec::new(),
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

    // Act
    let resp = provider.complete(req).await.unwrap();

    // Assert: the unified-quota carrier is populated on the OAuth path.
    let meta = resp
        .upstream_meta
        .as_ref()
        .expect("upstream_meta must be populated on the OauthBearer path");
    let quota = meta
        .anthropic_unified
        .as_ref()
        .expect("anthropic_unified present");
    assert_eq!(quota.status.as_deref(), Some("allowed"));
    assert_eq!(quota.representative_claim.as_deref(), Some("five_hour"));
    assert_eq!(quota.utilization.as_deref(), Some("0.42"));
}
