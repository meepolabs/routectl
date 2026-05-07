//! Response normalization: Anthropic wire format -> routectl shape.

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use routectl_core::{
    ChatResponse, Choice, Error, Message, MessageContent, ReasoningDetail, ReasoningDetailKind,
    Result, Role, Usage,
};

use super::types::{AnthropicResponse, ContentBlock};

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
            ContentBlock::Thinking { thinking, signature } => {
                reasoning_details.push(ReasoningDetail {
                    kind: ReasoningDetailKind::Text,
                    id: Some(Uuid::new_v4().to_string()),
                    format: Some(ANTHROPIC_FORMAT.to_string()),
                    index: Some(detail_index),
                    payload: json!({"text": thinking, "signature": signature}),
                });
                detail_index += 1;
            }
            ContentBlock::RedactedThinking { data } => {
                reasoning_details.push(ReasoningDetail {
                    kind: ReasoningDetailKind::Encrypted,
                    id: Some(Uuid::new_v4().to_string()),
                    format: Some(ANTHROPIC_FORMAT.to_string()),
                    index: Some(detail_index),
                    payload: json!({"data": data}),
                });
                detail_index += 1;
            }
            ContentBlock::Text { text } => {
                text_parts.push(text.clone());
            }
            ContentBlock::ToolUse { id: tool_id, name, input } => {
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
        }
    }

    let text = text_parts.join("");
    let tool_calls_opt = if tool_calls.is_empty() { None } else { Some(tool_calls) };

    Ok((text, reasoning_details, tool_calls_opt))
}

pub fn normalize(id: &str, raw: Value) -> Result<ChatResponse> {
    let resp: AnthropicResponse = serde_json::from_value(raw)
        .map_err(|e| Error::normalize_response(id, e.to_string()))?;

    let (text, reasoning_details, tool_calls) = walk_content_blocks(id, &resp.content)?;

    let usage = resp.usage.as_ref().map(|u| Usage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.input_tokens + u.output_tokens,
        reasoning_tokens: u.reasoning_tokens,
    });

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
        choices: vec![Choice { index: 0, message, finish_reason }],
        usage,
        routectl_provider: None,
    })
}
