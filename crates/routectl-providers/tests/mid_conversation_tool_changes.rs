//! Mid-conversation tool changes on the two Anthropic-shape egresses.
//!
//! A client announces a tool added or removed mid-conversation as a
//! `tool_addition` / `tool_removal` block inside an in-band `role: "system"`
//! turn. The canonical model has no type for it: the block deserializes as
//! `ContentPart::Other` and rides inside a `Role::System` message. Both lanes
//! below receive the identical request (a top-level system plus that turn).
//!
//! - anthropic-api forwards the turn in place, block and text verbatim, and
//!   leaves the upstream to accept or refuse the block.
//! - Bedrock Invoke never ships an in-band system turn, and with a top-level
//!   system present the legacy lift does not run either, so the whole turn,
//!   text included, is LOST from the Invoke body. This is a measured loss,
//!   not an intended edit: nothing declares it and nothing refuses locally.

#![cfg(feature = "bedrock")]

use routectl_core::{
    ChatRequest, Message, MessageContent, Provider, Role, content_part::ContentPart,
    system_content::SystemContent,
};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth::ResolvedCreds,
};
use serde_json::{Value, json};

const TOP_LEVEL_SYSTEM: &str = "you are a coding assistant";
const TOOL_CHANGE_TEXT: &str = "the lookup_weather tool is now available";
const ADDED_TOOL_NAME: &str = "lookup_weather";

fn anthropic_api_provider() -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "tool-change-anthropic".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
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
        mantle: None,
    })
}

fn bedrock_invoke_provider() -> BedrockProvider {
    let cfg = BedrockConfig {
        id: "tool-change-invoke".into(),
        region: "us-east-1".into(),
        model_id: "anthropic.claude-3-opus-20240229-v1:0".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey {
            key: "test-key".into(),
        },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = ResolvedCreds::Bearer {
        key: "test-key".into(),
    };
    BedrockProvider::new(cfg, resolved).expect("canonical region")
}

const fn message(role: Role, content: MessageContent) -> Message {
    Message {
        refusal: None,
        role,
        content,
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// The tool-change turn's content, decoded from client wire JSON through
/// `ContentPart`'s own deserializer so the block takes the shape ingress
/// gives it.
fn tool_change_parts() -> Vec<ContentPart> {
    let parts: Vec<ContentPart> = serde_json::from_value(json!([
        {"type": "tool_addition", "tools": [{"name": ADDED_TOOL_NAME}]},
        {"type": "text", "text": TOOL_CHANGE_TEXT},
    ]))
    .expect("client blocks decode");
    assert!(
        matches!(&parts[0], ContentPart::Other { type_tag, .. } if type_tag == "tool_addition"),
        "the tool-change block must decode as the forward-compat catchall: {parts:?}"
    );
    parts
}

/// A top-level system plus a tool-change system turn at `messages[1]`.
fn request_with_tool_change_turn() -> ChatRequest {
    ChatRequest {
        model: "claude-opus-4-8".into(),
        system: Some(SystemContent::Text(TOP_LEVEL_SYSTEM.into())),
        messages: vec![
            message(Role::User, MessageContent::Text("first".into())),
            message(Role::System, MessageContent::Parts(tool_change_parts())),
            message(Role::Assistant, MessageContent::Text("noted".into())),
            message(Role::User, MessageContent::Text("second".into())),
        ]
        .into(),
        max_tokens: Some(2048),
        ..Default::default()
    }
}

fn wire_roles(body: &Value) -> Vec<&str> {
    body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|m| m["role"].as_str().expect("role string"))
        .collect()
}

/// anthropic-api carries the tool change: the system turn stays at its
/// original position with the `tool_addition` block and the text, in order
/// and verbatim. Also the positive control for the Invoke loss below: the
/// same request does put both on a wire body.
#[test]
fn anthropic_api_carries_the_tool_change_turn_in_place() {
    // Arrange
    let req = request_with_tool_change_turn();

    // Act
    let body = anthropic_api_provider()
        .normalize_request(&req)
        .expect("anthropic-api normalize");

    // Assert
    assert_eq!(body["system"], TOP_LEVEL_SYSTEM);
    assert_eq!(
        wire_roles(&body),
        ["user", "system", "assistant", "user"],
        "got: {body}"
    );
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            {"type": "tool_addition", "tools": [{"name": ADDED_TOOL_NAME}]},
            {"type": "text", "text": TOOL_CHANGE_TEXT},
        ])
    );
}

/// LOSS on Bedrock Invoke: the tool-change system turn is dropped whole.
/// Neither the `tool_addition` block nor the turn's text reaches the body,
/// the top-level system is unchanged (nothing is lifted into it), and the
/// rest of the history keeps its order. The model is never told the tool
/// set changed.
#[test]
fn the_tool_change_turn_is_a_loss_on_bedrock_invoke() {
    // Arrange
    let req = request_with_tool_change_turn();

    // Act
    let body = bedrock_invoke_provider()
        .normalize_request(&req)
        .expect("bedrock invoke normalize");

    // Assert
    assert_eq!(body["system"], TOP_LEVEL_SYSTEM);
    assert_eq!(
        wire_roles(&body),
        ["user", "assistant", "user"],
        "got: {body}"
    );
    let wire = body.to_string();
    for lost in ["tool_addition", ADDED_TOOL_NAME, TOOL_CHANGE_TEXT] {
        assert!(
            !wire.contains(lost),
            "the Invoke body unexpectedly carries {lost:?}: {body}"
        );
    }
}
