//! Response normalization: Anthropic wire format -> routectl shape.
//!
//! Cache-stats handling: the upstream Anthropic / Bedrock-Invoke
//! `usage` object carries `cache_creation_input_tokens`,
//! `cache_read_input_tokens`, and a per-TTL `cache_creation` breakdown.
//! These now flow into the canonical `Usage` so OpenAI-SSE clients see
//! the same totals at end-of-stream. Forward-compat: unknown content
//! block types (server_tool_use, web_search_tool_result,
//! code_execution_tool_result, ...) are accepted on the wire as
//! `ContentBlock::Other` and dropped from the flat-text output with
//! a `tracing::warn!` so the client sees what was lost.

use chrono::Utc;
use serde_json::{json, Value};
use tracing::warn;
use uuid::Uuid;

use routectl_core::schema::CacheCreation;
use routectl_core::{
    ChatResponse, Choice, Error, Message, MessageContent, ReasoningDetail, ReasoningDetailKind,
    Result, Role, Usage,
};

use super::types::{AnthropicResponse, AnthropicUsage, ContentBlock};

const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Map Anthropic stop_reason to OpenAI finish_reason.
pub fn map_stop_reason(stop_reason: Option<&str>) -> Option<String> {
    let reason = match stop_reason? {
        "end_turn" => "stop",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        other => other,
    };
    Some(reason.to_string())
}

/// Walk content blocks and produce (text, reasoning_details, tool_calls).
pub fn walk_content_blocks(
    id: &str,
    blocks: &[ContentBlock],
) -> Result<(String, Vec<ReasoningDetail>, Option<Vec<Value>>)> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_details: Vec<ReasoningDetail> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut detail_index: u32 = 0;

    for block in blocks {
        match block {
            ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                reasoning_details.push(ReasoningDetail {
                    kind: ReasoningDetailKind::Text,
                    id: Some(Uuid::new_v4().to_string()),
                    format: Some(ANTHROPIC_FORMAT.to_string()),
                    index: Some(detail_index),
                    payload: json!({"text": thinking, "signature": signature}),
                });
                detail_index += 1;
            }
            ContentBlock::RedactedThinking { data, .. } => {
                reasoning_details.push(ReasoningDetail {
                    kind: ReasoningDetailKind::Encrypted,
                    id: Some(Uuid::new_v4().to_string()),
                    format: Some(ANTHROPIC_FORMAT.to_string()),
                    index: Some(detail_index),
                    payload: json!({"data": data}),
                });
                detail_index += 1;
            }
            ContentBlock::Text { text, .. } => {
                text_parts.push(text.clone());
            }
            ContentBlock::ToolUse {
                id: tool_id,
                name,
                input,
                ..
            } => {
                // Convert to OpenAI tool_call shape.
                let arguments = serde_json::to_string(input)
                    .map_err(|e| Error::normalize_response(id, e.to_string()))?;
                tool_calls.push(json!({
                    "id": tool_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }));
            }
            ContentBlock::ToolResult { .. } => {
                // Not expected in a response; skip.
            }
            ContentBlock::Image { .. } => {
                warn!(
                    provider = id,
                    "image block in response dropped from flat-text output"
                );
            }
            ContentBlock::Document { .. } => {
                warn!(
                    provider = id,
                    "document block in response dropped from flat-text output"
                );
            }
            ContentBlock::Other { type_tag, .. } => {
                warn!(
                    provider = id,
                    block_type = %type_tag,
                    "unknown content block type in response dropped from flat-text output",
                );
            }
        }
    }

    let text = text_parts.join("");
    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    Ok((text, reasoning_details, tool_calls_opt))
}

/// Translate Anthropic `usage` into the canonical `Usage`, including
/// the cache-stats extension (cache_creation_input_tokens,
/// cache_read_input_tokens, per-TTL breakdown).
fn translate_usage(u: &AnthropicUsage) -> Usage {
    Usage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.input_tokens + u.output_tokens,
        reasoning_tokens: u.reasoning_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_creation: u.cache_creation.as_ref().map(|c| CacheCreation {
            ephemeral_5m_input_tokens: c.ephemeral_5m_input_tokens,
            ephemeral_1h_input_tokens: c.ephemeral_1h_input_tokens,
        }),
    }
}

pub fn normalize(id: &str, raw: Value) -> Result<ChatResponse> {
    let resp: AnthropicResponse =
        serde_json::from_value(raw).map_err(|e| Error::normalize_response(id, e.to_string()))?;

    let (text, reasoning_details, tool_calls) = walk_content_blocks(id, &resp.content)?;
    let usage = resp.usage.as_ref().map(translate_usage);
    let finish_reason = map_stop_reason(resp.stop_reason.as_deref());

    let message = Message {
        role: Role::Assistant,
        content: MessageContent::Text(text),
        reasoning: None,
        reasoning_details,
        name: None,
        tool_call_id: None,
        tool_calls,
    };

    Ok(ChatResponse {
        id: resp.id,
        model: resp.model,
        created: Utc::now().timestamp(),
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason,
        }],
        usage,
        routectl_provider: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_usage_fields_propagate_to_canonical() {
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-7",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 10,
                "cache_creation_input_tokens": 4096,
                "cache_read_input_tokens": 8192,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 2048,
                    "ephemeral_1h_input_tokens": 2048
                }
            }
        });
        let resp = normalize("test", raw).unwrap();
        let u = resp.usage.unwrap();
        assert_eq!(u.cache_creation_input_tokens, Some(4096));
        assert_eq!(u.cache_read_input_tokens, Some(8192));
        let cc = u.cache_creation.unwrap();
        assert_eq!(cc.ephemeral_5m_input_tokens, Some(2048));
        assert_eq!(cc.ephemeral_1h_input_tokens, Some(2048));
    }

    #[test]
    fn unknown_block_in_response_does_not_panic() {
        // Forward-compat: a future Anthropic response with a
        // server_tool_use block must parse without error and surface
        // the rest of the content cleanly.
        let raw = json!({
            "id": "msg_02",
            "model": "claude-opus-4-7",
            "content": [
                {"type": "server_tool_use", "id": "srvtu_01", "name": "web_search", "input": {}},
                {"type": "text", "text": "after"}
            ],
            "stop_reason": "end_turn"
        });
        let resp = normalize("test", raw).unwrap();
        let msg = &resp.choices[0].message;
        if let MessageContent::Text(t) = &msg.content {
            assert_eq!(t, "after");
        } else {
            panic!("expected Text content");
        }
    }
}
