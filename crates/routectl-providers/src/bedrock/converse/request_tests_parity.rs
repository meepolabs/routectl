//! Cross-egress parity tests for `super::request::translate`.
//!
//! Lives in a sibling file so `request.rs` stays under the project's
//! 800-line ceiling. Imported via `#[path = "request_tests_parity.rs"]
//! mod tests_parity;` from `request.rs`.
//!
//! Coverage:
//!   - Thinking-mode sampling clamp: a reasoning-enabled Converse request
//!     forces `temperature = 1.0` and drops `top_p`, matching the
//!     Anthropic-API + Bedrock-Invoke seams (shared clamp helper).
//!   - Non-thinking requests pass `temperature` / `top_p` through
//!     unchanged (temperature still wins over top_p per Claude 4.x).
//!   - `req.response_format` is honored onto the Converse
//!     `additionalModelRequestFields.output_config.format` bag key.

use super::super::normalize_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
use routectl_core::{
    ChatRequest, CustomTool, Message, MessageContent, ReasoningConfig, Role, ToolDef,
};
use serde_json::json;

fn fake_cfg() -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test-converse".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-sonnet-4-5".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: Vec::new(),
        allowed_body_fields: vec![
            "thinking".into(),
            "output_config".into(),
            "anthropic_beta".into(),
        ],
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

fn user_msg(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn reasoning_enabled() -> ReasoningConfig {
    ReasoningConfig {
        effort: Some("medium".into()),
        max_tokens: None,
        exclude: None,
        enabled: Some(true),
    }
}

#[test]
fn thinking_enabled_forces_temperature_one_and_drops_top_p() {
    // Arrange: reasoning enabled with a legacy-fitting max_tokens, plus a
    // caller temperature of 0.5. Claude rejects thinking paired with a
    // non-1.0 temperature, so the clamp must override it.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        temperature: Some(0.5),
        reasoning: Some(reasoning_enabled()),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: temperature clamped to 1.0, top_p omitted, thinking present.
    let inf = &body["inferenceConfig"];
    assert_eq!(inf["temperature"], 1.0, "got: {body}");
    assert!(inf.get("topP").is_none(), "top_p must be dropped: {body}");
    assert_eq!(
        body["additionalModelRequestFields"]["thinking"]["type"], "enabled",
        "thinking must be composed: {body}"
    );
}

#[test]
fn thinking_enabled_drops_caller_top_p() {
    // Arrange: reasoning enabled, caller sets top_p (no temperature).
    // Claude forbids top_p while thinking, so the forced temperature=1.0
    // wins and top_p is dropped.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        top_p: Some(0.9),
        reasoning: Some(reasoning_enabled()),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert
    let inf = &body["inferenceConfig"];
    assert_eq!(inf["temperature"], 1.0, "got: {body}");
    assert!(inf.get("topP").is_none(), "top_p must be dropped: {body}");
}

#[test]
fn non_thinking_passes_temperature_through() {
    // Arrange: no reasoning -> no clamp. temperature passes through; top_p
    // is still dropped because temperature wins (pre-existing Claude 4.x
    // temperature-xor-top_p rule).
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        temperature: Some(0.5),
        top_p: Some(0.9),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert
    let inf = &body["inferenceConfig"];
    assert_eq!(inf["temperature"], 0.5, "got: {body}");
    assert!(
        inf.get("topP").is_none(),
        "top_p must be dropped when temperature is set: {body}"
    );
    assert!(
        body.get("additionalModelRequestFields").is_none(),
        "no thinking, no bag: {body}"
    );
}

#[test]
fn non_thinking_passes_top_p_through_when_no_temperature() {
    // Arrange: no reasoning, only top_p -> passes through unchanged.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        top_p: Some(0.9),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert
    let inf = &body["inferenceConfig"];
    assert_eq!(inf["topP"], 0.9, "got: {body}");
    assert!(inf.get("temperature").is_none(), "got: {body}");
}

#[test]
fn response_format_maps_to_output_config_format_bag() {
    // Arrange: a canonical json_schema response_format directive must land
    // on additionalModelRequestFields.output_config.format for Claude.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        response_format: Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "widget",
                "schema": {"type": "object", "required": ["x"]},
                "strict": true
            }
        })),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert
    let fmt = &body["additionalModelRequestFields"]["output_config"]["format"];
    assert_eq!(fmt["type"], "json_schema", "got: {body}");
    assert_eq!(fmt["schema"]["required"][0], "x", "got: {body}");
    assert_eq!(fmt["name"], "widget", "got: {body}");
    assert_eq!(fmt["strict"], true, "got: {body}");
}

#[test]
fn response_format_json_object_maps_to_output_config_format_bag() {
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        response_format: Some(json!({"type": "json_object"})),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    assert_eq!(
        body["additionalModelRequestFields"]["output_config"]["format"]["type"], "json_object",
        "got: {body}"
    );
}

#[test]
fn response_format_coexists_with_adaptive_effort_in_bag() {
    // Arrange: adaptive thinking writes output_config.effort; a
    // response_format must merge its format sibling without clobbering it.
    let mut cfg = fake_cfg();
    cfg.adaptive_thinking = Some(true);
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        reasoning: Some(reasoning_enabled()),
        response_format: Some(json!({"type": "json_object"})),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: both effort and format present under output_config.
    let oc = &body["additionalModelRequestFields"]["output_config"];
    assert_eq!(oc["format"]["type"], "json_object", "got: {body}");
    assert_eq!(oc["effort"], "medium", "got: {body}");
}

fn custom_tool(name: &str) -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: name.into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

#[test]
fn required_tool_choice_strips_thinking_and_preserves_caller_top_p() {
    // Arrange: reasoning enabled + tool_choice "required" + top_p only.
    // "required" translates to a forcing Converse toolChoice (Any), which
    // strips thinking from the bag. With no thinking on the wire, the
    // sampling clamp must NOT fire -- the caller's top_p must survive
    // rather than being dropped for a phantom (stripped) thinking block.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        top_p: Some(0.9),
        reasoning: Some(reasoning_enabled()),
        tools: Some(vec![custom_tool("calc")]),
        tool_choice: Some(json!("required")),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: thinking stripped, and caller top_p preserved (not clamped).
    assert!(
        body["additionalModelRequestFields"]
            .get("thinking")
            .is_none(),
        "thinking must be stripped when toolChoice forces a tool: {body}"
    );
    let inf = &body["inferenceConfig"];
    assert_eq!(
        inf["topP"], 0.9,
        "caller top_p must survive once thinking is stripped: {body}"
    );
    assert!(
        inf.get("temperature").is_none(),
        "temperature must not be forced to 1.0 when no thinking ships: {body}"
    );
}

#[test]
fn required_tool_choice_strips_thinking_and_preserves_caller_temperature() {
    // Arrange: reasoning enabled + tool_choice "required" + explicit
    // temperature. Thinking is stripped by the forcing toolChoice, so the
    // caller's temperature must pass through unclamped (top_p still dropped
    // by the pre-existing temperature-xor-top_p rule).
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        temperature: Some(0.3),
        top_p: Some(0.9),
        reasoning: Some(reasoning_enabled()),
        tools: Some(vec![custom_tool("calc")]),
        tool_choice: Some(json!("required")),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert
    assert!(
        body["additionalModelRequestFields"]
            .get("thinking")
            .is_none(),
        "thinking must be stripped when toolChoice forces a tool: {body}"
    );
    let inf = &body["inferenceConfig"];
    assert_eq!(
        inf["temperature"], 0.3,
        "caller temperature must survive once thinking is stripped: {body}"
    );
    assert!(
        inf.get("topP").is_none(),
        "top_p dropped because temperature is set: {body}"
    );
}

#[test]
fn auto_tool_choice_keeps_thinking_and_clamps_sampling() {
    // Regression guard for the strip/clamp interaction: an Auto toolChoice
    // does NOT strip thinking, so the clamp must still fire (temperature
    // forced to 1.0, top_p dropped) -- the fix must not disable clamping
    // for surviving-thinking requests.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        top_p: Some(0.9),
        reasoning: Some(reasoning_enabled()),
        tools: Some(vec![custom_tool("calc")]),
        tool_choice: Some(json!("auto")),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: thinking survives, clamp applied.
    assert_eq!(
        body["additionalModelRequestFields"]["thinking"]["type"], "enabled",
        "thinking must survive on Auto: {body}"
    );
    let inf = &body["inferenceConfig"];
    assert_eq!(inf["temperature"], 1.0, "clamp must fire: {body}");
    assert!(inf.get("topP").is_none(), "top_p dropped by clamp: {body}");
}

#[test]
fn response_format_survives_malformed_provider_extras_output_config() {
    // Arrange: a malformed forward-compat sweep leaves output_config as a
    // non-object (null) in provider_extras, while the caller also asks for
    // structured output. The Converse egress must still emit
    // output_config.format rather than dropping the directive.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        response_format: Some(json!({"type": "json_object"})),
        provider_extras: Some(json!({"output_config": null})),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert
    assert_eq!(
        body["additionalModelRequestFields"]["output_config"]["format"]["type"], "json_object",
        "structured-output format must survive a null provider_extras output_config: {body}"
    );
}
