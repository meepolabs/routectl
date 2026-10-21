//! The `additionalProperties` repair walk's node and depth limits refuse the
//! request before any send on every seam that runs the walk, so a schema the
//! walk stopped part-way through never reaches an upstream.
//!
//! One row per seam a schema can enter by, on all three send paths, against a
//! mock that records every request it receives. The Invoke operator row is
//! the only one whose schema enters after the shared assembly, so it alone
//! pins the Invoke rescan. Each seam's positive control proves the mock and
//! the delivery route reach the wire when the schema is within the limits.

use std::sync::Arc;

use routectl_core::{ChatRequest, Error, Provider, RefusalReason, StaticToken};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth};
use crate::anthropic_api::output_schema::{MAX_SCHEMA_DEPTH, MAX_SCHEMA_NODES};
use crate::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};

const MODEL: &str = "us.anthropic.claude-opus-5-5";

/// Where the over-limit schema enters and which egress assembles it.
#[derive(Clone, Copy, Debug)]
enum Seam {
    /// `response_format` through the anthropic-api egress.
    AnthropicApi,
    /// `response_format` through the shared assembly on Bedrock Invoke.
    InvokeClient,
    /// Operator `additional_model_request_fields.output_config`, merged by
    /// Bedrock Invoke after the shared assembly.
    InvokeOperator,
    /// `response_format` through the Bedrock Converse bag.
    Converse,
}

const SEAMS: [Seam; 4] = [
    Seam::AnthropicApi,
    Seam::InvokeClient,
    Seam::InvokeOperator,
    Seam::Converse,
];

#[derive(Clone, Copy, Debug)]
enum Call {
    Complete,
    Stream,
    CountTokens,
}

const CALLS: [Call; 3] = [Call::Complete, Call::Stream, Call::CountTokens];

/// An `anyOf` holding `MAX_SCHEMA_NODES` empty branches: one node past the
/// limit, and nothing object-shaped, so no other bound can fire first.
fn over_node_limit_schema() -> Value {
    json!({"anyOf": vec![json!({}); MAX_SCHEMA_NODES]})
}

/// Object schemas nested one level past the depth limit.
fn over_depth_limit_schema() -> Value {
    (0..=MAX_SCHEMA_DEPTH).fold(
        json!({"type": "object", "properties": {}}),
        |inner, _| json!({"type": "object", "properties": {"a": inner}}),
    )
}

/// A schema within both limits whose root omits the key the walk supplies.
fn repairable_schema() -> Value {
    json!({"type": "object", "properties": {"a": {"type": "string"}}})
}

/// A mock that answers 500 to anything, so a request that slips past the
/// limit still leaves a record and fails fast instead of hanging a stream.
async fn recording_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("{}"))
        .mount(&server)
        .await;
    server
}

async fn bedrock_provider(
    server: &MockServer,
    shape: BedrockApiShape,
    operator_output_config: Option<Value>,
) -> BedrockProvider {
    let creds = BedrockCreds::BearerKey {
        key: "schema-limit-test-key".into(),
    };
    let cfg = BedrockConfig {
        id: "bedrock-schema-limit".into(),
        region: "us-east-1".into(),
        model_id: MODEL.into(),
        api_shape: shape,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: operator_output_config
            .map(|output_config| json!({"output_config": output_config})),
        adaptive_thinking: None,
    };
    let resolved = auth::resolve(&creds, "us-east-1").await.expect("resolve");
    let mut provider = BedrockProvider::new(cfg, resolved).expect("canonical region");
    provider.client =
        crate::http_client::build_no_redirect(None, &server.uri()).expect("loopback client");
    provider.runtime_origin = server.uri();
    provider
}

fn anthropic_provider(server: &MockServer) -> AnthropicApiProvider {
    let mut cfg = AnthropicApiConfig::new_with_auth(
        "anthropic-schema-limit",
        Arc::new(StaticToken::new("k")),
    );
    cfg.base_url = server.uri();
    AnthropicApiProvider::new(cfg)
}

fn request(response_format_schema: Option<Value>) -> ChatRequest {
    ChatRequest {
        model: MODEL.into(),
        messages: vec![routectl_core::test_utils::user_msg("hello")].into(),
        max_tokens: Some(16),
        response_format: response_format_schema.map(
            |schema| json!({"type": "json_schema", "json_schema": {"name": "w", "schema": schema}}),
        ),
        ..Default::default()
    }
}

/// Build `seam`'s provider and request with `schema` delivered the way that
/// seam takes it.
async fn seam_target(
    seam: Seam,
    server: &MockServer,
    schema: Value,
) -> (Box<dyn Provider>, ChatRequest) {
    match seam {
        Seam::AnthropicApi => (Box::new(anthropic_provider(server)), request(Some(schema))),
        Seam::InvokeClient => (
            Box::new(bedrock_provider(server, BedrockApiShape::Invoke, None).await),
            request(Some(schema)),
        ),
        Seam::InvokeOperator => {
            let output_config = json!({"format": {"type": "json_schema", "schema": schema}});
            (
                Box::new(
                    bedrock_provider(server, BedrockApiShape::Invoke, Some(output_config)).await,
                ),
                request(None),
            )
        }
        Seam::Converse => (
            Box::new(bedrock_provider(server, BedrockApiShape::Converse, None).await),
            request(Some(schema)),
        ),
    }
}

/// The error `call` returned, or `None` when it succeeded.
async fn call_error(provider: &dyn Provider, call: Call, req: ChatRequest) -> Option<Error> {
    match call {
        Call::Complete => provider.complete(req).await.err(),
        Call::Stream => provider.stream(req).await.err(),
        Call::CountTokens => provider.count_tokens(req).await.err(),
    }
}

async fn assert_refused_before_send(schema: fn() -> Value, reason: RefusalReason) {
    for seam in SEAMS {
        for call in CALLS {
            // Arrange
            let server = recording_upstream().await;
            let (provider, req) = seam_target(seam, &server, schema()).await;

            // Act
            let err = call_error(provider.as_ref(), call, req).await;

            // Assert
            let received = server
                .received_requests()
                .await
                .expect("read mock requests");
            assert!(
                received.is_empty(),
                "{seam:?} {call:?}: an over-limit schema reached the upstream"
            );
            let Some(err) = err else {
                panic!("{seam:?} {call:?}: an over-limit schema must fail the request");
            };
            assert!(
                matches!(err, Error::NormalizeRequest(..)),
                "{seam:?} {call:?}: got {err:?}"
            );
            assert_eq!(
                err.refusal_reason(),
                Some(reason),
                "{seam:?} {call:?}: got {err:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_schema_past_the_node_limit_is_refused_before_send_on_every_seam() {
    assert_refused_before_send(over_node_limit_schema, RefusalReason::SchemaSize).await;
}

#[tokio::test]
async fn a_schema_past_the_depth_limit_is_refused_before_send_on_every_seam() {
    assert_refused_before_send(over_depth_limit_schema, RefusalReason::SchemaDepth).await;
}

/// The repaired schema root inside the body `seam` shipped.
fn shipped_schema(seam: Seam, body: &Value) -> Option<&Value> {
    match seam {
        Seam::Converse => body.pointer("/additionalModelRequestFields/output_config/format/schema"),
        _ => body.pointer("/output_config/format/schema"),
    }
}

#[tokio::test]
async fn a_schema_within_the_limits_ships_repaired_on_every_seam() {
    for seam in SEAMS {
        // Arrange
        let server = recording_upstream().await;
        let (provider, req) = seam_target(seam, &server, repairable_schema()).await;

        // Act
        let _ = call_error(provider.as_ref(), Call::Complete, req).await;

        // Assert
        let received = server
            .received_requests()
            .await
            .expect("read mock requests");
        assert_eq!(received.len(), 1, "{seam:?}: one upstream call");
        let body: Value = serde_json::from_slice(&received[0].body).expect("JSON body");
        let schema = shipped_schema(seam, &body)
            .unwrap_or_else(|| panic!("{seam:?}: the schema must ship: {body}"));
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "{seam:?}: the walk must run on what ships"
        );
    }
}
