//! Operator body fields reach a Bedrock upstream through `payload_extras`
//! alone: the dispatch layer layers them into the request, and both wire
//! shapes carry them, on the Invoke body's top level and in the Converse
//! `additionalModelRequestFields` bag. `BedrockConfig` holds no body-field
//! source of its own, so a request without them ships neither field.

use std::sync::Arc;

use routectl_core::{ChatRequest, Provider, RequestProvenance};
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

/// The one body `shape` sent to the mock.
async fn shipped_body(shape: BedrockApiShape, req: ChatRequest) -> Value {
    let server = recording_upstream().await;
    let _ = provider(&server, shape).await.complete(req).await;
    let received = server
        .received_requests()
        .await
        .expect("read mock requests");
    assert_eq!(received.len(), 1, "{shape:?}: one upstream call");
    serde_json::from_slice(&received[0].body).expect("JSON body")
}

/// Where operator body fields ride in `body`: the whole body on Invoke, the
/// field bag on Converse.
fn operator_fields(shape: BedrockApiShape, body: Value) -> Value {
    match shape {
        BedrockApiShape::Invoke => body,
        BedrockApiShape::Converse => body["additionalModelRequestFields"].clone(),
    }
}

async fn shipped_fields(shape: BedrockApiShape, req: ChatRequest) -> Value {
    operator_fields(shape, shipped_body(shape, req).await)
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

const SENTINEL: &str = "OPERATOR-SENTINEL";

/// Operator `payload_extras` naming fields the router writes itself on
/// `shape`, each carrying [`SENTINEL`], plus the non-managed `top_k`.
fn managed_key_extras(shape: BedrockApiShape) -> Value {
    let mut extras = json!({
        "model": SENTINEL,
        "messages": [{"role": "user", "content": SENTINEL}],
        "system": SENTINEL,
        "max_tokens": SENTINEL,
        "temperature": SENTINEL,
        "stop_sequences": [SENTINEL],
        "stream": SENTINEL,
        "tools": [{"name": SENTINEL}],
        "tool_choice": {"type": SENTINEL},
        "cache_control": {"type": SENTINEL},
        "thinking": {"type": SENTINEL},
        "top_k": 40,
    });
    let per_shape = match shape {
        BedrockApiShape::Invoke => json!({
            "anthropic_version": SENTINEL,
            "parallel_tool_calls": SENTINEL,
        }),
        BedrockApiShape::Converse => json!({
            "inferenceConfig": {"maxTokens": SENTINEL},
            "toolConfig": {"tools": [SENTINEL]},
            "additionalModelResponseFieldPaths": [SENTINEL],
        }),
    };
    let (Some(obj), Some(more)) = (extras.as_object_mut(), per_shape.as_object()) else {
        unreachable!("both literals are objects");
    };
    obj.extend(more.clone());
    extras
}

/// The router's own values for the managed fields it always writes: the
/// client's message, its `max_tokens`, and the Bedrock `anthropic_version`
/// on Invoke.
fn assert_router_values_shipped(shape: BedrockApiShape, body: &Value) {
    let text = body.to_string();
    assert!(text.contains("hello"), "{shape:?}: {body}");
    match shape {
        BedrockApiShape::Invoke => {
            assert_eq!(body["max_tokens"], json!(16), "{shape:?}: {body}");
            assert_eq!(
                body["anthropic_version"],
                json!("bedrock-2023-05-31"),
                "{shape:?}: {body}"
            );
        }
        BedrockApiShape::Converse => {
            assert_eq!(
                body["inferenceConfig"]["maxTokens"],
                json!(16),
                "{shape:?}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn operator_payload_extras_cannot_override_router_managed_fields_on_either_shape() {
    for provenance in [RequestProvenance::Library, RequestProvenance::OpenaiIngress] {
        for shape in SHAPES {
            // Arrange
            let mut req = request(Some(managed_key_extras(shape)));
            req.routectl_internal.provenance = provenance;

            // Act
            let body = shipped_body(shape, req).await;

            // Assert
            assert!(
                !body.to_string().contains(SENTINEL),
                "{provenance:?} {shape:?}: {body}"
            );
            assert_router_values_shipped(shape, &body);
            let fields = operator_fields(shape, body);
            assert_eq!(
                fields["top_k"],
                json!(40),
                "{provenance:?} {shape:?}: {fields}"
            );
        }
    }
}
