//! Request normalization: routectl shape -> Anthropic wire format.

use serde_json::{json, Value};

use routectl_core::{ChatRequest, Error, Message, MessageContent, ReasoningDetailKind, Result, Role};

use super::types::{
    AnthropicContent, AnthropicMessage, AnthropicRequest, AnthropicRole, AnthropicTool,
    ContentBlock, ThinkingConfig,
};
use crate::model_profile::profile_for;

const DEFAULT_MAX_TOKENS: u32 = 4096;
const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Proportional budget_tokens as fraction of max_tokens per effort level.
fn effort_ratio(effort: &str) -> f64 {
    match effort {
        "xhigh" => 0.95,
        "high" => 0.80,
        "medium" => 0.50,
        "low" => 0.20,
        "minimal" => 0.10,
        _ => 0.50,
    }
}

fn build_thinking(req: &ChatRequest) -> Option<ThinkingConfig> {
    let r = req.reasoning.as_ref()?;

    // Explicit disable
    if r.enabled == Some(false) {
        return Some(ThinkingConfig::Disabled);
    }

    if let Some(budget) = r.max_tokens {
        return Some(ThinkingConfig::Enabled { budget_tokens: budget });
    }

    if let Some(effort) = r.effort.as_deref() {
        if effort == "none" {
            return Some(ThinkingConfig::Disabled);
        }
        let max = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let budget = ((max as f64) * effort_ratio(effort)).max(1.0) as u32;
        // Future extension point: when the Anthropic API exposes a
        // distinct `adaptive` thinking type in JSON, branch on
        // `profile_for(&req.model).supports_adaptive_thinking` here.
        // For now both code paths produced the same budget_tokens shape.
        let _adaptive = profile_for(&req.model).supports_adaptive_thinking;
        return Some(ThinkingConfig::Enabled { budget_tokens: budget });
    }

    if r.enabled == Some(true) {
        let max = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let budget = (max / 2).max(1);
        return Some(ThinkingConfig::Enabled { budget_tokens: budget });
    }

    None
}

/// Translate an OpenAI-shape tool object into an Anthropic tool.
/// OpenAI: `{type: "function", function: {name, description, parameters}}`
/// Anthropic: `{name, description?, input_schema}`
fn translate_tool(id: &str, tool: &Value) -> Result<AnthropicTool> {
    let func = tool
        .get("function")
        .ok_or_else(|| Error::normalize_request(id, "tool missing 'function' key"))?;

    let name = func
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::normalize_request(id, "tool.function missing 'name'"))?
        .to_string();

    let description = func
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let input_schema = func
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

    Ok(AnthropicTool { name, description, input_schema })
}

/// Reconstruct an Anthropic content array for an assistant message that
/// carries reasoning_details (tool-use continuity). thinking blocks with
/// signatures must be passed back verbatim.
fn build_assistant_content(id: &str, msg: &Message) -> Result<AnthropicContent> {
    if msg.reasoning_details.is_empty() {
        // No reasoning -- plain text or empty string.
        let text = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(_) | MessageContent::Null => String::new(),
        };
        return Ok(AnthropicContent::Text(text));
    }

    let mut blocks: Vec<ContentBlock> = Vec::new();

    // Emit reasoning blocks first (in index order).
    let mut details = msg.reasoning_details.clone();
    details.sort_by_key(|d| d.index.unwrap_or(0));

    for detail in &details {
        match detail.kind {
            ReasoningDetailKind::Text => {
                // Verify format tag before trusting the payload.
                if detail.format.as_deref() != Some(ANTHROPIC_FORMAT) {
                    continue;
                }
                let thinking = detail
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let signature = detail
                    .payload
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        Error::normalize_request(
                            id,
                            "thinking block missing signature in reasoning_details",
                        )
                    })?
                    .to_string();
                blocks.push(ContentBlock::Thinking { thinking, signature });
            }
            ReasoningDetailKind::Encrypted => {
                if detail.format.as_deref() != Some(ANTHROPIC_FORMAT) {
                    continue;
                }
                let data = detail
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                blocks.push(ContentBlock::RedactedThinking { data });
            }
            ReasoningDetailKind::Summary => {
                // Not an Anthropic block; skip.
            }
        }
    }

    // Append text block from message content.
    let text = match &msg.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Parts(_) | MessageContent::Null => String::new(),
    };
    if !text.is_empty() {
        blocks.push(ContentBlock::Text { text });
    }

    Ok(AnthropicContent::Blocks(blocks))
}

/// Build the tool_result content for a tool-role message.
fn build_tool_message(msg: &Message) -> AnthropicMessage {
    // Anthropic represents tool results as a user message with tool_result block.
    let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
    let content_val = match &msg.content {
        MessageContent::Text(t) => Value::String(t.clone()),
        MessageContent::Parts(parts) => Value::Array(parts.clone()),
        MessageContent::Null => Value::Null,
    };
    AnthropicMessage {
        role: AnthropicRole::User,
        content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id,
            content: content_val,
        }]),
    }
}

pub fn normalize(id: &str, req: &ChatRequest) -> Result<Value> {
    let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let thinking = build_thinking(req);

    // Lift system messages out of the messages array.
    let mut system: Option<String> = None;
    let mut anthropic_messages: Vec<AnthropicMessage> = Vec::new();

    for msg in &req.messages {
        match msg.role {
            Role::System => {
                let text = match &msg.content {
                    MessageContent::Text(t) => t.clone(),
                    MessageContent::Parts(_) | MessageContent::Null => String::new(),
                };
                // Multiple system messages are concatenated.
                match system.as_mut() {
                    Some(s) => {
                        s.push('\n');
                        s.push_str(&text);
                    }
                    None => system = Some(text),
                }
            }
            Role::User => {
                let content = match &msg.content {
                    MessageContent::Text(t) => AnthropicContent::Text(t.clone()),
                    MessageContent::Null => AnthropicContent::Text(String::new()),
                    MessageContent::Parts(parts) => {
                        // Pass parts array directly; Anthropic accepts vision blocks.
                        AnthropicContent::Blocks(
                            parts
                                .iter()
                                .filter_map(|p| {
                                    p.get("text")
                                        .and_then(|v| v.as_str())
                                        .map(|t| ContentBlock::Text { text: t.to_string() })
                                })
                                .collect(),
                        )
                    }
                };
                anthropic_messages.push(AnthropicMessage { role: AnthropicRole::User, content });
            }
            Role::Assistant => {
                let content = build_assistant_content(id, msg)?;
                anthropic_messages.push(AnthropicMessage {
                    role: AnthropicRole::Assistant,
                    content,
                });
            }
            Role::Tool => {
                anthropic_messages.push(build_tool_message(msg));
            }
        }
    }

    // Translate tools.
    let tools = req
        .tools
        .as_ref()
        .map(|ts| ts.iter().map(|t| translate_tool(id, t)).collect::<Result<Vec<_>>>())
        .transpose()?;

    // When thinking is enabled, temperature must be 1.0 (Anthropic requirement).
    let temperature = match &thinking {
        Some(ThinkingConfig::Enabled { .. }) => Some(1.0f64),
        _ => req.temperature,
    };

    let ar = AnthropicRequest {
        model: req.model.clone(),
        messages: anthropic_messages,
        max_tokens,
        system,
        thinking,
        temperature,
        top_p: req.top_p,
        stop_sequences: req.stop.clone(),
        stream: None, // caller sets this
        tools,
        tool_choice: req.tool_choice.clone(),
    };

    let mut body = serde_json::to_value(&ar)
        .map_err(|e| Error::normalize_request(id, e.to_string()))?;

    // Merge provider_extras last (caller wins).
    if let Some(extras) = req.provider_extras.as_ref() {
        if let (Some(obj), Some(extra_obj)) =
            (body.as_object_mut(), extras.as_object())
        {
            for (k, v) in extra_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
    }

    Ok(body)
}
