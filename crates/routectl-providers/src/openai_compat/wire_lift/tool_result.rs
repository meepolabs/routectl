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
//!   `user[ text + tool_result + text + tool_result ]`
//!     -> `user[text], tool[tr1], user[text], tool[tr2]`
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

use super::reject_or_drop_unrepresentable;

/// Per-request record of which inner-block drop classes fired, so the
/// process-wide counters bump once per REQUEST rather than once per
/// dropped block: a turn whose tool results carry three unrepresentable
/// documents is one drop event an operator acts on, not three.
#[derive(Default)]
struct ToolResultDropTally {
    document: bool,
    image_source: bool,
}

impl ToolResultDropTally {
    /// Bump the `(openai-compat, class)` counters for whatever fired.
    /// Called exactly once per request from `lift`, which `lift_all` runs
    /// exactly once. Only the lenient path reaches here -- strict mode
    /// returns `Err` from the drop arm, so nothing lost is nothing counted.
    fn flush(&self) {
        if self.document {
            crate::translation_drop_metrics::record_translation_drop(
                "openai-compat",
                "tool_result_document_unrepresentable",
            );
        }
        if self.image_source {
            crate::translation_drop_metrics::record_translation_drop(
                "openai-compat",
                "tool_result_image_source_unrepresentable",
            );
        }
    }
}

pub fn lift(
    id: &str,
    obj: &mut Map<String, Value>,
    _req: &ChatRequest,
    strict: bool,
) -> Result<()> {
    let messages = match obj.remove("messages") {
        Some(Value::Array(arr)) => arr,
        // TRANSLATION-DROP: structural -- a non-array messages value is put back
        // verbatim; this lift only reshapes an array of messages.
        Some(other) => {
            obj.insert("messages".into(), other);
            return Ok(());
        }
        // TRANSLATION-DROP: structural -- no messages key means nothing to reshape.
        None => return Ok(()),
    };

    let mut tally = ToolResultDropTally::default();
    let mut rewritten: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        rewrite_message(id, msg, strict, &mut rewritten, &mut tally)?;
    }
    obj.insert("messages".into(), Value::Array(rewritten));
    tally.flush();
    Ok(())
}

fn rewrite_message(
    id: &str,
    msg: Value,
    strict: bool,
    out: &mut Vec<Value>,
    tally: &mut ToolResultDropTally,
) -> Result<()> {
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
    // Check on a BORROW first so messages with no tool_result pay no clone.
    let has_tool_result = msg
        .as_object()
        .and_then(|o| o.get("content"))
        .and_then(|c| c.as_array())
        .is_some_and(|parts| parts.iter().any(part_is_tool_result));
    if !has_tool_result {
        out.push(msg);
        return Ok(());
    }
    // A tool_result is present: take ownership of the parts now.
    // TRANSLATION-DROP: structural -- the else arm cannot be reached (the borrow
    // check above already matched an array); the message rides on untouched.
    let parts = if let Some(Value::Array(parts)) = msg.as_object().and_then(|o| o.get("content")) {
        parts.clone()
    } else {
        out.push(msg);
        return Ok(());
    };

    // Split the user message into a sequence of (user-text-chunk, tool-msg)
    // entries preserving original order.
    let mut pending_user_chunk: Vec<Value> = Vec::new();
    for part in parts {
        if part_is_tool_result(&part) {
            flush_user_chunk(&msg, &mut pending_user_chunk, out);
            if let Some(tool_msg) = build_tool_message(id, &part, strict, tally)? {
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
        for (k, v) in orig {
            // TRANSLATION-DROP: structural -- the template's own content key is
            // skipped only because the freshly-built chunk content replaces it two
            // lines below; every other key is copied through.
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

fn build_tool_message(
    id: &str,
    part: &Value,
    strict: bool,
    tally: &mut ToolResultDropTally,
) -> Result<Option<Value>> {
    // TRANSLATION-DROP: structural -- `part_is_tool_result` already proved this is
    // an object, so this arm is unreachable defensive coding, not a drop.
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
    let is_error = obj
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let content = obj
        .get("content")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let content = translate_tool_result_content(id, content, strict, tally)?;
    let content = if is_error {
        mark_error_content(content)
    } else {
        content
    };
    Ok(Some(serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_use_id,
        "content": content
    })))
}

/// Prefix a tool_result payload with an `Error:` marker so the upstream
/// model sees the failure signal that Anthropic carries via the
/// `is_error` flag (OpenAI tool messages have no such field).
/// - String content -> `format!("Error: {s}")`.
/// - Array content -> prepend a leading `{"type":"text","text":"Error: "}`
///   block so the marker survives multimodal payloads.
/// - Any other shape -> wrapped as a leading error text block + the value.
fn mark_error_content(content: Value) -> Value {
    // At the only call site, `translate_tool_result_content` has already
    // stringified scalar / object content, so in practice only the String
    // and Array arms are reachable here; the `other` arm is defensive.
    match content {
        Value::String(s) => Value::String(format!("Error: {s}")),
        Value::Array(mut arr) => {
            arr.insert(0, serde_json::json!({"type": "text", "text": "Error: "}));
            Value::Array(arr)
        }
        other => Value::Array(vec![
            serde_json::json!({"type": "text", "text": "Error: "}),
            other,
        ]),
    }
}

/// Translate a tool_result content payload for OpenAI:
/// - String -> string
/// - Text-only array -> single string (text fields joined with "\n\n").
///   Strict backends (DeepSeek, older vLLM) 400 on an array here and
///   accept only a string.
/// - Array with an image block -> array, with Anthropic image shapes
///   lifted to image_url shape (mirrors the `content` lift, which
///   doesn't recurse into tool_result). Unrepresentable inner blocks
///   (document, unknown image source) are dropped (lenient) or rejected
///   (strict) via `reject_or_drop_unrepresentable`.
/// - Object / scalar -> stringified JSON
fn translate_tool_result_content(
    id: &str,
    content: Value,
    strict: bool,
    tally: &mut ToolResultDropTally,
) -> Result<Value> {
    match content {
        Value::String(_) => Ok(content),
        Value::Array(arr) => {
            if !arr.is_empty() && arr.iter().all(is_text_block) {
                let texts = arr.iter().map(|b| b["text"].as_str().unwrap_or(""));
                let sep = "\n\n";
                let cap = texts.clone().map(str::len).sum::<usize>()
                    + sep.len() * arr.len().saturating_sub(1);
                let mut joined = String::with_capacity(cap);
                for (i, text) in texts.enumerate() {
                    if i > 0 {
                        joined.push_str(sep);
                    }
                    joined.push_str(text);
                }
                return Ok(Value::String(joined));
            }
            let mut lifted: Vec<Value> = Vec::with_capacity(arr.len());
            for block in arr {
                if let Some(out) = lift_inner_block(id, block, strict, tally)? {
                    lifted.push(out);
                }
            }
            Ok(Value::Array(lifted))
        }
        // Object or scalar: encode as string for OpenAI's wire (which
        // expects string content on tool messages outside of multimodal).
        other => Ok(Value::String(other.to_string())),
    }
}

/// Lift one inner tool_result content block to OpenAI wire shape.
/// Returns `Ok(None)` when the block is dropped (lenient mode) and
/// `Err` when strict mode rejects an unrepresentable shape. Recognized
/// base64 / url image blocks lift to `image_url`; all other recognized
/// blocks pass through with Anthropic-only `cache_control` stripped.
fn lift_inner_block(
    id: &str,
    block: Value,
    strict: bool,
    tally: &mut ToolResultDropTally,
) -> Result<Option<Value>> {
    // TRANSLATION-DROP: structural -- a non-object or untagged inner block rides
    // through verbatim; nothing is lost.
    let Some(obj) = block.as_object() else {
        return Ok(Some(block));
    };
    let Some(t) = obj.get("type").and_then(|v| v.as_str()) else {
        return Ok(Some(block));
    };
    // Cross-dialect translation lane: an Anthropic-shape `document` block
    // nested inside a tool result. Drop rather than forward -- OpenAI tool
    // message content admits only text and `image_url` elements, with no
    // document member at any nesting depth, so there is no wire slot to
    // translate onto. Baked seed verdict: it stands until this lane's own
    // wire evidence contradicts it, and it is not eligible for deletion
    // until then.
    // TRANSLATION-DROP: lane=openai-compat class=tool_result_document_unrepresentable test=inner_document_block_drops_and_warns
    if t == "document" {
        tally.document = true;
        reject_or_drop_unrepresentable(
            id,
            strict,
            "tool_result block",
            "document content block (no OpenAI equivalent)",
        )?;
        return Ok(None);
    }
    // TRANSLATION-DROP: structural -- a non-image recognized block rides through
    // with only the Anthropic-only cache_control marker stripped, which the
    // top-level content lift's own strip site owns.
    if t != "image" {
        return Ok(Some(strip_cache_control(block)));
    }
    // TRANSLATION-DROP: structural -- an image block with no `source` object at
    // all is not an image this egress can be said to have translated; it rides
    // through for the upstream to reject, matching pre-existing behavior.
    let Some(source) = obj.get("source").and_then(|v| v.as_object()) else {
        return Ok(Some(strip_cache_control(block)));
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
        Some("url") => {
            // Cross-dialect translation lane: a nested image block whose
            // `source` claims the url form but carries no `url`. Drop rather
            // than forward -- `image_url.url` is the only carrier and it is
            // mandatory, so there is nothing to put in it. Same seed status
            // as the unsupported-source arm below.
            // TRANSLATION-DROP: lane=openai-compat class=tool_result_image_source_unrepresentable test=inner_image_url_missing_url_drops_and_warns
            if let Some(u) = source.get("url").and_then(|v| v.as_str()) {
                u.to_string()
            } else {
                tally.image_source = true;
                reject_or_drop_unrepresentable(
                    id,
                    strict,
                    "tool_result block",
                    "image block with url source missing `url`",
                )?;
                return Ok(None);
            }
        }
        // Cross-dialect translation lane: a nested image block whose `source`
        // is neither the base64 nor the url form. Drop rather than forward --
        // OpenAI's nested image carrier is `image_url.url` and an
        // unrecognized source yields no URL to build, so forwarding the raw
        // Anthropic block would 400 the whole request over one element.
        // Baked seed verdict: it stands until this lane's own wire evidence
        // contradicts it, and it is not eligible for deletion until then.
        // TRANSLATION-DROP: lane=openai-compat class=tool_result_image_source_unrepresentable test=inner_image_unknown_source_drops_and_warns
        _ => {
            tally.image_source = true;
            reject_or_drop_unrepresentable(
                id,
                strict,
                "tool_result block",
                "image block with unsupported source shape (expected base64 or url)",
            )?;
            return Ok(None);
        }
    };
    Ok(Some(serde_json::json!({
        "type": "image_url",
        "image_url": {"url": url}
    })))
}

/// Strip the Anthropic-only `cache_control` field from a content block,
/// mirroring the top-level strip in `content.rs`. Non-object blocks pass
/// through unchanged.
fn strip_cache_control(mut block: Value) -> Value {
    if let Some(obj) = block.as_object_mut() {
        obj.remove("cache_control");
    }
    block
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
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
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
    fn tool_result_text_only_array_collapses_to_string() {
        // Arrange -- a tool_result whose content is an array with a
        // single text block. Strict backends 400 on the array; we must
        // collapse it to a plain string.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "42"}
                ]}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(
            msgs[0]["content"],
            json!("42"),
            "text-only array must collapse to a string"
        );
    }

    #[test]
    fn tool_result_multi_text_array_joins_with_double_newline() {
        // Arrange -- two text blocks join into one string.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "first"},
                    {"type": "text", "text": "second"}
                ]}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], json!("first\n\nsecond"));
    }

    #[test]
    fn tool_result_text_plus_image_array_stays_array() {
        // Arrange -- a non-text (image) block present, so the array form
        // must be preserved (with the image lifted to image_url).
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "see image"},
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
        assert!(
            msgs[0]["content"].is_array(),
            "array with an image block must stay an array"
        );
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "image_url");
    }

    #[test]
    fn tool_result_string_content_stays_string() {
        // Arrange -- a plain string content is unchanged.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "already a string"}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], json!("already a string"));
    }

    #[test]
    fn no_tool_result_user_passes_through() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"}
        ]);

        // Act
        let obj = run(messages);

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

    /// Strict variant of `run_result` for the strict-mode error paths.
    fn run_result_strict(messages: Value) -> routectl_core::Result<Map<String, Value>> {
        let req = empty_req();
        let mut obj = Map::new();
        obj.insert("messages".into(), messages);
        lift("test", &mut obj, &req, true)?;
        Ok(obj)
    }

    /// cache_control on an inner tool_result block (kept in array
    /// form by a sibling image) must be stripped from the wire, mirroring
    /// the top-level content strip.
    #[test]
    fn inner_block_cache_control_is_stripped() {
        // Arrange -- a text block carrying cache_control plus an image so
        // the array form survives (text-only collapses to a string).
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "ZZ=="
                    }}
                ]}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let content = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert!(
            content[0].get("cache_control").is_none(),
            "cache_control must be stripped from the inner block"
        );
    }

    /// An inner image block with an unrecognized source shape is
    /// dropped in lenient mode -- the raw Anthropic image must not reach
    /// the wire.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_image_source_unrepresentable)]
    fn inner_image_unknown_source_dropped_lenient() {
        // Arrange -- image with an unsupported source type, plus a real
        // image so the array form is preserved.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "image", "source": {"type": "weird_unknown_source"}},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "ZZ=="
                    }}
                ]}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert -- only the recognized image survives, lifted to image_url.
        let content = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "unknown-source image must be dropped");
        assert_eq!(content[0]["type"], "image_url");
    }

    /// The url-source-missing-url image is dropped in lenient mode.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_image_source_unrepresentable)]
    fn inner_image_url_missing_url_dropped_lenient() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "image", "source": {"type": "url"}},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "ZZ=="
                    }}
                ]}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let content = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "url-missing-url image must be dropped");
        assert_eq!(content[0]["type"], "image_url");
    }

    /// Strict mode rejects the unrepresentable inner image.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_image_source_unrepresentable)]
    fn inner_image_unknown_source_strict_errors() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "image", "source": {"type": "weird_unknown_source"}},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "ZZ=="
                    }}
                ]}
            ]}
        ]);

        // Act
        let res = run_result_strict(messages);

        // Assert
        assert!(
            res.is_err(),
            "strict mode must reject unknown inner image source"
        );
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("strict_translation"));
    }

    /// A nested document block inside a tool_result is dropped
    /// in lenient mode -- it must not reach the wire.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_document_unrepresentable)]
    fn inner_document_block_dropped_lenient() {
        // Arrange -- document block plus an image so the array survives.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "document", "source": {
                        "type": "base64", "media_type": "application/pdf", "data": "AA=="
                    }},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "ZZ=="
                    }}
                ]}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert -- document gone, image lifted.
        let content = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "document block must be dropped");
        assert_eq!(content[0]["type"], "image_url");
    }

    /// Strict mode rejects a nested document block.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_document_unrepresentable)]
    fn inner_document_block_strict_errors() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "document", "source": {
                        "type": "base64", "media_type": "application/pdf", "data": "AA=="
                    }},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "ZZ=="
                    }}
                ]}
            ]}
        ]);

        // Act
        let res = run_result_strict(messages);

        // Assert
        assert!(res.is_err(), "strict mode must reject inner document block");
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("strict_translation"));
        assert!(msg.contains("document"));
    }

    /// is_error with string content prefixes "Error: ".
    #[test]
    fn is_error_string_content_prefixed() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
                 "content": "boom"}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        let content = obj["messages"][0]["content"].as_str().unwrap();
        assert!(
            content.starts_with("Error: "),
            "is_error string content must start with 'Error: ', got: {content}"
        );
        assert_eq!(content, "Error: boom");
    }

    /// is_error with array content prepends a leading error
    /// text block.
    #[test]
    fn is_error_array_content_prepends_error_block() {
        // Arrange -- array kept in array form via an image block.
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
                 "content": [
                    {"type": "text", "text": "details"},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "ZZ=="
                    }}
                 ]}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert -- first block is the injected error text marker, and the
        // original blocks (details text + lifted image) survive in order.
        let content = obj["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            content.len(),
            3,
            "error marker is prepended ahead of the two original blocks"
        );
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Error: ");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "details");
        assert_eq!(content[2]["type"], "image_url");
        assert_eq!(content[2]["image_url"]["url"], "data:image/png;base64,ZZ==");
    }

    /// Absence of is_error leaves content unchanged.
    #[test]
    fn no_is_error_leaves_content_unchanged() {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        assert_eq!(obj["messages"][0]["content"], json!("ok"));
    }

    /// The `(openai-compat, class)` counter's current value, read back
    /// through the public snapshot.
    fn drop_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "openai-compat" && e.drop_class == class)
            .map_or(0, |e| e.drop_count)
    }

    /// Run the lenient lift and return the EMITTED WIRE BODY as the string
    /// an upstream would receive, plus every captured event. Asserting
    /// against the serialized body -- not a typed view of the content array
    /// -- is what catches a dropped block that actually rode upstream
    /// nested inside some other element's payload.
    fn emitted_wire(messages: Value) -> (String, Vec<routectl_testkit::CapturedEvent>) {
        let req = empty_req();
        let mut obj = Map::new();
        obj.insert("messages".into(), messages);
        let events = routectl_testkit::capture_events(|| {
            lift("test", &mut obj, &req, false).expect("lenient lift must succeed");
        });
        let wire = serde_json::to_string(&Value::Object(obj)).expect("wire body serializes");
        (wire, events)
    }

    /// A tool result carrying `block` alongside a representable image
    /// sibling (the sibling also keeps the content in array form, which
    /// text-only content would collapse out of).
    fn tool_result_with(block: Value) -> Value {
        json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    block,
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png",
                        "data": "TUFSS0VSLUdPT0Qt"
                    }}
                ]}
            ]}
        ])
    }

    /// Assert the three bars for one dropped inner block: the WARN fired
    /// with the expected structured `what`, the block's own payload is gone
    /// from the EMITTED WIRE BODY, and the representable image sibling
    /// survives in that same body. `marker` is a token unique to the
    /// fixture so the absence assertion cannot pass by accident.
    fn assert_inner_drop(block: Value, marker: &str, expected_what: &str, class: &str) {
        // Act
        let before = drop_count(class);
        let (wire, events) = emitted_wire(tool_result_with(block));
        let after = drop_count(class);

        // Assert 1 -- the WARN fired, naming the tool_result context.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
        assert_eq!(warn.field("provider"), Some("test"));
        assert_eq!(warn.field("context"), Some("tool_result block"));
        assert_eq!(warn.field("what"), Some(expected_what));

        // Assert 2 -- the dropped block's payload is off the emitted wire.
        assert!(
            !wire.contains(marker),
            "the dropped block's payload must not reach the wire, got: {wire}"
        );

        // Assert 3 -- the representable sibling survived, lifted, in that
        // same body.
        assert!(
            wire.contains("data:image/png;base64,TUFSS0VSLUdPT0Qt"),
            "the representable image sibling must survive, got: {wire}"
        );

        assert_eq!(
            after - before,
            1,
            "the drop must be counted exactly once for the request"
        );
    }

    /// NEGATIVE CONTROL: a nested document block.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_document_unrepresentable)]
    fn inner_document_block_drops_and_warns() {
        assert_inner_drop(
            json!({"type": "document", "source": {
                "type": "base64", "media_type": "application/pdf",
                "data": "TUFSS0VSLURPQ1VNRU5U"
            }}),
            "TUFSS0VSLURPQ1VNRU5U",
            "document content block (no OpenAI equivalent)",
            "tool_result_document_unrepresentable",
        );
    }

    /// NEGATIVE CONTROL: a nested image block with an unrecognized source.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_image_source_unrepresentable)]
    fn inner_image_unknown_source_drops_and_warns() {
        assert_inner_drop(
            json!({"type": "image", "source": {
                "type": "marker_future_source_kind", "blob": "unused"
            }}),
            "marker_future_source_kind",
            "image block with unsupported source shape (expected base64 or url)",
            "tool_result_image_source_unrepresentable",
        );
    }

    /// NEGATIVE CONTROL: a nested image block claiming the url source form
    /// but carrying no `url`.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_image_source_unrepresentable)]
    fn inner_image_url_missing_url_drops_and_warns() {
        assert_inner_drop(
            json!({"type": "image", "source": {
                "type": "url", "href": "marker_wrong_url_key"
            }}),
            "marker_wrong_url_key",
            "image block with url source missing `url`",
            "tool_result_image_source_unrepresentable",
        );
    }

    /// POSITIVE CONTROL for all three fixtures above: a nested image block
    /// with a REPRESENTABLE url source takes the same arm family, must
    /// survive on the emitted body, and must warn not at all. Without this,
    /// every absence assertion above would pass on a lift that dropped
    /// every nested block.
    #[test]
    fn representable_inner_url_image_survives_without_warning() {
        // Act
        let (wire, events) = emitted_wire(tool_result_with(json!({
            "type": "image",
            "source": {"type": "url", "url": "https://example.com/marker_kept.png"}
        })));

        // Assert
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "a representable nested image must not warn at all, got: {events:?}"
        );
        assert!(
            wire.contains("https://example.com/marker_kept.png"),
            "the representable nested image must reach the wire, got: {wire}"
        );
    }

    /// Two dropped nested documents in ONE request are ONE counted drop
    /// event, even when they sit in different tool results.
    #[test]
    #[serial_test::serial(openai_compat_tool_result_document_unrepresentable)]
    fn two_dropped_inner_documents_count_as_one_request_drop() {
        // Arrange
        let doc = json!({"type": "document", "source": {
            "type": "base64", "media_type": "application/pdf", "data": "QQ=="
        }});
        let keeper = json!({"type": "image", "source": {
            "type": "base64", "media_type": "image/png", "data": "Wlo="
        }});
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": [doc.clone(), keeper.clone()]},
                {"type": "tool_result", "tool_use_id": "t2",
                 "content": [doc, keeper]}
            ]}
        ]);

        // Act
        let before = drop_count("tool_result_document_unrepresentable");
        run(messages);
        let after = drop_count("tool_result_document_unrepresentable");

        // Assert
        assert_eq!(
            after - before,
            1,
            "two dropped blocks in one request must count once"
        );
    }
}
