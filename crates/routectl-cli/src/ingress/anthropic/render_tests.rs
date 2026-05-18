use super::*;
use serde_json::json;

#[test]
fn render_response_emits_messages_shape() {
    use routectl_core::{schema::Choice, Message, Role, Usage};
    let resp = ChatResponse {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("hi there".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
        }],
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
    };
    let v = AnthropicIngress.render_response(resp).unwrap();
    assert_eq!(v["id"], "msg_01");
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "hi there");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["usage"]["input_tokens"], 10);
    assert_eq!(v["usage"]["output_tokens"], 5);
}

/// Bug D (cc-via-* 2026-05-18): openai-responses and anthropic-api
/// non-streaming responses populate BOTH `msg.tool_calls`
/// (OpenAI shape) AND a typed `ContentPart::ToolUse` on
/// `msg.content` for the same upstream function_call. Without
/// dedup, the renderer emits two identical tool_use blocks
/// back-to-back in the Anthropic `content` array. Pin: only ONE
/// tool_use block per call_id, with the parts-native shape
/// preserved.
#[test]
fn render_response_dedupes_tool_use_when_present_in_both_tool_calls_and_parts() {
    use routectl_core::{
        schema::Choice, ContentPart, KnownContentPart, Message, MessageContent, Role, Usage,
    };
    let resp = ChatResponse {
        id: "msg_dup".into(),
        model: "gpt-5".into(),
        created: 0,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "I'll compute that".into(),
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::ToolUse {
                        id: "call_dup".into(),
                        name: "calculator".into(),
                        input: json!({"x": 1, "y": 2}),
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_dup",
                    "type": "function",
                    "function": {
                        "name": "calculator",
                        "arguments": "{\"x\":1,\"y\":2}"
                    }
                })]),
            },
            finish_reason: Some("tool_calls".into()),
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
    };
    let v = AnthropicIngress.render_response(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    // Count tool_use blocks for the dup id.
    let tool_uses_for_id: Vec<&Value> = content
        .iter()
        .filter(|b| b["type"] == "tool_use" && b["id"] == "call_dup")
        .collect();
    assert_eq!(
        tool_uses_for_id.len(),
        1,
        "expected exactly one tool_use block for call_dup, got content: {content:?}",
    );
    // The surviving block carries Anthropic-native shape (parts source).
    assert_eq!(tool_uses_for_id[0]["name"], "calculator");
    assert_eq!(tool_uses_for_id[0]["input"]["x"], 1);
    assert_eq!(tool_uses_for_id[0]["input"]["y"], 2);
    // Text block also rendered.
    let text_blocks: Vec<&Value> = content.iter().filter(|b| b["type"] == "text").collect();
    assert_eq!(text_blocks.len(), 1);
    assert_eq!(text_blocks[0]["text"], "I'll compute that");
}

/// Counterpart: openai-compat populates ONLY `msg.tool_calls`
/// (parts is empty / Text). The renderer must still emit one
/// tool_use block per call so this code path doesn't regress
/// when the dedup set is empty.
#[test]
fn render_response_emits_tool_use_from_tool_calls_when_parts_has_no_tool_use() {
    use routectl_core::{schema::Choice, Message, MessageContent, Role, Usage};
    let resp = ChatResponse {
        id: "msg_oc".into(),
        model: "qwen-3-coder".into(),
        created: 0,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                // openai-compat doesn't populate parts.ToolUse; content
                // is the model's plain text reply.
                content: MessageContent::Text("running tool now".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_oc",
                    "type": "function",
                    "function": {
                        "name": "ls",
                        "arguments": "{\"path\":\"/tmp\"}"
                    }
                })]),
            },
            finish_reason: Some("tool_calls".into()),
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
    };
    let v = AnthropicIngress.render_response(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    let tool_uses: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
    assert_eq!(tool_uses.len(), 1);
    assert_eq!(tool_uses[0]["id"], "call_oc");
    assert_eq!(tool_uses[0]["name"], "ls");
    assert_eq!(tool_uses[0]["input"]["path"], "/tmp");
}

/// Review follow-up to Bug D: the pre-scan must also recognize
/// `ContentPart::Other` entries whose `type_tag` is "tool_use".
/// A future Anthropic sub-field on the tool_use block would
/// cause the deserializer to fall through to Other; without
/// this branch, the dedup HashSet is blind to it and a
/// duplicate emit reappears on the all-Anthropic path.
#[test]
fn render_response_dedupes_tool_use_when_parts_carries_other_typed_tool_use() {
    use routectl_core::{schema::Choice, ContentPart, Message, MessageContent, Role, Usage};
    let mut extras = serde_json::Map::new();
    extras.insert("id".into(), Value::String("call_future".into()));
    extras.insert("name".into(), Value::String("future_tool".into()));
    extras.insert("input".into(), json!({"k": "v"}));
    // Hypothetical future sub-field that breaks
    // KnownContentPart::ToolUse's serde struct.
    extras.insert("future_subfield".into(), Value::Bool(true));
    let resp = ChatResponse {
        id: "msg_future".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Other {
                    type_tag: "tool_use".into(),
                    cache_control: None,
                    extras,
                }]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_future",
                    "type": "function",
                    "function": {"name": "future_tool", "arguments": "{\"k\":\"v\"}"}
                })]),
            },
            finish_reason: Some("tool_calls".into()),
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
    };
    let v = AnthropicIngress.render_response(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    let tool_uses_for_id: Vec<&Value> = content
        .iter()
        .filter(|b| b["type"] == "tool_use" && b["id"] == "call_future")
        .collect();
    assert_eq!(
        tool_uses_for_id.len(),
        1,
        "Other-typed tool_use must dedupe against tool_calls: {content:?}",
    );
}

/// Review follow-up to Bug D / monotonicity: the pre-scan must
/// NOT incorrectly dedupe an Other-typed block that lacks an
/// `id` extras field. Without the id, we cannot prove the
/// parts version is the same call as a tool_calls entry; emit
/// both rather than mis-dropping the tool_calls one.
#[test]
fn render_response_does_not_dedupe_other_tool_use_when_id_missing() {
    use routectl_core::{schema::Choice, ContentPart, Message, MessageContent, Role, Usage};
    let mut extras = serde_json::Map::new();
    // No `id` field on the Other block.
    extras.insert("name".into(), Value::String("anon".into()));
    let resp = ChatResponse {
        id: "msg_anon".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Other {
                    type_tag: "tool_use".into(),
                    cache_control: None,
                    extras,
                }]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_oc",
                    "type": "function",
                    "function": {"name": "ls", "arguments": "{}"}
                })]),
            },
            finish_reason: Some("tool_calls".into()),
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
    };
    let v = AnthropicIngress.render_response(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    // tool_calls entry still emits even though parts has an Other tool_use
    // (the parts block is also rendered as-is in the parts iteration).
    let tool_uses: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
    assert!(
        tool_uses.iter().any(|b| b["id"] == "call_oc"),
        "tool_calls entry must still emit when Other has no id: {content:?}",
    );
}
