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
//!
//! OpenAI tool messages admit text only, so lifted images leave the
//! tool message. Every tool message of one contiguous run of results
//! is emitted first, then ONE user message carrying the run's images
//! in source order, ahead of any user parts that follow the run:
//!
//!   `user[ tr1(text1, img1) + tr2(img2) + text ]`
//!     -> `tool[text1], tool[""], user[img1, img2, text]`
//!
//! A user message between two results would end the tool-result
//! segment and orphan the second result, hence one message per run.

use serde_json::{Map, Value};

use routectl_core::{ChatRequest, Result};

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
                super::super::LANE,
                "tool_result_document_unrepresentable",
            );
        }
        if self.image_source {
            crate::translation_drop_metrics::record_translation_drop(
                super::super::LANE,
                "tool_result_image_source_unrepresentable",
            );
        }
    }
}

/// Flushes the drop tally on BOTH arms of the fallible body below.
///
/// The split exists only for that: a `?` inside `lift_tallied` would otherwise
/// skip the flush, so a request that dropped content and THEN failed
/// translation would never reach the numerator -- while `request::normalize`
/// has already bumped this lane's denominator ahead of its first fallible
/// step. The rate would then read low for exactly the requests that went
/// worst. Mirrors `gemini::request::translate`/`build_body`.
pub fn lift(id: &str, obj: &mut Map<String, Value>, req: &ChatRequest, strict: bool) -> Result<()> {
    let mut tally = ToolResultDropTally::default();
    let out = lift_tallied(id, obj, req, strict, &mut tally);
    tally.flush();
    out
}

fn lift_tallied(
    id: &str,
    obj: &mut Map<String, Value>,
    _req: &ChatRequest,
    strict: bool,
    tally: &mut ToolResultDropTally,
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

    let mut rewritten: Vec<Value> = Vec::with_capacity(messages.len());
    let mut hoisted_images: Vec<Value> = Vec::new();
    let mut messages = messages.into_iter().peekable();
    while let Some(msg) = messages.next() {
        let run_continues = messages.peek().is_some_and(opens_with_tool_result);
        let mut ctx = RewriteCtx {
            id,
            strict,
            run_continues,
            hoisted_images: &mut hoisted_images,
            tally,
        };
        rewrite_message(msg, &mut ctx, &mut rewritten)?;
    }
    debug_assert!(hoisted_images.is_empty());
    obj.insert("messages".into(), Value::Array(rewritten));
    Ok(())
}

/// Per-message state for `rewrite_message`. `hoisted_images` outlives one
/// message: a run of results split across consecutive user messages is still
/// one run, so its images wait until `run_continues` is false.
struct RewriteCtx<'a> {
    id: &'a str,
    strict: bool,
    run_continues: bool,
    hoisted_images: &'a mut Vec<Value>,
    tally: &'a mut ToolResultDropTally,
}

fn opens_with_tool_result(msg: &Value) -> bool {
    let Some(obj) = msg.as_object() else {
        return false;
    };
    obj.get("role").and_then(|r| r.as_str()) == Some("user")
        && obj
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|parts| parts.first())
            .is_some_and(part_is_tool_result)
}

fn rewrite_message(msg: Value, ctx: &mut RewriteCtx<'_>, out: &mut Vec<Value>) -> Result<()> {
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
    // entries preserving original order. Images hoisted out of a run of
    // results open the user chunk that follows that run.
    let mut pending_user_chunk: Vec<Value> = Vec::new();
    for part in parts {
        if part_is_tool_result(&part) {
            flush_user_chunk(&msg, &mut pending_user_chunk, out);
            if let Some(lifted) = build_tool_message(ctx.id, &part, ctx.strict, ctx.tally)? {
                out.push(lifted.message);
                ctx.hoisted_images.extend(lifted.images);
            }
        } else {
            pending_user_chunk.append(ctx.hoisted_images);
            pending_user_chunk.push(part);
        }
    }
    if !ctx.run_continues {
        pending_user_chunk.append(ctx.hoisted_images);
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

/// One translated tool result: its `role:"tool"` message, plus the lifted
/// `image_url` blocks that must ride in the user message after the run.
struct LiftedToolResult {
    message: Value,
    images: Vec<Value>,
}

fn build_tool_message(
    id: &str,
    part: &Value,
    strict: bool,
    tally: &mut ToolResultDropTally,
) -> Result<Option<LiftedToolResult>> {
    // TRANSLATION-DROP: structural -- `part_is_tool_result` already proved this is
    // an object, so this arm is unreachable defensive coding, not a drop.
    let obj = match part.as_object() {
        Some(o) => o,
        None => return Ok(None),
    };
    // The id rides verbatim, absent or not a string included: nothing on
    // this egress pairs a tool message with its call, so only the upstream
    // can object to it.
    let tool_use_id = obj.get("tool_use_id").cloned();
    let is_error = obj
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let content = obj
        .get("content")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let (content, images) = translate_tool_result_content(id, content, strict, tally)?;
    let content = if is_error {
        mark_error_content(content)
    } else {
        content
    };
    let mut tool_msg = Map::new();
    tool_msg.insert("role".into(), Value::String("tool".into()));
    if let Some(tool_use_id) = tool_use_id {
        tool_msg.insert("tool_call_id".into(), tool_use_id);
    }
    tool_msg.insert("content".into(), content);
    Ok(Some(LiftedToolResult {
        message: Value::Object(tool_msg),
        images,
    }))
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

/// Translate a tool_result content payload for OpenAI, returning the
/// tool message content and the lifted images hoisted out of it:
/// - String -> string
/// - Array -> each block lifted first: Anthropic image shapes become
///   image_url (mirrors the `content` lift, which doesn't recurse into
///   tool_result) and every image_url block is hoisted out; unrepresentable
///   blocks (document, unknown image source) are dropped (lenient) or
///   rejected (strict) via `reject_or_drop_unrepresentable`. The blocks
///   kept on the tool message then decide its content, whether or not an
///   image was hoisted:
///   - none kept -> `""`
///   - all text -> one string, text fields joined with "\n\n". Strict
///     backends (DeepSeek, older vLLM) 400 on an array here and accept
///     only a string.
///   - any other block kept -> the array of kept blocks
/// - Object / scalar -> stringified JSON
///
/// SEED-VERDICT: acceptance-seed -- every backend accepts the joined string, so
/// no rejection could retire the join, and it rewrites tool history:
/// prefix-changing, retirable only by a canary showing the unjoined array
/// accepted, and no canary samples this site yet.
fn translate_tool_result_content(
    id: &str,
    content: Value,
    strict: bool,
    tally: &mut ToolResultDropTally,
) -> Result<(Value, Vec<Value>)> {
    match content {
        Value::String(_) => Ok((content, Vec::new())),
        Value::Array(arr) => {
            let mut kept: Vec<Value> = Vec::with_capacity(arr.len());
            let mut images: Vec<Value> = Vec::new();
            for block in arr {
                let Some(out) = lift_inner_block(id, block, strict, tally)? else {
                    continue;
                };
                // TRANSLATION-DROP: structural -- a lifted image is relocated, not
                // lost: OpenAI tool messages admit only text, so it rides in the user
                // message emitted right after the run of tool messages.
                if is_image_url_block(&out) {
                    images.push(out);
                } else {
                    kept.push(out);
                }
            }
            if !kept.iter().all(is_text_block) {
                return Ok((Value::Array(kept), images));
            }
            Ok((join_text_blocks(&kept), images))
        }
        // Object or scalar: encode as string for OpenAI's wire (which
        // expects string content on tool messages outside of multimodal).
        other => Ok((Value::String(other.to_string()), Vec::new())),
    }
}

/// Join the `text` fields of text blocks with "\n\n" into one string.
fn join_text_blocks(blocks: &[Value]) -> Value {
    let texts = blocks.iter().map(|b| b["text"].as_str().unwrap_or(""));
    let sep = "\n\n";
    let cap =
        texts.clone().map(str::len).sum::<usize>() + sep.len() * blocks.len().saturating_sub(1);
    let mut joined = String::with_capacity(cap);
    for (i, text) in texts.enumerate() {
        if i > 0 {
            joined.push_str(sep);
        }
        joined.push_str(text);
    }
    Value::String(joined)
}

fn is_image_url_block(part: &Value) -> bool {
    part.as_object()
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str())
        == Some("image_url")
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
#[path = "tool_result_tests.rs"]
mod tests;
