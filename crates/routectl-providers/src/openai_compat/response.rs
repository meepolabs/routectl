//! Per-dialect response normalization.
//!
//! Converts a raw upstream JSON body into a routectl `ChatResponse`.
//! Each dialect has a single responsibility:
//!   - OpenAi/OpenRouter/Passthrough: direct deserialization; lift any
//!     stray `reasoning_content` for safety.
//!   - DeepSeek/Vllm: lift `message.reasoning_content` ->
//!     `reasoning_details[type=reasoning.text, format=<dialect-tag>]`.
//!   - RawThinkTag: regex-strip `<think>...</think>` blocks from content,
//!     push them as `reasoning_details`.

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;
use uuid::Uuid;

use routectl_core::{
    ChatResponse, Error, Message, ReasoningDetail, ReasoningDetailKind, Result,
};

use super::dialect::ReasoningDialect;

pub fn normalize(id: &str, raw: Value, dialect: ReasoningDialect) -> Result<ChatResponse> {
    let preprocessed = coalesce_reasoning_content_in_response(raw);
    let mut resp: ChatResponse = serde_json::from_value(preprocessed)
        .map_err(|e| Error::normalize_response(id, e.to_string()))?;

    for choice in resp.choices.iter_mut() {
        apply_dialect_to_message(id, &mut choice.message, dialect)?;
    }

    Ok(resp)
}

/// Coalesce `message.reasoning_content` -> `message.reasoning` across all
/// choices, so downstream serde deserialization sees a single canonical key.
///
/// Some providers emit BOTH (NIM's llama-3.3 returns both fields, often
/// null) which causes serde to refuse the alias mapping with "duplicate
/// field `reasoning`". By rewriting the JSON to a single field name
/// before deserializing, we avoid the collision and pick the non-null
/// value when only one is set.
pub(crate) fn coalesce_reasoning_content_in_response(mut raw: Value) -> Value {
    if let Some(choices) = raw.get_mut("choices").and_then(|v| v.as_array_mut()) {
        for choice in choices.iter_mut() {
            if let Some(msg) = choice.get_mut("message").and_then(|v| v.as_object_mut()) {
                merge_reasoning_keys(msg);
            }
        }
    }
    raw
}

/// Merge `reasoning_content` into `reasoning`, preferring whichever is a
/// non-null string. Always strips `reasoning_content` after.
pub(crate) fn merge_reasoning_keys(obj: &mut serde_json::Map<String, Value>) {
    let rc = obj.remove("reasoning_content");
    let r_is_null = obj.get("reasoning").map_or(true, |v| v.is_null());
    if r_is_null {
        // Either no `reasoning` key, or it's null. Promote rc if non-null.
        match rc {
            Some(v) if !v.is_null() => {
                obj.insert("reasoning".into(), v);
            }
            _ => {
                obj.remove("reasoning");
            }
        }
    }
}

fn apply_dialect_to_message(
    id: &str,
    msg: &mut Message,
    dialect: ReasoningDialect,
) -> Result<()> {
    match dialect {
        ReasoningDialect::DeepSeek | ReasoningDialect::Vllm => {
            lift_reasoning_content_field(msg, dialect.format_tag());
        }
        ReasoningDialect::RawThinkTag => {
            lift_think_tags(id, msg)?;
        }
        ReasoningDialect::OpenAi
        | ReasoningDialect::OpenRouter
        | ReasoningDialect::Passthrough => {}
    }
    Ok(())
}

/// Pull `reasoning_content` (a plain string field that DeepSeek and vLLM
/// emit alongside `content`) into a typed `ReasoningDetail`.
fn lift_reasoning_content_field(msg: &mut Message, format_tag: &str) {
    // The field may survive as `msg.reasoning` (our schema maps it there via
    // serde flatten or manual mapping). In practice the raw JSON has
    // `reasoning_content` which serde_json deserializes into `reasoning`.
    let text = match msg.reasoning.take() {
        Some(t) if !t.is_empty() => t,
        _ => return,
    };

    let detail = build_reasoning_detail(&text, format_tag, msg.reasoning_details.len() as u32);
    msg.reasoning_details.push(detail);
}

/// Regex-extract `<think>...</think>` from the content string.
/// The entire block (potentially multiple blocks) is lifted into
/// `reasoning_details`. Remaining content replaces the original.
fn lift_think_tags(id: &str, msg: &mut Message) -> Result<()> {
    let content = match &msg.content {
        routectl_core::MessageContent::Text(t) => t.clone(),
        // Parts/Null: not expected from reasoning-model endpoints;
        // leave untouched.
        routectl_core::MessageContent::Parts(_) | routectl_core::MessageContent::Null => {
            return Ok(())
        }
    };

    let re = think_tag_regex();
    let mut reasoning_text = String::new();
    let stripped = re.replace_all(&content, |caps: &regex::Captures| {
        reasoning_text.push_str(caps.get(1).map_or("", |m| m.as_str()));
        ""
    });

    if reasoning_text.is_empty() {
        return Ok(());
    }

    let detail = build_reasoning_detail(
        &reasoning_text,
        ReasoningDialect::RawThinkTag.format_tag(),
        msg.reasoning_details.len() as u32,
    );
    msg.reasoning_details.push(detail);
    msg.content = routectl_core::MessageContent::Text(stripped.trim_start().to_string());
    let _ = id; // id reserved for future error paths
    Ok(())
}

fn build_reasoning_detail(text: &str, format_tag: &str, index: u32) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some(Uuid::new_v4().to_string()),
        format: Some(format_tag.into()),
        index: Some(index),
        payload: serde_json::json!({"text": text}),
    }
}

fn think_tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // DOTALL via `(?s)` so `.` matches newlines inside think blocks.
        Regex::new(r"(?s)<think>(.*?)</think>").expect("static regex is valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{MessageContent, ReasoningDetailKind};
    use serde_json::json;

    fn fake_response(content: &str) -> Value {
        json!({
            "id": "chatcmpl-test",
            "model": "test-model",
            "created": 1_700_000_000_i64,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            }
        })
    }

    fn fake_response_with_reasoning(content: &str, reasoning: &str) -> Value {
        json!({
            "id": "chatcmpl-test",
            "model": "test-model",
            "created": 1_700_000_000_i64,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                    "reasoning": reasoning
                },
                "finish_reason": "stop"
            }]
        })
    }

    #[test]
    fn openai_passthrough() {
        let raw = fake_response("hello");
        let resp = normalize("test", raw, ReasoningDialect::OpenAi).unwrap();
        assert_eq!(resp.choices[0].message.reasoning_details.len(), 0);
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn deepseek_lifts_reasoning_content() {
        let raw = fake_response_with_reasoning("The answer is 42", "Let me think step by step");
        let resp = normalize("test", raw, ReasoningDialect::DeepSeek).unwrap();
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("deepseek-v1"));
        assert!(matches!(details[0].kind, ReasoningDetailKind::Text));
        assert_eq!(details[0].payload["text"], "Let me think step by step");
    }

    #[test]
    fn vllm_lifts_reasoning_content() {
        let raw = fake_response_with_reasoning("result", "vllm reasoning trace");
        let resp = normalize("test", raw, ReasoningDialect::Vllm).unwrap();
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("vllm-reasoning-v1"));
        assert!(matches!(details[0].kind, ReasoningDetailKind::Text));
    }

    #[test]
    fn raw_think_tag_strips_and_lifts() {
        let raw = fake_response("<think>inner thought</think>The answer is 42");
        let resp = normalize("test", raw, ReasoningDialect::RawThinkTag).unwrap();
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("raw-think-tag-v1"));
        assert_eq!(details[0].payload["text"], "inner thought");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "The answer is 42"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn raw_think_tag_multiline() {
        let raw = fake_response("<think>\nline1\nline2\n</think>After thought");
        let resp = normalize("test", raw, ReasoningDialect::RawThinkTag).unwrap();
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details[0].payload["text"], "\nline1\nline2\n");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t.trim(), "After thought"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn passthrough_no_mutation() {
        let raw = fake_response("keep me");
        let resp = normalize("test", raw, ReasoningDialect::Passthrough).unwrap();
        assert_eq!(resp.choices[0].message.reasoning_details.len(), 0);
    }

    #[test]
    fn openrouter_no_mutation() {
        let raw = fake_response("openrouter content");
        let resp = normalize("test", raw, ReasoningDialect::OpenRouter).unwrap();
        assert_eq!(resp.choices[0].message.reasoning_details.len(), 0);
    }
}
