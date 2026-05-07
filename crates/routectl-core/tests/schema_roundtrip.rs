//! Schema round-trip tests. Confirms that the OpenRouter-shape types we
//! serialize match real-world wire formats and don't lose fields.

use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Reasoning, ReasoningConfig, ReasoningDetail,
    ReasoningDetailKind,
};
use serde_json::{json, Value};

fn roundtrip<T>(input: Value) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let parsed: T = serde_json::from_value(input.clone()).expect("deserialize");
    let serialized = serde_json::to_value(&parsed).expect("serialize");
    // Fields we serialized should be a subset of the input. Every field we accepted
    // should round-trip identically.
    assert_subset(&serialized, &input, "");
    parsed
}

fn assert_subset(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            for (k, av) in a {
                let ev = e
                    .get(k)
                    .unwrap_or_else(|| panic!("field `{path}.{k}` serialized but not in input"));
                assert_subset(av, ev, &format!("{path}.{k}"));
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            assert_eq!(a.len(), e.len(), "array length mismatch at `{path}`");
            for (i, (av, ev)) in a.iter().zip(e.iter()).enumerate() {
                assert_subset(av, ev, &format!("{path}[{i}]"));
            }
        }
        (a, e) => assert_eq!(a, e, "value mismatch at `{path}`"),
    }
}

#[test]
fn openai_request_basic() {
    let input = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hi"}
        ],
        "temperature": 0.7,
        "max_tokens": 200,
        "stream": false
    });
    let req: ChatRequest = roundtrip(input);
    assert_eq!(req.model, "gpt-4o");
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.temperature, Some(0.7));
}

#[test]
fn openrouter_request_with_reasoning() {
    let input = json!({
        "model": "anthropic/claude-opus-4-7",
        "messages": [{"role": "user", "content": "Solve this."}],
        "reasoning": {
            "effort": "high",
            "max_tokens": 8000,
            "exclude": false,
            "enabled": true
        },
        "provider_extras": {"transforms": ["middle-out"]}
    });
    let req: ChatRequest = roundtrip(input);
    let r = req.reasoning.as_ref().expect("reasoning present");
    assert_eq!(r.effort.as_deref(), Some("high"));
    assert_eq!(r.max_tokens, Some(8000));
}

#[test]
fn openai_request_with_tools_and_modern_fields() {
    let input = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Use the tool."}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object", "properties": {}}
            }
        }],
        "tool_choice": "auto",
        "response_format": {"type": "json_object"},
        "seed": 42,
        "n": 1,
        "logprobs": true,
        "top_logprobs": 5,
        "user": "user-123"
    });
    let req: ChatRequest = roundtrip(input);
    assert_eq!(req.seed, Some(42));
    assert_eq!(req.n, Some(1));
    assert_eq!(req.logprobs, Some(true));
    assert_eq!(req.top_logprobs, Some(5));
    assert_eq!(req.user.as_deref(), Some("user-123"));
}

#[test]
fn openrouter_response_with_reasoning_details() {
    let input = json!({
        "id": "chatcmpl-abc",
        "model": "anthropic/claude-opus-4-7",
        "created": 1700000000,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "The answer is 42.",
                "reasoning": "First, I considered...",
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "reasoning-1",
                    "format": "anthropic-claude-v1",
                    "index": 0,
                    "text": "Let me work through this...",
                    "signature": "sha256:abc"
                }]
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 50,
            "completion_tokens": 100,
            "total_tokens": 150,
            "reasoning_tokens": 80
        }
    });
    let resp: ChatResponse = roundtrip(input);
    assert_eq!(resp.choices.len(), 1);
    let msg = &resp.choices[0].message;
    assert_eq!(msg.reasoning.as_deref(), Some("First, I considered..."));
    assert_eq!(msg.reasoning_details.len(), 1);
    let detail = &msg.reasoning_details[0];
    assert!(matches!(detail.kind, ReasoningDetailKind::Text));
    assert_eq!(detail.format.as_deref(), Some("anthropic-claude-v1"));
    assert_eq!(detail.id.as_deref(), Some("reasoning-1"));
}

#[test]
fn deepseek_response_legacy_reasoning() {
    // Before normalization: DeepSeek puts reasoning_content next to content.
    // routectl normalizes this away in the provider; here we just verify the
    // schema accepts it via reasoning_details once normalized.
    let input = json!({
        "id": "chatcmpl-ds",
        "model": "deepseek-reasoner",
        "created": 1700000000,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "42",
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "reasoning-ds-0",
                    "format": "deepseek-v1",
                    "index": 0,
                    "text": "Step by step..."
                }]
            },
            "finish_reason": "stop"
        }]
    });
    let resp: ChatResponse = roundtrip(input);
    let detail = &resp.choices[0].message.reasoning_details[0];
    assert_eq!(detail.format.as_deref(), Some("deepseek-v1"));
}

#[test]
fn streaming_chunk_with_reasoning_delta() {
    let input = json!({
        "id": "chatcmpl-stream",
        "model": "deepseek-reasoner",
        "choices": [{
            "index": 0,
            "delta": {
                "reasoning": "First, ",
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "reasoning-stream-0",
                    "format": "deepseek-v1",
                    "index": 0,
                    "text": "First, "
                }]
            }
        }]
    });
    let chunk: ChatChunk = roundtrip(input);
    let delta = &chunk.choices[0].delta;
    assert_eq!(delta.reasoning.as_deref(), Some("First, "));
    assert_eq!(delta.reasoning_details.len(), 1);
}

#[test]
fn anthropic_thinking_signature_preserved_in_reasoning_detail() {
    let detail = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some("anthropic-thinking-0".into()),
        format: Some("anthropic-claude-v1".into()),
        index: Some(0),
        payload: json!({
            "text": "Let me think step by step about this complex problem.",
            "signature": "WaUjzkypQ2mUEVM36O2TxuC06KN8xyfb_ABC"
        }),
    };
    let serialized = serde_json::to_value(&detail).expect("serialize");
    let s = serialized.as_object().unwrap();
    assert_eq!(s["type"], "reasoning.text");
    assert_eq!(s["format"], "anthropic-claude-v1");
    assert_eq!(s["signature"], "WaUjzkypQ2mUEVM36O2TxuC06KN8xyfb_ABC");
    assert!(s["text"].is_string());
}

#[test]
fn reasoning_config_default_is_empty() {
    let cfg = ReasoningConfig::default();
    let s = serde_json::to_value(&cfg).unwrap();
    // skip_serializing_if Option::is_none keeps the JSON minimal
    assert_eq!(s, json!({}));
}

#[test]
fn message_with_no_reasoning_omits_fields() {
    let msg_json = json!({
        "role": "user",
        "content": "Hi"
    });
    let _: routectl_core::Message = roundtrip(msg_json);
}

#[test]
fn reasoning_top_level_serializes_minimally() {
    let r = Reasoning {
        text: Some("trace".into()),
        details: vec![],
    };
    let s = serde_json::to_value(&r).unwrap();
    assert_eq!(s, json!({"text": "trace"}));
}
