//! Bedrock mantle lane wire behavior: SigV4/bearer-signed egress, no
//! `x-api-key`, and a no-redirect client. These pin the runtime lane
//! against a mock upstream; the credential-scope and URL-builder units
//! live in `routectl-providers/src/mantle.rs`.

use super::*;
use routectl_core::Error;
use routectl_core::failure_class::{FailureClass, classify};
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

fn req_with_mcp_servers() -> ChatRequest {
    let mut req = base_req("claude-3-opus", vec![user_msg("hello")]);
    req.provider_extras = Some(json!({"mcp_servers": [{
        "type": "url",
        "url": "https://mcp.example.com/sse",
        "name": "remote",
        "authorization_token": "zz-mcp-connector-token",
    }]}));
    req
}

/// Mount a 200 on both body-carrying endpoints, send one messages and one
/// count_tokens request carrying a client `mcp_servers`, and return the two
/// bodies the upstream received, messages first.
async fn mcp_servers_bodies_received(mantle: bool) -> Vec<Value> {
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
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "input_tokens": 3 })))
        .mount(&mock_server)
        .await;
    let provider = if mantle {
        mantle_provider_bearer(&mock_server.uri()).await
    } else {
        make_provider(&mock_server.uri())
    };

    provider.complete(req_with_mcp_servers()).await.unwrap();
    provider.count_tokens(req_with_mcp_servers()).await.unwrap();

    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].url.path(), "/v1/messages");
    assert_eq!(received[1].url.path(), "/v1/messages/count_tokens");
    received
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// A client `mcp_servers` carries connector credentials for a remote MCP
/// server: on the mantle lane neither the messages body nor the
/// count_tokens body may carry it.
#[tokio::test]
async fn mantle_lane_withholds_mcp_servers_from_messages_and_count_tokens() {
    for body in mcp_servers_bodies_received(true).await {
        assert!(body.get("mcp_servers").is_none(), "{body}");
        assert!(
            !body.to_string().contains("zz-mcp-connector-token"),
            "{body}"
        );
    }
}

/// Positive control: the first-party lane forwards `mcp_servers` on both
/// endpoints, so the mantle absence above is the withhold, not the fixture.
#[tokio::test]
async fn first_party_lane_forwards_mcp_servers_to_messages_and_count_tokens() {
    for body in mcp_servers_bodies_received(false).await {
        assert_eq!(
            body["mcp_servers"][0]["authorization_token"], "zz-mcp-connector-token",
            "{body}"
        );
    }
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

/// A mantle-lane provider posting to `base_url` with a resolved SigV4
/// (static-key) credential. The signer runs post-build, so the wire
/// carries an `AWS4-HMAC-SHA256` `Authorization` scoped to the
/// `bedrock-mantle` service under the lane region.
async fn mantle_provider_sigv4(base_url: &str) -> AnthropicApiProvider {
    let creds = resolve(
        &BedrockCreds::Static {
            access_key: "AKIAmantlewire000000".into(),
            secret_key: "mantle-wire-secret-key".into(),
            session_token: None,
        },
        "us-west-2",
    )
    .await
    .unwrap();
    let cfg = AnthropicApiConfig {
        id: "mantle-sigv4-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("")),
        base_url: base_url.to_string(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
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

/// SigV4 lane: complete() signs the built request with an
/// `AWS4-HMAC-SHA256` `Authorization` credential scoped to
/// `.../us-west-2/bedrock-mantle/aws4_request`, stamps `x-amz-date`,
/// carries the bare model id in the body, stamps `anthropic-version`, and
/// never attaches `x-api-key`.
#[tokio::test]
async fn sigv4_lane_signs_wire_with_mantle_service_scope() {
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

    let provider = mantle_provider_sigv4(&mock_server.uri()).await;
    let req = base_req("claude-3-opus", vec![user_msg("hello")]);

    provider.complete(req).await.unwrap();

    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let auth = received[0]
        .headers
        .get("authorization")
        .expect("SigV4 lane must attach Authorization")
        .to_str()
        .unwrap();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 "),
        "SigV4 lane must sign with AWS4-HMAC-SHA256; got {auth}"
    );
    assert!(
        auth.contains("Credential="),
        "SigV4 Authorization must carry a Credential; got {auth}"
    );
    assert!(
        auth.contains("/us-west-2/bedrock-mantle/aws4_request"),
        "credential scope must name the mantle service under the lane region; got {auth}"
    );
    assert!(
        received[0].headers.get("x-amz-date").is_some(),
        "SigV4 lane must stamp x-amz-date"
    );
    assert!(
        received[0].headers.get("x-api-key").is_none(),
        "mantle lane must never attach x-api-key"
    );
    assert_eq!(
        received[0]
            .headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some("2023-06-01"),
        "anthropic-version must stamp 2023-06-01 on the mantle wire"
    );
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        body.get("model").and_then(Value::as_str),
        Some("claude-3-opus"),
        "the bare model id must reach the wire body verbatim (no ARN, no region prefix)"
    );
}

/// An AWS-shaped 403 (`SignatureDoesNotMatch`) returned by the mantle
/// upstream round-trips end-to-end through the provider error path -- the
/// AWS exception token lifts into `upstream_type` and the error classifies
/// as `FailureClass::Auth` (403 is Auth by status; the lifted token is
/// what makes the log truthful).
#[tokio::test]
async fn aws_shaped_403_classifies_as_auth_end_to_end() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "__type": "com.amazonaws.bedrock#SignatureDoesNotMatch",
            "message": "The request signature we calculated does not match the signature you provided."
        })))
        .mount(&mock_server)
        .await;

    let provider = mantle_provider_bearer(&mock_server.uri()).await;
    let req = base_req("claude-3-opus", vec![user_msg("hello")]);

    let err = provider.complete(req).await.unwrap_err();
    match &err {
        Error::Upstream {
            status,
            upstream_type,
            ..
        } => {
            assert_eq!(*status, 403);
            assert_eq!(
                upstream_type.as_deref(),
                Some("SignatureDoesNotMatch"),
                "the bare AWS exception token must lift into upstream_type"
            );
        }
        other => panic!("expected Error::Upstream from an AWS 403, got {other:?}"),
    }
    assert_eq!(
        classify(&err, Some("anthropic-api")).class,
        FailureClass::Auth,
        "an AWS-shaped 403 on the mantle lane must classify as Auth end-to-end"
    );
}

const PLANTED_HOST: &str = "planted-bucket-zq7";
const PLANTED_PATH: &str = "secret-key-path-k3";

fn planted(scheme: &str) -> String {
    format!("{scheme}://{PLANTED_HOST}/{PLANTED_PATH}.png")
}

fn parts_msg(role: Role, parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extras: serde_json::Map::new(),
    }
}

fn user_parts_req(part: ContentPart) -> ChatRequest {
    let text = ContentPart::Known(KnownContentPart::Text {
        text: "look".into(),
        citations: None,
        cache_control: None,
        extras: serde_json::Map::new(),
    });
    base_req(
        "claude-3-opus",
        vec![parts_msg(Role::User, vec![text, part])],
    )
}

fn native_image(url: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Image {
        source: json!({"type": "url", "url": url}),
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

fn image_url_part(url: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::ImageUrl {
        image_url: json!({"url": url}),
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

fn url_document(url: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Document {
        source: json!({"type": "url", "url": url}),
        title: None,
        citations: None,
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

fn file_url_part(url: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::File {
        file: json!({"file_url": url}),
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

/// A user turn, an assistant `tool_use` whose input is `input`, and a tool
/// result carrying `result_part`.
fn tool_round_req(input: Value, result_part: ContentPart) -> ChatRequest {
    let tool_use = ContentPart::Known(KnownContentPart::ToolUse {
        id: "toolu_1".into(),
        name: "screenshot".into(),
        input,
        cache_control: None,
        extras: serde_json::Map::new(),
    });
    let mut tool = parts_msg(Role::Tool, vec![result_part]);
    tool.tool_call_id = Some("toolu_1".into());
    base_req(
        "claude-3-opus",
        vec![
            user_msg("take one"),
            parts_msg(Role::Assistant, vec![tool_use]),
            tool,
        ],
    )
}

/// An unmodeled `tool_search_output` content part whose `tools` carries a
/// url-type image source.
fn nested_tool_search_output(url: &str) -> ContentPart {
    let mut extras = serde_json::Map::new();
    extras.insert(
        "tools".into(),
        json!([{"type": "image", "source": {"type": "url", "url": url}}]),
    );
    ContentPart::Other {
        type_tag: "tool_search_output".into(),
        cache_control: None,
        extras,
    }
}

/// A `tool_result` whose content holds a `tool_use` object, so the
/// `tool_use` sits below a content part; its `input` carries a url-type
/// image source.
fn tool_use_in_tool_result(url: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::ToolResult {
        tool_use_id: "toolu_1".into(),
        content: json!([{
            "type": "tool_use",
            "id": "toolu_9",
            "name": "relay",
            "input": {"type": "image", "source": {"type": "url", "url": url}}
        }]),
        is_error: None,
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

async fn media_upstream() -> MockServer {
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
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "input_tokens": 3 })))
        .mount(&mock_server)
        .await;
    mock_server
}

fn assert_media_refusal(result: Result<(), Error>, row: &str, field_at: &str) {
    match result {
        Err(Error::NormalizeRequest(_, msg, routectl_core::RefusalReason::Unrepresentable)) => {
            assert!(msg.contains(field_at), "{row}: names the field: {msg}");
            assert!(
                msg.contains("https or data"),
                "{row}: names the class: {msg}"
            );
            assert!(
                !msg.contains(PLANTED_HOST) && !msg.contains(PLANTED_PATH),
                "{row}: refusal must not echo the URL: {msg}"
            );
        }
        other => panic!("{row}: expected a local unrepresentable refusal, got {other:?}"),
    }
}

/// Every image or document URL Mantle would fetch under the operator's AWS
/// identity is refused locally on both the messages and the count_tokens
/// path unless it is https or data, and the upstream is never called.
#[tokio::test]
async fn mantle_lane_refuses_unforwardable_media_urls_on_messages_and_count_tokens() {
    let rows: Vec<(&str, ChatRequest, &str)> = vec![
        (
            "native image s3",
            user_parts_req(native_image(&planted("s3"))),
            "`source` at messages[0].content[1] ",
        ),
        (
            "openai image_url gs",
            user_parts_req(image_url_part(&planted("gs"))),
            "`source` at messages[0].content[1] ",
        ),
        (
            "document source http",
            user_parts_req(url_document(&planted("http"))),
            "`source` at messages[0].content[1] ",
        ),
        (
            "tool_result nested image s3",
            tool_round_req(json!({}), native_image(&planted("s3"))),
            "`source` at messages[2].content[0].content[0] ",
        ),
        (
            "verbatim file block file_url s3",
            user_parts_req(file_url_part(&planted("s3"))),
            "`file_url` at messages[0].content[1].file ",
        ),
        (
            "empty image url",
            user_parts_req(image_url_part("")),
            "`source` at messages[0].content[1] ",
        ),
        (
            "tool_search_output content part tools s3",
            user_parts_req(nested_tool_search_output(&planted("s3"))),
            "`source` at messages[0].content[1].tools[0] ",
        ),
        (
            "tool_use nested in a tool_result input s3",
            tool_round_req(json!({}), tool_use_in_tool_result(&planted("s3"))),
            "`source` at messages[2].content[0].content[0].content[0].input ",
        ),
    ];

    for (row, req, field_at) in rows {
        let mock_server = media_upstream().await;
        let provider = mantle_provider_bearer(&mock_server.uri()).await;

        let complete = provider.complete(req.clone()).await.map(|_| ());
        let count = provider.count_tokens(req).await.map(|_| ());

        assert_media_refusal(complete, &format!("{row} (messages)"), field_at);
        assert_media_refusal(count, &format!("{row} (count_tokens)"), field_at);
        let received = mock_server.received_requests().await.unwrap();
        assert!(received.is_empty(), "{row}: no upstream call");
    }
}

/// https and data media URLs reach the Mantle upstream on both paths.
#[tokio::test]
async fn mantle_lane_forwards_https_and_data_media_urls() {
    let rows: Vec<(&str, ChatRequest)> = vec![
        (
            "native image https",
            user_parts_req(native_image("https://example.com/a.png")),
        ),
        (
            "openai image_url data",
            user_parts_req(image_url_part("data:image/png;base64,AAAA")),
        ),
        (
            "document source https",
            user_parts_req(url_document("https://example.com/a.pdf")),
        ),
        (
            "tool_result nested image https",
            tool_round_req(json!({}), native_image("https://example.com/b.png")),
        ),
        (
            "verbatim file block file_url https",
            user_parts_req(file_url_part("https://example.com/c.pdf")),
        ),
        (
            "tool_search_output content part tools https",
            user_parts_req(nested_tool_search_output("https://example.com/d.png")),
        ),
        (
            "tool_use nested in a tool_result input https",
            tool_round_req(
                json!({}),
                tool_use_in_tool_result("https://example.com/e.png"),
            ),
        ),
    ];

    for (row, req) in rows {
        let mock_server = media_upstream().await;
        let provider = mantle_provider_bearer(&mock_server.uri()).await;

        provider
            .complete(req.clone())
            .await
            .unwrap_or_else(|e| panic!("{row}: messages must reach the upstream, got {e:?}"));
        provider
            .count_tokens(req)
            .await
            .unwrap_or_else(|e| panic!("{row}: count_tokens must reach the upstream, got {e:?}"));

        let received = mock_server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2, "{row}");
    }
}

/// Client tool arguments are not media: a `tool_use.input` naming an s3 URL
/// in a media-shaped member reaches the Mantle upstream verbatim.
#[tokio::test]
async fn mantle_lane_does_not_inspect_tool_use_input() {
    let mock_server = media_upstream().await;
    let provider = mantle_provider_bearer(&mock_server.uri()).await;
    let input =
        json!({"image_url": planted("s3"), "source": {"type": "url", "url": planted("s3")}});
    let req = tool_round_req(input.clone(), text_part("done"));

    provider
        .complete(req)
        .await
        .expect("a tool_use input naming s3 is not a media URL");

    let received = mock_server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["messages"][1]["content"][0]["input"], input, "{body}");
}

/// Off Mantle the URL check does not run: an empty OpenAI `image_url` still
/// ships as an empty url source, and an s3 image reaches the upstream.
#[tokio::test]
async fn first_party_lane_forwards_media_urls_unchanged() {
    let rows = [
        (
            "empty image_url",
            user_parts_req(image_url_part("")),
            json!({"type": "url", "url": ""}),
        ),
        (
            "native image s3",
            user_parts_req(native_image(&planted("s3"))),
            json!({"type": "url", "url": planted("s3")}),
        ),
    ];

    for (row, req, expected_source) in rows {
        let mock_server = media_upstream().await;
        let provider = make_provider(&mock_server.uri());

        provider
            .complete(req)
            .await
            .unwrap_or_else(|e| panic!("{row}: first-party lane must forward, got {e:?}"));

        let received = mock_server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(
            body["messages"][0]["content"][1]["source"], expected_source,
            "{row}: {body}"
        );
    }
}

/// A user turn whose text part carries `part_extras` and whose message
/// carries `message_extras`, both unmodeled members a same-dialect client
/// sent; the request comes from the Anthropic ingress, so both ride the
/// body.
fn carrying_req(
    part_extras: serde_json::Map<String, Value>,
    message_extras: serde_json::Map<String, Value>,
) -> ChatRequest {
    let text = ContentPart::Known(KnownContentPart::Text {
        text: "look".into(),
        citations: None,
        cache_control: None,
        extras: part_extras,
    });
    let mut turn = parts_msg(Role::User, vec![text]);
    turn.extras = message_extras;
    let mut req = base_req("claude-3-opus", vec![turn]);
    req.routectl_internal.provenance = routectl_core::RequestProvenance::AnthropicIngress;
    req
}

fn member(key: &str, value: Value) -> serde_json::Map<String, Value> {
    std::iter::once((key.to_owned(), value)).collect()
}

fn url_image_value(url: &str) -> Value {
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

/// The walker sees inside carried members: a non-https media URL nested in
/// a part's extras, and a `tool_use`-shaped object in an array under a
/// message member other than `content`, are refused on Mantle on both the
/// messages and the count_tokens path.
#[tokio::test]
async fn mantle_lane_refuses_unforwardable_media_urls_in_carried_members() {
    let rows: Vec<(&str, ChatRequest, &str)> = vec![
        (
            "part extras image s3",
            carrying_req(
                member("meta", url_image_value(&planted("s3"))),
                serde_json::Map::new(),
            ),
            "`source` at messages[0].content[0].meta ",
        ),
        (
            "message extras array holding a tool_use shape",
            carrying_req(
                serde_json::Map::new(),
                member(
                    "meta",
                    json!([{"type": "tool_use", "input": url_image_value(&planted("s3"))}]),
                ),
            ),
            "`source` at messages[0].meta[0].input ",
        ),
    ];

    for (row, req, field_at) in rows {
        let mock_server = media_upstream().await;
        let provider = mantle_provider_bearer(&mock_server.uri()).await;

        let complete = provider.complete(req.clone()).await.map(|_| ());
        let count = provider.count_tokens(req).await.map(|_| ());

        assert_media_refusal(complete, &format!("{row} (messages)"), field_at);
        assert_media_refusal(count, &format!("{row} (count_tokens)"), field_at);
        let received = mock_server.received_requests().await.unwrap();
        assert!(received.is_empty(), "{row}: no upstream call");
    }
}

/// Off Mantle the same carried member is the client's own request and
/// reaches the first-party upstream as sent.
#[tokio::test]
async fn first_party_lane_forwards_a_media_url_in_part_extras() {
    let meta = url_image_value(&planted("s3"));
    let req = carrying_req(member("meta", meta.clone()), serde_json::Map::new());
    let mock_server = media_upstream().await;
    let provider = make_provider(&mock_server.uri());

    provider
        .complete(req)
        .await
        .expect("first-party lane must forward");

    let received = mock_server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["messages"][0]["content"][0]["meta"], meta, "{body}");
}
