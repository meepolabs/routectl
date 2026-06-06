//! Lift Anthropic-shape `tool_result` content blocks on user
//! messages into OpenAI-shape `role:"tool"` messages.
//!
//! Anthropic shape:
//!
//!   { "role": "user", "content": [
//!       {"type":"tool_result", "tool_use_id":"toolu_X",
//!        "content":"42"}
//!   ]}
//!
//! OpenAI shape -- a separate message with `role:"tool"`:
//!
//!   { "role": "tool", "tool_call_id":"toolu_X", "content":"42" }
//!
//! Mixed content (text + tool_result blocks in one user message)
//! splits into multiple wire messages preserving order:
//!
//!   user[ text + tool_result + text + tool_result ]
//!     -> user[text], tool[tr1], user[text], tool[tr2]
//!
//! `tool_result.content` may be a string OR an array of blocks
//! (Anthropic supports `[{type:"text", text}]` and image blocks
//! inside results). Strings flow through as strings; arrays
//! carry through with inner image blocks already lifted by the
//! preceding `content` lift -- but that lift only walks
//! `messages[].content[]`, not nested-inside-tool_result. So we
//! lift inner image shapes here too.

use serde_json::{Map, Value};

use routectl_core::{ChatRequest, Error, Result};

pub fn lift(
    id: &str,
    obj: &mut Map<String, Value>,
    _req: &ChatRequest,
    _strict: bool,
) -> Result<()> {
    let messages = match obj.remove("messages") {
        Some(Value::Array(arr)) => arr,
        Some(other) => {
            obj.insert("messages".into(), other);
            return Ok(());
        }
        None => return Ok(()),
    };

    let mut rewritten: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        rewrite_message(id, msg, &mut rewritten)?;
    }
    obj.insert("messages".into(), Value::Array(rewritten));
    Ok(())
}

fn rewrite_message(id: &str, msg: Value, out: &mut Vec<Value>) -> Result<()> {
    let role_is_user = msg
        .as_object()
        .and_then(|o| o.get("role"))
        .and_then(|r| r.as_str())
        == Some("user");
    if !role_is_user {
        out.push(msg);
        return Ok(());
    }
    // Only act when content is an array carrying at least one tool_result.
    let parts_clone = msg
        .as_object()
        .and_then(|o| o.get("content"))
        .and_then(|c| c.as_array())
        .cloned();
    let parts = match parts_clone {
        Some(p) => p,
        None => {
            out.push(msg);
            return Ok(());
        }
    };
    let has_tool_result = parts.iter().any(part_is_tool_result);
    if !has_tool_result {
        out.push(msg);
        return Ok(());
    }

    // Split the user message into a sequence of (user-text-chunk, tool-msg)
    // entries preserving original order.
    let mut pending_user_chunk: Vec<Value> = Vec::new();
    for part in parts {
        if part_is_tool_result(&part) {
            flush_user_chunk(&msg, &mut pending_user_chunk, out);
            if let Some(tool_msg) = build_tool_message(id, &part)? {
                out.push(tool_msg);
            }
        } else {
            pending_user_chunk.push(part);
        }
    }
    flush_user_chunk(&msg, &mut pending_user_chunk, out);
    Ok(())
}

fn flush_user_chunk(template: &Value, pending: &mut Vec<Value>, out: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let chunk = std::mem::take(pending);
    // Collapse a single text block into a string for OpenAI ergonomics.
    let content = if chunk.len() == 1 && is_text_block(&chunk[0]) {
        Value::String(chunk[0]["text"].as_str().unwrap_or("").to_string())
    } else {
        Value::Array(chunk)
    };
    let mut new_msg = Map::new();
    if let Some(orig) = template.as_object() {
        for (k, v) in orig.iter() {
            if k == "content" {
                continue;
            }
            new_msg.insert(k.clone(), v.clone());
        }
    }
    new_msg.insert("content".into(), content);
    out.push(Value::Object(new_msg));
}

fn part_is_tool_result(part: &Value) -> bool {
    part.as_object()
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str())
        == Some("tool_result")
}

fn is_text_block(part: &Value) -> bool {
    part.as_object()
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str())
        == Some("text")
}

fn build_tool_message(id: &str, part: &Value) -> Result<Option<Value>> {
    let obj = match part.as_object() {
        Some(o) => o,
        None => return Ok(None),
    };
    let tool_use_id = match obj.get("tool_use_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Err(Error::normalize_request(
                id,
                "tool_result block is missing required `tool_use_id`; \
                 cannot construct OpenAI-compat tool message",
            ));
        }
    };
    let content = obj
        .get("content")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let content = normalize_tool_result_content(content);
    Ok(Some(serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_use_id,
        "content": content
    })))
}

/// Normalize a tool_result content payload for OpenAI:
/// - String -> string
/// - Array of blocks -> array, with Anthropic image shapes lifted to
///   image_url shape (mirrors the `content` lift, which doesn't
///   recurse into tool_result).
/// - Object / scalar -> stringified JSON
fn normalize_tool_result_content(content: Value) -> Value {
    match content {
        Value::String(_) => content,
        Value::Array(arr) => {
            let lifted: Vec<Value> = arr.into_iter().map(lift_inner_block).collect();
            Value::Array(lifted)
        }
        // Object or scalar: encode as string for OpenAI's wire (which
        // expects string content on tool messages outside of multimodal).
        other => Value::String(other.to_string()),
    }
}

fn lift_inner_block(block: Value) -> Value {
    let Some(obj) = block.as_object() else {
        return block;
    };
    let Some(t) = obj.get("type").and_then(|v| v.as_str()) else {
        return block;
    };
    if t != "image" {
        return block;
    }
    let Some(source) = obj.get("source").and_then(|v| v.as_object()) else {
        return block;
    };
    let url = match source.get("type").and_then(|v| v.as_str()) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream");
            let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
            format!("data:{media_type};base64,{data}")
        }
        Some("url") => match source.get("url").and_then(|v| v.as_str()) {
            Some(u) => u.to_string(),
            None => return block,
        },
        _ => return block,
    };
    serde_json::json!({
        "type": "image_url",
        "image_url": {"url": url}
    })
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

    fn run(messages: Value) -> Map<String, Value> {
        let req = empty_req();
        let mut obj = Map::new();
        obj.insert("messages".into(), messages);
        lift("test", &mut obj, &req, false).unwrap();
        obj
    }

    /// Variant of `run` that surfaces the `Result` so tests can assert
    /// on the error path without a panic.
    fn run_result(messages: Value) -> routectl_core::Result<Map<String, Value>> {
        let req = empty_req();
        let mut obj = Map::new();
        obj.insert("messages".into(), messages);
        lift("test", &mut obj, &req, false)?;
        Ok(obj)
    }

    #[test]
    fn single_tool_result_becomes_role_tool_message() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01ABC", "content": "4"}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "single tool_result -> single tool message");
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "toolu_01ABC");
        assert_eq!(msgs[0]["content"], "4");
    }

    #[test]
    fn multiple_tool_results_split_into_multiple_messages() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "first"},
                {"type": "tool_result", "tool_use_id": "t2", "content": "second"}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "t1");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "t2");
    }

    #[test]
    fn mixed_text_and_tool_result_splits_into_user_then_tool() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "text", "text": "see result:"},
                {"type": "tool_result", "tool_use_id": "t1", "content": "the answer"}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "see result:");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "t1");
        assert_eq!(msgs[1]["content"], "the answer");
    }

    #[test]
    fn tool_result_with_array_content_lifts_inner_image() {
        // Arrange -- tool_result containing an Anthropic image block.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "here is the rendering"},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "ZZ=="
                    }}
                ]}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,ZZ==");
    }

    #[test]
    fn no_tool_result_user_passes_through() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"}
        ]);

        // Act
        let obj = run(messages.clone());

        // Assert -- structure preserved
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "hello");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn tool_result_missing_tool_use_id_returns_error() {
        // Arrange -- malformed tool_result without tool_use_id.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "content": "orphan"}
            ]}
        ]);

        // Act -- must hard-fail, not silently drop.
        let result = run_result(messages);

        // Assert
        assert!(
            result.is_err(),
            "tool_result missing tool_use_id must return an error, not silently drop"
        );
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("tool_use_id"),
            "error message must mention tool_use_id, got: {msg}"
        );
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
