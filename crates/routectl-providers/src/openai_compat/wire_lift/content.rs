//! Lift Anthropic-shape content blocks inside `obj["messages"][].content`
//! to the OpenAI-compat wire shape.
//!
//! Walks the wire-form messages array (the body produced after
//! `serde_json::to_value(req)` then `dialect.apply_request`) and
//! rewrites:
//!
//!   {"type":"image", "source":{"type":"base64", "media_type", "data"}}
//!     -> {"type":"image_url", "image_url":{"url":"data:<media_type>;base64,<data>"}}
//!
//!   {"type":"image", "source":{"type":"url", "url":"..."}}
//!     -> {"type":"image_url", "image_url":{"url":"..."}}
//!
//!   {"type":"document", ...}
//!     -> warn + drop (no OpenAI chat-completions equivalent;
//!        strict_translation rejects with 400)
//!
//! Other block types (text, tool_use, tool_result, image_url,
//! thinking, redacted_thinking, forward-compat Other) pass through
//! verbatim. tool_use and tool_result are fixed up by the dedicated
//! lifts that run AFTER content (see wire_lift/mod.rs).
//!
//! When `content` is a string (legacy shape), no-op.

use serde_json::{Map, Value};
use tracing::warn;

use routectl_core::{ChatRequest, Error, Result};

pub fn lift(
    id: &str,
    obj: &mut Map<String, Value>,
    _req: &ChatRequest,
    strict: bool,
) -> Result<()> {
    let messages = match obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return Ok(()),
    };
    for (msg_idx, msg) in messages.iter_mut().enumerate() {
        let Some(msg_obj) = msg.as_object_mut() else {
            continue;
        };
        let Some(content_val) = msg_obj.get_mut("content") else {
            continue;
        };
        // String / null / non-array content -- nothing to do.
        let Some(parts) = content_val.as_array_mut() else {
            continue;
        };
        rewrite_parts(id, msg_idx, parts, strict)?;
    }
    Ok(())
}

fn rewrite_parts(id: &str, msg_idx: usize, parts: &mut Vec<Value>, strict: bool) -> Result<()> {
    // Build a new vec; drop document blocks; rewrite image blocks.
    let original = std::mem::take(parts);
    for part in original {
        match part_kind(&part) {
            PartKind::AnthropicImage => match rewrite_image_part(&part) {
                Some(rewritten) => parts.push(rewritten),
                None => {
                    if strict {
                        return Err(Error::Validation(format!(
                            "strict_translation: provider `{id}`: message {msg_idx} \
                                 image block has unsupported source shape \
                                 (expected base64 or url): {part}"
                        )));
                    }
                    warn!(
                        provider = id,
                        message_index = msg_idx,
                        "openai-compat egress: dropping image block with unsupported source shape"
                    );
                }
            },
            PartKind::Document => {
                if strict {
                    return Err(Error::Validation(format!(
                        "strict_translation: provider `{id}`: message {msg_idx} \
                         document content block cannot be represented on the \
                         OpenAI-compat wire"
                    )));
                }
                warn!(
                    provider = id,
                    message_index = msg_idx,
                    "openai-compat egress: dropping document content block (no OpenAI equivalent)"
                );
            }
            PartKind::Other => {
                parts.push(part);
            }
        }
    }
    Ok(())
}

enum PartKind {
    AnthropicImage,
    Document,
    Other,
}

fn part_kind(part: &Value) -> PartKind {
    let Some(obj) = part.as_object() else {
        return PartKind::Other;
    };
    let Some(t) = obj.get("type").and_then(|v| v.as_str()) else {
        return PartKind::Other;
    };
    match t {
        "image" => PartKind::AnthropicImage,
        "document" => PartKind::Document,
        _ => PartKind::Other,
    }
}

/// Translate a single Anthropic-shape image block to OpenAI-shape.
/// Returns `None` if the source shape is unrecognized (caller decides
/// strict-vs-warn).
fn rewrite_image_part(part: &Value) -> Option<Value> {
    let source = part.get("source")?.as_object()?;
    let src_type = source.get("type").and_then(|v| v.as_str())?;
    let url = match src_type {
        "base64" => {
            let media_type = source.get("media_type").and_then(|v| v.as_str())?;
            let data = source.get("data").and_then(|v| v.as_str())?;
            format!("data:{media_type};base64,{data}")
        }
        "url" => source.get("url").and_then(|v| v.as_str())?.to_string(),
        _ => return None,
    };
    Some(serde_json::json!({
        "type": "image_url",
        "image_url": {"url": url}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    fn empty_req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        }
    }

    fn run(messages: Value, strict: bool) -> Result<Map<String, Value>> {
        let req = empty_req();
        let mut obj = Map::new();
        obj.insert("messages".into(), messages);
        lift("test", &mut obj, &req, strict)?;
        Ok(obj)
    }

    #[test]
    fn anthropic_image_base64_lifts_to_data_url() {
        // Arrange
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "what's this?"},
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "iVBORw0KGgo="
                    }
                }
            ]
        }]);

        // Act
        let obj = run(messages, false).unwrap();

        // Assert
        let parts = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn anthropic_image_url_lifts_to_image_url() {
        // Arrange
        let messages = json!([{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {"type": "url", "url": "https://example.com/x.png"}
                }
            ]
        }]);

        // Act
        let obj = run(messages, false).unwrap();

        // Assert
        let parts = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["url"], "https://example.com/x.png");
    }

    #[test]
    fn document_block_warn_drops_in_default_mode() {
        // Arrange
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "see attached"},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "AA=="}}
            ]
        }]);

        // Act
        let obj = run(messages, false).unwrap();

        // Assert -- document dropped, text remains
        let parts = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");
    }

    #[test]
    fn document_block_strict_returns_err() {
        // Arrange
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "AA=="}}
            ]
        }]);

        // Act
        let res = run(messages, true);

        // Assert
        assert!(res.is_err(), "strict mode must reject document blocks");
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("strict_translation"));
        assert!(msg.contains("document"));
    }

    #[test]
    fn string_content_is_no_op() {
        // Arrange -- legacy string content shape; lift must not touch it.
        let messages = json!([{"role": "user", "content": "plain string"}]);

        // Act
        let obj = run(messages, false).unwrap();

        // Assert
        assert_eq!(obj["messages"][0]["content"], "plain string");
    }

    #[test]
    fn unknown_blocks_pass_through_verbatim() {
        // Arrange -- text + an unknown forward-compat block.
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "toolu_X", "name": "f", "input": {}}
            ]
        }]);

        // Act
        let obj = run(messages, false).unwrap();

        // Assert -- tool_use untouched (the tool_use lift handles it later)
        let parts = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["type"], "tool_use");
    }

    #[test]
    fn no_messages_is_no_op() {
        // Arrange
        let req = empty_req();
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert!(obj.get("messages").is_none());
    }
}
