//! Tests for the `additionalProperties: false` repair on the Converse
//! `additionalModelRequestFields.output_config.format.schema`.
//!
//! Anthropic behind Converse applies the same rule as the direct API: every
//! object in a structured-output schema must carry `additionalProperties:
//! false` explicitly, and Bedrock does not fill it in. These tests drive the
//! whole `normalize_request` path so they observe the bag that ships.

use serde_json::{Value, json};

use routectl_core::{ChatRequest, Error, Message, MessageContent, Role};
use routectl_testkit::{CapturedEvent, capture_events};

use super::super::normalize_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

/// Structured `event` field of the repair's not-`false` forward WARN.
const FORWARD_EVENT: &str = "output_schema_additional_properties_not_false";

fn cfg_with_body_fields(allowed_body_fields: Vec<String>) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test-converse".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-sonnet-4-6".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: Vec::new(),
        allowed_body_fields,
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

fn cfg() -> BedrockConfig {
    cfg_with_body_fields(Vec::new())
}

fn req_with_schema(schema: Value) -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-sonnet-4-6".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(1024),
        response_format: Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "widget", "schema": schema}
        })),
        ..Default::default()
    }
}

/// Normalize under a thread-local capture so log assertions see only this
/// request's events.
fn normalized(
    cfg: &BedrockConfig,
    req: &ChatRequest,
) -> (routectl_core::Result<Value>, Vec<CapturedEvent>) {
    let mut result = Ok(Value::Null);
    let events = capture_events(|| {
        result = normalize_request(cfg, req);
    });
    (result, events)
}

fn shipped_schema(body: &Value) -> &Value {
    &body["additionalModelRequestFields"]["output_config"]["format"]["schema"]
}

fn forward_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.field("event") == Some(FORWARD_EVENT))
        .collect()
}

/// Counts every object-shaped node reachable in `value` (any position) and how
/// many of them carry `additionalProperties: false`.
fn count_objects_with_false(value: &Value) -> (usize, usize) {
    let mut objects = 0;
    let mut with_false = 0;
    let mut stack = vec![value];
    while let Some(node) = stack.pop() {
        match node {
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("object") {
                    objects += 1;
                    if map.get("additionalProperties") == Some(&Value::Bool(false)) {
                        with_false += 1;
                    }
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    (objects, with_false)
}

#[test]
fn injects_false_at_every_object_node_of_a_dense_nested_schema() {
    // Arrange: root, a nested property, an array item, anyOf and oneOf
    // branches, a $defs entry, and an object nested two levels down -- none
    // carrying the key.
    let req = req_with_schema(json!({
        "type": "object",
        "properties": {
            "address": {
                "type": "object",
                "properties": {
                    "geo": {
                        "type": "object",
                        "properties": {"lat": {"type": "number"}}
                    }
                }
            },
            "tags": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"label": {"type": "string"}}
                }
            },
            "contact": {
                "anyOf": [
                    {"type": "object", "properties": {"email": {"type": "string"}}},
                    {"type": "object", "properties": {"phone": {"type": "string"}}}
                ]
            },
            "shape": {
                "oneOf": [
                    {"type": "object", "properties": {"r": {"type": "number"}}}
                ]
            }
        },
        "$defs": {
            "money": {"type": "object", "properties": {"cents": {"type": "integer"}}}
        }
    }));

    // Act
    let (result, _) = normalized(&cfg(), &req);
    let body = result.expect("a dense in-bound schema must normalize");

    // Assert
    let schema = shipped_schema(&body);
    let (objects, with_false) = count_objects_with_false(schema);
    assert_eq!(
        objects, 8,
        "fixture must exercise eight object nodes: {schema}"
    );
    assert_eq!(
        with_false, objects,
        "every object node must carry additionalProperties: false: {schema}"
    );
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(
        schema["properties"]["tags"]["items"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        schema["properties"]["contact"]["anyOf"][1]["additionalProperties"],
        json!(false)
    );
}

#[test]
fn leaves_non_object_schema_nodes_untouched() {
    // Arrange
    let req = req_with_schema(json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "status": {"enum": [{"type": "object"}]}
        }
    }));

    // Act
    let (result, _) = normalized(&cfg(), &req);
    let body = result.unwrap();

    // Assert
    let schema = shipped_schema(&body);
    assert_eq!(schema["additionalProperties"], json!(false));
    assert!(
        schema["properties"]["name"]
            .get("additionalProperties")
            .is_none()
    );
    assert_eq!(
        schema["properties"]["status"]["enum"][0],
        json!({"type": "object"}),
        "instance data inside enum must stay byte-identical"
    );
}

#[test]
fn forwards_an_explicit_additional_properties_true_verbatim_with_one_warn() {
    // Arrange: a nested object opts into true; its sibling omits the key.
    let req = req_with_schema(json!({
        "type": "object",
        "properties": {
            "bag": {"type": "object", "additionalProperties": true},
            "fixed": {"type": "object", "properties": {"x": {"type": "string"}}}
        }
    }));

    // Act
    let (result, events) = normalized(&cfg(), &req);
    let body = result.unwrap();

    // Assert
    let schema = shipped_schema(&body);
    assert_eq!(
        schema["properties"]["bag"]["additionalProperties"],
        json!(true),
        "a caller's explicit true is never rewritten"
    );
    assert_eq!(
        schema["properties"]["fixed"]["additionalProperties"],
        json!(false),
        "the sibling that omitted the key is still repaired"
    );
    let warns = forward_warns(&events);
    assert_eq!(warns.len(), 1, "expected exactly one WARN; got: {events:?}");
    assert_eq!(warns[0].level, tracing::Level::WARN);
    assert_eq!(warns[0].field("provider"), Some("bedrock:test-converse"));
}

#[test]
fn emits_no_forward_warn_when_every_present_value_is_false() {
    // Arrange
    let req = req_with_schema(json!({
        "type": "object",
        "properties": {"inner": {"type": "object"}}
    }));

    // Act
    let (result, events) = normalized(&cfg(), &req);

    // Assert
    assert!(result.is_ok());
    assert!(forward_warns(&events).is_empty(), "got: {events:?}");
}

#[test]
fn does_not_repair_an_output_config_dropped_by_allowed_body_fields() {
    // Arrange: the allowlist omits output_config, so the bag ships without it.
    let cfg = cfg_with_body_fields(vec!["thinking".into(), "anthropic_beta".into()]);
    let req = req_with_schema(json!({
        "type": "object",
        "properties": {"inner": {"type": "object"}}
    }));

    // Act
    let (result, events) = normalized(&cfg, &req);
    let body = result.unwrap();

    // Assert
    assert!(
        !body.to_string().contains("additionalProperties"),
        "nothing may be injected into a bag that ships no output_config: {body}"
    );
    assert!(forward_warns(&events).is_empty(), "got: {events:?}");
}

/// A schema nested one level past the repair walk's depth limit (256).
fn over_depth_schema() -> Value {
    let mut schema = json!({"type": "object", "properties": {}});
    for _ in 0..=256 {
        schema = json!({"type": "object", "properties": {"a": schema}});
    }
    schema
}

#[test]
fn an_over_bound_schema_fails_locally_with_a_normalize_error() {
    // Arrange
    let req = req_with_schema(over_depth_schema());

    // Act
    let (result, _) = normalized(&cfg(), &req);

    // Assert: the error surfaces from request building, which every send
    // path runs before it opens a connection.
    let Err(err) = result else {
        panic!("an over-bound schema must fail request normalization");
    };
    assert!(matches!(err, Error::NormalizeRequest(..)), "got: {err:?}");
    assert!(err.to_string().contains("nests deeper"), "got: {err}");
}

#[test]
fn an_over_bound_schema_in_a_dropped_output_config_does_not_fail() {
    // Arrange: the repair runs on the shipped bag, so a schema the allowlist
    // removes is never walked and cannot fail the request.
    let cfg = cfg_with_body_fields(vec!["thinking".into(), "anthropic_beta".into()]);
    let req = req_with_schema(over_depth_schema());

    // Act
    let (result, _) = normalized(&cfg, &req);

    // Assert
    let body = result.expect("a dropped output_config must not be walked");
    assert!(
        body["additionalModelRequestFields"]
            .get("output_config")
            .is_none()
    );
}
