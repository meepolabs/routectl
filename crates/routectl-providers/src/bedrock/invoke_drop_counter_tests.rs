//! Translation telemetry of the Invoke lane: the request-volume denominator,
//! and the unrecognized `response_format` drop counted under this lane rather
//! than the anthropic lane whose assembly builds the body.
//!
//! SERIAL GUARDS: the registry is process-global and the runner is threaded.
//! Every test reaching `(bedrock-invoke, response_format_unrepresentable)`
//! carries `bedrock_invoke_response_format_unrepresentable`; the one reading
//! the anthropic counter back also carries that lane's guard.

use serde_json::json;

use routectl_core::{ChatRequest, Message, MessageContent, Role};

use super::{LANE, normalize_request};
use crate::anthropic_api::request_drop_counter_tests::schema_past_the_repair_depth_bound;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const DROP_CLASS: &str = "response_format_unrepresentable";

fn cfg() -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-haiku-4-5".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

fn user_req() -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

fn drop_count(lane: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == lane && e.drop_class == DROP_CLASS)
        .map_or(0, |e| e.drop_count)
}

/// Every Invoke request counts toward this lane's denominator, including one
/// that fails assembly. A LOWER BOUND, as on the anthropic lane: every Invoke
/// normalization in the suite bumps this one key, and the registry only moves
/// up, so an unwired site still reads a delta of 0.
#[test]
fn every_invoke_request_counts_toward_the_lane_denominator() {
    // Arrange: one clean request and one the replay invariant rejects.
    let clean = user_req();
    let mut rejected = user_req();
    rejected.messages = vec![Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Text("result".into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }]
    .into();

    // Act
    let before = crate::translation_drop_metrics::translation_lane_seen(LANE);
    let _ = normalize_request(&cfg(), &clean).expect("the clean request assembles");
    let _ = normalize_request(&cfg(), &rejected).expect_err("the malformed request is rejected");
    let after = crate::translation_drop_metrics::translation_lane_seen(LANE);

    // Assert
    assert!(after - before >= 2, "delta was {}", after - before);
}

/// SINGLE-SOURCE pin for this lane: the directive is absent from the body
/// that ships, counted once here, and credited to no other lane.
#[test]
#[serial_test::serial(
    bedrock_invoke_response_format_unrepresentable,
    anthropic_response_format_unrepresentable
)]
fn an_unrecognized_response_format_is_absent_from_the_invoke_body_and_counted() {
    // Arrange
    let mut req = user_req();
    req.response_format = Some(json!({"type": "zz_grammar_mode"}));

    // Act
    let (invoke_before, anthropic_before) = (drop_count(LANE), drop_count("anthropic"));
    let body = normalize_request(&cfg(), &req).expect("invoke body assembles");
    let (invoke_after, anthropic_after) = (drop_count(LANE), drop_count("anthropic"));

    // Assert
    let rendered = serde_json::to_string(&body).expect("body serializes");
    assert!(!rendered.contains("zz_grammar_mode"), "{rendered}");
    assert!(body.get("output_config").is_none(), "{rendered}");
    assert_eq!(invoke_after - invoke_before, 1);
    assert_eq!(
        anthropic_after, anthropic_before,
        "an Invoke request is not anthropic traffic"
    );
}

/// The flush runs on the Err arm of the shared assembly too: the directive is
/// dropped by the converter, then the schema repair rejects the request.
#[test]
#[serial_test::serial(bedrock_invoke_response_format_unrepresentable)]
fn an_unrecognized_response_format_still_counts_when_invoke_assembly_fails() {
    // Arrange
    let mut req = user_req();
    req.response_format = Some(json!({"type": "zz_grammar_mode"}));
    req.provider_extras = Some(schema_past_the_repair_depth_bound());

    // Act
    let before = drop_count(LANE);
    let outcome = normalize_request(&cfg(), &req);
    let after = drop_count(LANE);

    // Assert
    assert!(outcome.is_err(), "the fixture must fail assembly");
    assert_eq!(after - before, 1);
}

/// POSITIVE CONTROL: a mapped directive leaves the counter untouched.
#[test]
#[serial_test::serial(bedrock_invoke_response_format_unrepresentable)]
fn a_mapped_response_format_counts_no_invoke_drop() {
    // Arrange
    let mut req = user_req();
    req.response_format = Some(json!({"type": "json_object"}));

    // Act
    let before = drop_count(LANE);
    let body = normalize_request(&cfg(), &req).expect("invoke body assembles");
    let after = drop_count(LANE);

    // Assert
    assert_eq!(body["output_config"]["format"]["type"], "json_object");
    assert_eq!(after, before);
}
