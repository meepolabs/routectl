//! A request carrying a `tool_choice` but an empty tool list must never ship
//! the `tool_choice` alone: Anthropic rejects the pairing, so every such
//! request would fail. Checked on the body Invoke actually ships.

use serde_json::{Value, json};

use routectl_core::{ChatRequest, CustomTool, Message, MessageContent, Role, ToolDef};

use super::normalize_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const FUNCTION_NAME: &str = "get_weather";

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
        allowed_betas: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

fn function_tool() -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: FUNCTION_NAME.into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

fn request_with(tools: Vec<ToolDef>, tool_choice: Value) -> ChatRequest {
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
        tools: Some(tools),
        tool_choice: Some(tool_choice),
        ..Default::default()
    }
}

fn choices() -> [Value; 2] {
    [
        json!({"type": "mcp", "server_label": "docs", "name": "search"}),
        json!({"type": "tool", "name": FUNCTION_NAME}),
    ]
}

#[test]
fn an_empty_tool_list_takes_the_tool_choice_with_it() {
    for choice in choices() {
        // Arrange
        let cfg = cfg();
        let req = request_with(Vec::new(), choice.clone());

        // Act
        let body = normalize_request(&cfg, &req).expect("invoke body assembles");

        // Assert
        assert!(
            body.get("tools")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty),
            "{choice}: {body}"
        );
        assert!(
            body.get("tool_choice").is_none(),
            "{choice}: a tool_choice with no tools must not ship: {body}"
        );
    }
}

#[test]
fn a_non_empty_tool_list_ships_both() {
    // Arrange -- control: the same request with a tool to select.
    let cfg = cfg();
    let req = request_with(
        vec![function_tool()],
        json!({"type": "tool", "name": FUNCTION_NAME}),
    );

    // Act
    let body = normalize_request(&cfg, &req).expect("invoke body assembles");

    // Assert
    assert_eq!(body["tools"][0]["name"], FUNCTION_NAME, "{body}");
    assert_eq!(
        body["tool_choice"],
        json!({"type": "tool", "name": FUNCTION_NAME}),
        "{body}"
    );
}
