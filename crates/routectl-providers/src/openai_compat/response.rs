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

use serde_json::Value;

use routectl_core::{ChatResponse, Error, Message, MessageContent, Result};

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

/// OpenAI-compat upstreams (DeepSeek, vLLM, OpenRouter, NIM, llama.cpp,
/// ...) don't expose the matched stop sequence on the response wire --
/// the spec carries only `finish_reason`, and most hosts strip the
/// matched sequence from the response content. The Anthropic ingress
/// needs `Choice.matched_stop_sequence` populated to render
/// `stop_reason:"stop_sequence"` instead of the lossy `end_turn`,
/// which breaks claude-code structured-output flows whose
/// `is_error: true` envelope is keyed on the wire `stop_reason`.
///
/// Heuristic: when the request shipped at least one stop_sequence AND
/// a Choice finished with `finish_reason == "stop"` AND the egress
/// hasn't already lifted a native value, look for a suffix match
/// against the response content. If exactly one stop_sequence was
/// configured and no suffix match is found (typical: the host
/// stripped the matched marker from the content), fall back to that
/// sole sequence -- this is the common structured-output pattern
/// (single fence, finish_reason "stop") and the alternative is a
/// hard `end_turn` mis-render. Multiple ambiguous stops without a
/// suffix hit stay `None` so we don't over-claim.
/// Public for integration-test access; not a stability surface --
/// `#[doc(hidden)]` keeps it out of the rendered docs.
#[doc(hidden)]
pub fn apply_stop_sequence_heuristic(
    chat_resp: &mut ChatResponse,
    request_stop: Option<&[String]>,
) {
    let stops = match request_stop {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    for choice in chat_resp.choices.iter_mut() {
        if choice.matched_stop_sequence.is_some() {
            continue;
        }
        if choice.finish_reason.as_deref() != Some("stop") {
            continue;
        }
        let text = message_text_for_match(&choice.message);
        choice.matched_stop_sequence = detect_matched_stop_sequence(text.as_deref(), stops);
    }
}

/// Pick a single suffix-matching stop sequence from `stops` if the
/// content trails one (host left it in the body). Otherwise, when
/// exactly one stop_sequence was configured AND the response actually
/// carried content, fall back to that sequence as the best-guess for
/// the structured-output single-fence flow. Returns `None` for
/// multiple ambiguous stops without a suffix hit, or when there is
/// no content to evidence either way.
///
/// Public for integration-test access; not a stability surface --
/// `#[doc(hidden)]` keeps it out of the rendered docs.
#[doc(hidden)]
pub fn detect_matched_stop_sequence(text: Option<&str>, stops: &[String]) -> Option<String> {
    // Bail when there is no content to evidence the heuristic. Common
    // cases: tool-only responses (defense in depth -- `finish_reason`
    // on tool turns is "tool_calls", not "stop", and the caller
    // already gates on "stop", but be explicit), reasoning-only
    // responses, and `MessageContent::Null` from non-Anthropic hosts.
    // Without this gate the single-stop fallback below would over-claim
    // on responses that never actually emitted a stop_sequence.
    let t = text?;
    let trimmed = t.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    // Try the longest sequences first so an inner "</answer>" wins over a
    // shorter "<" when both are configured. Stable on equal length.
    let mut ordered: Vec<&String> = stops.iter().filter(|s| !s.is_empty()).collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in ordered.iter() {
        // ASCII-safe on UTF-8 because `str::ends_with(&str)` aligns
        // on char boundaries. Note we trim trailing whitespace from
        // content but NOT from `stops` -- mirror trimming the stop
        // would silently rewrite an operator's explicit choice and is
        // not worth the bytes saved.
        if trimmed.ends_with(s.as_str()) {
            return Some((**s).clone());
        }
    }
    // Single-stop fallback: structured-output flows configure one fence
    // and rely on stop_reason discrimination. Without this fallback the
    // common case stays broken because most hosts strip the matched
    // marker from the content. Only safe when content was present
    // (the `trimmed.is_empty()` guard above handles tool-only / null /
    // whitespace-only responses).
    //
    // Residual risk: a non-structured-output flow that configures a
    // single stop_sequence and naturally ends mid-thought without
    // emitting the sequence gets `stop_sequence` instead of
    // `end_turn`. Operators tracking this can disable the fallback by
    // adopting an Anthropic-upstream provider, which surfaces the
    // matched sequence natively.
    if ordered.len() == 1 {
        return Some(ordered[0].clone());
    }
    None
}

fn message_text_for_match(msg: &Message) -> Option<String> {
    match &msg.content {
        MessageContent::Text(t) => Some(t.clone()),
        MessageContent::Parts(parts) => {
            let mut buf = String::new();
            for p in parts {
                if let routectl_core::ContentPart::Known(routectl_core::KnownContentPart::Text {
                    text,
                    ..
                }) = p
                {
                    buf.push_str(text);
                }
            }
            if buf.is_empty() {
                None
            } else {
                Some(buf)
            }
        }
        MessageContent::Null => None,
    }
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

fn apply_dialect_to_message(id: &str, msg: &mut Message, dialect: ReasoningDialect) -> Result<()> {
    dialect.as_dyn().apply_response(id, msg)
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
