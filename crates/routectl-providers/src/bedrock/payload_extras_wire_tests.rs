//! Operator body fields reach a Bedrock upstream through `payload_extras`
//! alone: the dispatch layer layers them into the request, and both wire
//! shapes carry them, on the Invoke body's top level and in the Converse
//! `additionalModelRequestFields` bag. `BedrockConfig` holds no body-field
//! source of its own, so a request without them ships neither field.

use std::sync::Arc;

use routectl_core::{ChatRequest, Provider};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth};

const SHAPES: [BedrockApiShape; 2] = [BedrockApiShape::Invoke, BedrockApiShape::Converse];

/// A mock that answers 500 to anything, so every request leaves a record
/// and the call fails fast.
async fn recording_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("{}"))
        .mount(&server)
        .await;
    server
}

async fn provider(server: &MockServer, shape: BedrockApiShape) -> BedrockProvider {
    let creds = BedrockCreds::BearerKey {
        key: "payload-extras-test-key".into(),
    };
    let cfg = BedrockConfig {
        id: "bedrock-payload-extras".into(),
        region: "us-east-1".into(),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        api_shape: shape,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
    };
    let resolved = auth::resolve(&creds, "us-east-1").await.expect("resolve");
    let mut provider = BedrockProvider::new(cfg, resolved).expect("canonical region");
    provider.client =
        crate::http_client::build_no_redirect(None, &server.uri()).expect("loopback client");
    provider.runtime_origin = server.uri();
    provider
}

fn request(payload_extras: Option<Value>) -> ChatRequest {
    let mut req = ChatRequest {
        model: "us.anthropic.claude-opus-5-5".into(),
        messages: vec![routectl_core::test_utils::user_msg("hello")].into(),
        max_tokens: Some(16),
        ..Default::default()
    };
    if let Some(extras) = payload_extras {
        req.provider_extras = Some(extras.clone());
        req.routectl_internal.operator_payload_extras = Some(Arc::new(extras));
    }
    req
}

/// The one body `shape` sent to the mock, cut to where operator body fields
/// ride: the whole body on Invoke, the field bag on Converse.
async fn shipped_fields(shape: BedrockApiShape, req: ChatRequest) -> Value {
    let server = recording_upstream().await;
    let _ = provider(&server, shape).await.complete(req).await;
    let received = server
        .received_requests()
        .await
        .expect("read mock requests");
    assert_eq!(received.len(), 1, "{shape:?}: one upstream call");
    let body: Value = serde_json::from_slice(&received[0].body).expect("JSON body");
    match shape {
        BedrockApiShape::Invoke => body,
        BedrockApiShape::Converse => body["additionalModelRequestFields"].clone(),
    }
}

#[tokio::test]
async fn an_operator_payload_extras_entry_reaches_the_wire_on_both_shapes() {
    for shape in SHAPES {
        // Arrange
        let req = request(Some(json!({
            "top_k": 40,
            "metadata": {"trace": "op-trace"},
        })));

        // Act
        let fields = shipped_fields(shape, req).await;

        // Assert
        assert_eq!(fields["top_k"], json!(40), "{shape:?}: {fields}");
        assert_eq!(
            fields["metadata"],
            json!({"trace": "op-trace"}),
            "{shape:?}: {fields}"
        );
    }
}

/// Control for the test above: with no `payload_extras` the same provider
/// ships neither field, so nothing but `payload_extras` put them there.
#[tokio::test]
async fn without_payload_extras_no_operator_body_field_reaches_the_wire() {
    for shape in SHAPES {
        // Act
        let fields = shipped_fields(shape, request(None)).await;

        // Assert
        assert!(fields.get("top_k").is_none(), "{shape:?}: {fields}");
        assert!(fields.get("metadata").is_none(), "{shape:?}: {fields}");
    }
}
