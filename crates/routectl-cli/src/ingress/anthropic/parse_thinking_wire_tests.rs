//! A client `thinking` object, parsed by this ingress, reaches every
//! Anthropic-shape wire (anthropic-api, Bedrock Invoke, Bedrock Converse)
//! as sent, `display` included, whatever its `type`. Known types are pinned
//! against the committed driver captures as the positive control.

use routectl_core::Provider;
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
use serde_json::{Value, json};

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;

use super::*;

fn parse(body: Value) -> ChatRequest {
    AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("request should parse")
}

fn request_with(thinking: Value) -> Value {
    json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 4096,
        "thinking": thinking,
    })
}

fn bedrock_cfg(api_shape: BedrockApiShape) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-opus-4-7".into(),
        api_shape,
        creds: BedrockCreds::BearerKey { key: "k".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

/// `(lane, the wire thinking)` for each Anthropic-shape egress.
fn wire_thinking_per_lane(req: &ChatRequest) -> Vec<(&'static str, Value)> {
    let anthropic = AnthropicApiProvider::new(AnthropicApiConfig::new("anthropic:test", "k"))
        .normalize_request(req)
        .expect("anthropic-api body");
    let invoke = routectl_providers::bedrock::invoke::normalize_request(
        &bedrock_cfg(BedrockApiShape::Invoke),
        req,
    )
    .expect("invoke body");
    let converse = routectl_providers::bedrock::converse::normalize_request(
        &bedrock_cfg(BedrockApiShape::Converse),
        req,
    )
    .expect("converse body");
    vec![
        ("anthropic-api", anthropic["thinking"].clone()),
        ("bedrock-invoke", invoke["thinking"].clone()),
        (
            "bedrock-converse",
            converse["additionalModelRequestFields"]["thinking"].clone(),
        ),
    ]
}

fn assert_byte_identical(expected: &Value, wire: &Value, row: &str) {
    assert_eq!(
        serde_json::to_string(wire).expect("serialize"),
        serde_json::to_string(expected).expect("serialize"),
        "{row}"
    );
}

#[test]
fn an_unknown_thinking_type_reaches_every_anthropic_shape_wire_as_sent() {
    for thinking in [
        json!({"type": "future_mode", "depth": 3, "display": "summarized"}),
        json!({"type": "future_mode", "display": "future_display"}),
        json!({"type": "future_mode"}),
    ] {
        // Arrange
        let req = parse(request_with(thinking.clone()));

        // Act
        let lanes = wire_thinking_per_lane(&req);

        // Assert
        for (lane, wire) in lanes {
            assert_byte_identical(&thinking, &wire, &format!("{lane}: {thinking}"));
        }
    }
}

fn driver_fixture(case: &str, file: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/driver/anthropic-api")
        .join(case)
        .join(file);
    let text = std::fs::read_to_string(&path).expect("driver fixture must read");
    serde_json::from_str(&text).expect("driver fixture must parse")
}

#[test]
fn known_thinking_types_match_the_committed_driver_captures_on_every_lane() {
    for case in ["thinking-01", "plain-turn-01"] {
        // Arrange
        let ingress = driver_fixture(case, "ingress_request.json");
        let outgoing = driver_fixture(case, "outgoing_request.json");
        let mut body = request_with(ingress["thinking"].clone());
        if let Some(output_config) = ingress.get("output_config") {
            body["output_config"] = output_config.clone();
        }
        let req = parse(body);

        // Act
        let lanes = wire_thinking_per_lane(&req);

        // Assert
        for (lane, wire) in lanes {
            assert_byte_identical(&outgoing["thinking"], &wire, &format!("{lane}: {case}"));
        }
    }
}
