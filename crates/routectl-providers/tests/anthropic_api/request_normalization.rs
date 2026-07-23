//! Request normalization tests: system lift, reasoning budget, and tool shape.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn system_message_lifted_to_top_level() {
    let provider = make_provider("https://api.anthropic.com");
    let req = base_req(
        "claude-3-opus",
        vec![
            system_msg("You are a helpful assistant."),
            user_msg("Hello!"),
        ],
    );
    let body = provider.normalize_request(&req).unwrap();

    // top-level system field must be present
    assert_eq!(body["system"], "You are a helpful assistant.");

    // messages array must contain only the user message
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], "user");

    // no system role in messages
    for m in msgs {
        assert_ne!(m["role"], "system");
    }
}

#[test]
fn legacy_system_lift_skips_non_text_content() {
    // A Role::System message with Parts content (image/document/etc.)
    // or Null must NOT produce `system: ""` upstream. The legacy lift
    // returns None when no meaningful text is found, so the top-level
    // `system` field is absent rather than an empty string.
    let provider = make_provider("https://api.anthropic.com");
    let req = base_req(
        "claude-3-opus",
        vec![
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Image {
                    source: serde_json::json!({"type": "url", "url": "https://example/x.png"}),
                    cache_control: None,
                })]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            user_msg("Hello!"),
        ],
    );
    let body = provider.normalize_request(&req).unwrap();
    assert!(
        body.get("system").is_none(),
        "expected absent `system`, got {:?}",
        body.get("system")
    );
}

#[test]
fn legacy_system_lift_extracts_text_from_parts() {
    // A Role::System message with Parts containing a text block
    // should still lift -- we extract the text content rather than
    // dropping the whole message.
    let provider = make_provider("https://api.anthropic.com");
    let req = base_req(
        "claude-3-opus",
        vec![
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                    text: "primary system".into(),
                    citations: None,
                    cache_control: None,
                })]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            user_msg("Hello!"),
        ],
    );
    let body = provider.normalize_request(&req).unwrap();
    assert_eq!(body["system"], "primary system");
}

#[test]
fn reasoning_max_tokens_maps_to_budget_tokens() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    // Bump request max_tokens above the explicit budget so the
    // ceiling cap in `clamp_budget_to_legacy_window` (which keeps
    // budget < max_tokens) does NOT lower the caller's value.
    req.max_tokens = Some(8192);
    req.reasoning = Some(ReasoningConfig {
        max_tokens: Some(5000),
        ..Default::default()
    });
    let body = provider.normalize_request(&req).unwrap();

    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 5000);
}

#[test]
fn reasoning_effort_high_maps_to_exact_table_budget() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    req.max_tokens = Some(10000);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        ..Default::default()
    });
    let body = provider.normalize_request(&req).unwrap();

    assert_eq!(body["thinking"]["type"], "enabled");
    // table("high")=24576 clamped to window ceiling max_tokens-1 = 9999.
    assert_eq!(body["thinking"]["budget_tokens"], 9999u64);
}

#[test]
fn reasoning_effort_none_disables_thinking() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("none".into()),
        ..Default::default()
    });
    let body = provider.normalize_request(&req).unwrap();

    assert_eq!(body["thinking"]["type"], "disabled");
}

#[test]
fn tools_translated_to_anthropic_shape() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    req.tools = Some(vec![ToolDef::Other(json!({
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the current weather",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": {"type": "string"}
                },
                "required": ["location"]
            }
        }
    }))]);
    let body = provider.normalize_request(&req).unwrap();

    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    // Anthropic shape: name, description, input_schema (no 'type' or 'function' wrapper)
    assert_eq!(tool["name"], "get_weather");
    assert_eq!(tool["description"], "Get the current weather");
    assert!(tool.get("input_schema").is_some());
    assert_eq!(
        tool["input_schema"]["properties"]["location"]["type"],
        "string"
    );
    // No 'parameters' key in Anthropic shape
    assert!(tool.get("parameters").is_none());
    assert!(tool.get("function").is_none());
}
