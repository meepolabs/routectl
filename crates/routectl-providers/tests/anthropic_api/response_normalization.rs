//! Response normalization: thinking blocks, tool_use, stop_reason, and usage mapping.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn response_thinking_blocks_normalized() {
    let provider = make_provider("https://api.anthropic.com");
    let raw = json!({
        "id": "msg_01abc",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-5-20251101",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 120, "output_tokens": 80},
        "content": [
            {"type": "thinking", "thinking": "Let me think...", "signature": "sig_abc123"},
            {"type": "text", "text": "The answer is 42."}
        ]
    });

    let resp = provider.normalize_response(raw).unwrap();
    assert_eq!(resp.choices.len(), 1);
    let msg = &resp.choices[0].message;

    // Text content
    match &msg.content {
        MessageContent::Text(t) => assert_eq!(t, "The answer is 42."),
        other => panic!("expected Text content, got {other:?}"),
    }

    // Reasoning details
    assert_eq!(msg.reasoning_details.len(), 1);
    let detail = &msg.reasoning_details[0];
    assert!(matches!(detail.kind, ReasoningDetailKind::Text));
    assert_eq!(detail.format.as_deref(), Some("anthropic-claude-v1"));
    assert_eq!(detail.payload["text"], "Let me think...");
    assert_eq!(detail.payload["signature"], "sig_abc123");

    assert_eq!(resp.choices[0].finish_reason, Some("stop".into()));
}

#[test]
fn response_tool_use_maps_to_tool_calls() {
    let provider = make_provider("https://api.anthropic.com");
    let raw = json!({
        "id": "msg_02tool",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-5-20251101",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 50, "output_tokens": 30},
        "content": [
            {"type": "tool_use", "id": "toolu_01", "name": "get_weather",
             "input": {"location": "London"}}
        ]
    });

    let resp = provider.normalize_response(raw).unwrap();
    let msg = &resp.choices[0].message;

    let tool_calls = msg.tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["id"], "toolu_01");
    assert_eq!(tool_calls[0]["type"], "function");
    assert_eq!(tool_calls[0]["function"]["name"], "get_weather");

    // arguments should be JSON string
    let args: Value =
        serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["location"], "London");

    assert_eq!(resp.choices[0].finish_reason, Some("tool_calls".into()));
}

#[test]
fn stop_reason_mapping() {
    use routectl_providers::anthropic_api::response::map_stop_reason;
    assert_eq!(map_stop_reason(Some("end_turn")), Some("stop".into()));
    assert_eq!(map_stop_reason(Some("max_tokens")), Some("length".into()));
    assert_eq!(map_stop_reason(Some("stop_sequence")), Some("stop".into()));
    assert_eq!(map_stop_reason(Some("tool_use")), Some("tool_calls".into()));
    assert_eq!(map_stop_reason(None), None);
}

#[test]
fn usage_mapped_correctly() {
    let provider = make_provider("https://api.anthropic.com");
    let raw = json!({
        "id": "msg_03",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 20
        },
        "content": [{"type": "text", "text": "Hi"}]
    });

    let resp = provider.normalize_response(raw).unwrap();
    let usage = resp.usage.unwrap();
    // prompt_tokens is the SUM of input + cache_creation + cache_read
    // (OpenAI-spec correct: total prompt size, with the per-bucket
    // breakdown still on the cache_* fields). Anthropic's
    // input_tokens=100 (new only) + cache_read=20 = 120.
    assert_eq!(usage.prompt_tokens, 120);
    assert_eq!(usage.completion_tokens, 50);
    assert_eq!(usage.total_tokens, 170);
}

#[test]
fn complete_usage_carries_server_tool_use() {
    let provider = make_provider("https://api.anthropic.com");
    let raw = json!({
        "id": "msg_stu",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "server_tool_use": {"web_search_requests": 3}
        },
        "content": [{"type": "text", "text": "Hi"}]
    });
    let resp = provider.normalize_response(raw).unwrap();
    let usage = resp.usage.unwrap();
    assert_eq!(
        usage.server_tool_use,
        Some(json!({"web_search_requests": 3}))
    );
}

#[test]
fn redacted_thinking_maps_to_encrypted_kind() {
    let provider = make_provider("https://api.anthropic.com");
    let raw = json!({
        "id": "msg_04",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "content": [
            {"type": "redacted_thinking", "data": "base64encryptedblob"},
            {"type": "text", "text": "Here is my answer."}
        ]
    });

    let resp = provider.normalize_response(raw).unwrap();
    let detail = &resp.choices[0].message.reasoning_details[0];
    assert!(matches!(detail.kind, ReasoningDetailKind::Encrypted));
    assert_eq!(detail.payload["data"], "base64encryptedblob");
}
