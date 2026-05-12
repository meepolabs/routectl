//! Lift Anthropic-shape `tool_use` content blocks on assistant
//! messages into the OpenAI-compat top-level `tool_calls` field.
//!
//! Anthropic assistant content can mix text + tool_use blocks:
//!
//!   { "role": "assistant", "content": [
//!       {"type":"text", "text":"I'll check"},
//!       {"type":"tool_use", "id":"toolu_X", "name":"get_weather",
//!        "input":{"city":"SF"}}
//!   ]}
//!
//! OpenAI shape: assistant message carries `content` (text or null) and
//! a sibling `tool_calls` array:
//!
//!   { "role": "assistant",
//!     "content": "I'll check",
//!     "tool_calls": [{
//!         "id":"toolu_X", "type":"function",
//!         "function":{"name":"get_weather",
//!                     "arguments":"{\"city\":\"SF\"}"}
//!     }]
//!   }
//!
//! When stripping all tool_use blocks leaves no remaining text content,
//! `content` is set to `null` (OpenAI accepts this for tool-only turns).
//!
//! Runs AFTER the `content` lift so image rewriting is already done;
//! tool_use itself is left alone by the content lift.

use serde_json::{Map, Value};

use routectl_core::{ChatRequest, Error, Result};

pub fn lift(
    id: &str,
    obj: &mut Map<String, Value>,
    _req: &ChatRequest,
    _strict: bool,
) -> Result<()> {
    let messages = match obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return Ok(()),
    };
    for msg in messages.iter_mut() {
        let Some(msg_obj) = msg.as_object_mut() else {
            continue;
        };
        let role = msg_obj
            .get("role")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string());
        if role.as_deref() != Some("assistant") {
            continue;
        }
        rewrite_assistant_message(id, msg_obj)?;
    }
    Ok(())
}

fn rewrite_assistant_message(
    id: &str,
    msg: &mut Map<String, Value>,
) -> Result<()> {
    let parts = match msg.get("content").and_then(|c| c.as_array()) {
        Some(p) => p.clone(),
        None => return Ok(()),
    };

    // Partition into surviving content blocks and lifted tool_calls.
    let mut surviving: Vec<Value> = Vec::with_capacity(parts.len());
    let mut tool_calls: Vec<Value> = Vec::new();
    for part in parts {
        if part_is_tool_use(&part) {
            tool_calls.push(tool_use_to_tool_call(id, &part)?);
        } else {
            surviving.push(part);
        }
    }

    if tool_calls.is_empty() {
        // No tool_use blocks. Leave content untouched.
        return Ok(());
    }

    // Append to any pre-existing tool_calls (e.g. from a dialect that
    // already populated some). New entries go AFTER existing ones to
    // preserve order.
    if let Some(existing) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
        existing.extend(tool_calls);
    } else {
        msg.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    // Rewrite content. If only text-shaped blocks survive and there's
    // exactly one, collapse to a string for OpenAI ergonomics. If
    // nothing survives, set content to null.
    if surviving.is_empty() {
        msg.insert("content".into(), Value::Null);
    } else if surviving.len() == 1 && is_text_block(&surviving[0]) {
        let text = surviving[0]["text"].as_str().unwrap_or("").to_string();
        msg.insert("content".into(), Value::String(text));
    } else {
        msg.insert("content".into(), Value::Array(surviving));
    }
    Ok(())
}

fn part_is_tool_use(part: &Value) -> bool {
    part.as_object()
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str())
        == Some("tool_use")
}

fn is_text_block(part: &Value) -> bool {
    part.as_object()
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str())
        == Some("text")
}

fn tool_use_to_tool_call(id: &str, part: &Value) -> Result<Value> {
    let obj = part.as_object().ok_or_else(|| {
        Error::normalize_request(id, format!("tool_use block is not an object: {part}"))
    })?;
    let tool_id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::normalize_request(id, "tool_use block missing `id`"))?
        .to_string();
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::normalize_request(id, "tool_use block missing `name`"))?
        .to_string();
    let input = obj
        .get("input")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let arguments = serde_json::to_string(&input).map_err(|e| {
        Error::normalize_request(id, format!("failed to encode tool_use input: {e}"))
    })?;
    Ok(serde_json::json!({
        "id": tool_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
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

    fn run(messages: Value) -> Map<String, Value> {
        let req = empty_req();
        let mut obj = Map::new();
        obj.insert("messages".into(), messages);
        lift("test", &mut obj, &req, false).unwrap();
        obj
    }

    #[test]
    fn assistant_with_text_only_no_op() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "text", "text": "hello"}]}
        ]);

        // Act
        let obj = run(messages);

        // Assert -- no tool_calls, content unchanged
        assert!(obj["messages"][1].get("tool_calls").is_none());
        let content = &obj["messages"][1]["content"];
        // content is still the array form (untouched).
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn single_tool_use_lifts_with_null_content() {
        // Arrange -- assistant has only a tool_use block.
        let messages = json!([
            {"role": "user", "content": "calc"},
            {"role": "assistant", "content": [{
                "type": "tool_use",
                "id": "toolu_01",
                "name": "calculator",
                "input": {"expr": "2+2"}
            }]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let assistant = &obj["messages"][1];
        assert!(assistant["content"].is_null(), "content should be null when only tool_use");
        let tcs = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "toolu_01");
        assert_eq!(tcs[0]["type"], "function");
        assert_eq!(tcs[0]["function"]["name"], "calculator");
        let args: Value = serde_json::from_str(tcs[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"expr": "2+2"}));
    }

    #[test]
    fn text_then_tool_use_lifts_text_to_string() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": "calc"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "I'll check"},
                {"type": "tool_use", "id": "toolu_X", "name": "get_weather", "input": {"city": "SF"}}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert -- text collapsed to string, tool_calls populated
        let assistant = &obj["messages"][1];
        assert_eq!(assistant["content"], "I'll check");
        let tcs = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn multiple_tool_use_produces_multi_element_tool_calls() {
        // Arrange
        let messages = json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "a", "input": {}},
                {"type": "tool_use", "id": "t2", "name": "b", "input": {"x": 1}}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let tcs = obj["messages"][0]["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0]["id"], "t1");
        assert_eq!(tcs[1]["id"], "t2");
        assert_eq!(tcs[1]["function"]["arguments"], "{\"x\":1}");
    }

    #[test]
    fn user_message_with_tool_use_blocks_is_left_alone() {
        // Arrange -- only assistant tool_use should be lifted; user
        // content is the tool_result lift's job.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_use", "id": "wrong_role", "name": "x", "input": {}}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert -- user's content array is untouched, no tool_calls injected
        assert!(obj["messages"][0].get("tool_calls").is_none());
        assert!(obj["messages"][0]["content"].is_array());
    }

    #[test]
    fn assistant_string_content_is_no_op() {
        // Arrange
        let messages = json!([
            {"role": "assistant", "content": "ok"}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        assert_eq!(obj["messages"][0]["content"], "ok");
        assert!(obj["messages"][0].get("tool_calls").is_none());
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
