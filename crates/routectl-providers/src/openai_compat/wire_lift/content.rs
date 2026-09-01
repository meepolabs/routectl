//! Lift Anthropic-shape content blocks inside `obj["messages"][].content`
//! to the OpenAI-compat wire shape.
//!
//! Walks the wire-form messages array (the body produced after
//! `serde_json::to_value(req)` then `dialect.apply_request`) and
//! rewrites:
//!
//!   {"type":"image", "source":{"type":"base64", "media_type", "data"}}
//!     -> {"type":"image_url", "image_url":{"url":`"data:<media_type>;base64,<data>"`}}
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

use routectl_core::{ChatRequest, Result};

use super::reject_or_drop_unrepresentable;

/// Per-request record of which content-block drop classes fired, so the
/// process-wide counters are bumped once per REQUEST rather than once
/// per dropped block: a turn carrying three unrepresentable documents is
/// one drop event an operator should see, not three.
#[derive(Default)]
struct ContentDropTally {
    document: bool,
    image_source: bool,
}

impl ContentDropTally {
    /// Bump the `(openai-compat, class)` counters for whatever fired.
    /// Called exactly once per request from `lift`, which `lift_all`
    /// itself runs exactly once. Only the lenient path reaches here --
    /// strict mode returns `Err` from the drop arm, so nothing was lost
    /// and nothing is counted.
    fn flush(&self) {
        if self.document {
            crate::translation_drop_metrics::record_translation_drop(
                "openai-compat",
                "document_block_unrepresentable",
            );
        }
        if self.image_source {
            crate::translation_drop_metrics::record_translation_drop(
                "openai-compat",
                "image_source_unrepresentable",
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
    let mut tally = ContentDropTally::default();
    let out = lift_tallied(id, obj, req, strict, &mut tally);
    tally.flush();
    out
}

fn lift_tallied(
    id: &str,
    obj: &mut Map<String, Value>,
    _req: &ChatRequest,
    strict: bool,
    tally: &mut ContentDropTally,
) -> Result<()> {
    let messages = match obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return Ok(()),
    };
    for (msg_idx, msg) in messages.iter_mut().enumerate() {
        // TRANSLATION-DROP: structural -- a non-object messages entry has no
        // content array to walk; the entry itself rides on untouched.
        let Some(msg_obj) = msg.as_object_mut() else {
            continue;
        };
        // TRANSLATION-DROP: structural -- a message with no content key has no
        // blocks to rewrite; the message rides on untouched.
        let Some(content_val) = msg_obj.get_mut("content") else {
            continue;
        };
        // String / null / non-array content -- nothing to do.
        // TRANSLATION-DROP: structural -- legacy string / null content carries no
        // blocks to rewrite and is emitted verbatim.
        let Some(parts) = content_val.as_array_mut() else {
            continue;
        };
        rewrite_parts(id, msg_idx, parts, strict, tally)?;
        // Strip Anthropic-only per-block `cache_control` from every
        // surviving part. The block-level warn path in
        // `request::check_dropped_anthropic_fields` was informational
        // only; the strip itself never happened, so a canonical
        // request with `cache_control` on any text / tool_use /
        // tool_result / thinking content block would emit the field
        // into the openai-compat wire body and 400 strict hosts.
        // Caught by `contract_egress::scenario_5_cache_control_positions::openai_compat_egress`.
        for part in parts.iter_mut() {
            if let Some(obj) = part.as_object_mut() {
                obj.remove("cache_control");
            }
        }
    }
    Ok(())
}

fn rewrite_parts(
    id: &str,
    msg_idx: usize,
    parts: &mut Vec<Value>,
    strict: bool,
    tally: &mut ContentDropTally,
) -> Result<()> {
    // Build a new vec; drop document blocks; rewrite image blocks.
    let original = std::mem::take(parts);
    for part in original {
        match part_kind(&part) {
            PartKind::AnthropicImage => match rewrite_image_part(&part) {
                Some(rewritten) => parts.push(rewritten),
                // Cross-dialect translation lane: an Anthropic-shape image
                // block whose `source` is neither the base64 nor the url form
                // this egress knows how to build a data / plain URL from.
                // Drop rather than forward -- the OpenAI-compat wire has one
                // image carrier, `image_url.url`, and an unrecognized source
                // yields no URL to put in it, so forwarding the raw Anthropic
                // block would 400 a strict host with a shape it cannot parse
                // instead of losing one block. Baked seed verdict: it stands
                // until this lane's own wire evidence contradicts it, and is
                // not eligible for deletion until then.
                // TRANSLATION-DROP: lane=openai-compat class=image_source_unrepresentable test=unsupported_image_source_drops_and_warns
                None => {
                    tally.image_source = true;
                    reject_or_drop_unrepresentable(
                        id,
                        strict,
                        &format!("message {msg_idx}"),
                        "image block with unsupported source shape (expected base64 or url)",
                    )?;
                }
            },
            // Cross-dialect translation lane: an Anthropic-shape `document`
            // block reaching the OpenAI-compat egress. Drop rather than
            // forward -- OpenAI chat-completions message content has no
            // document member at all (its `file` block is a distinct shape
            // this canonical part is not, and carries no Anthropic `source`
            // union), so there is no wire slot to translate onto. Baked seed
            // verdict: it stands until this lane's own wire evidence
            // contradicts it, and is not eligible for deletion until then.
            // TRANSLATION-DROP: lane=openai-compat class=document_block_unrepresentable test=document_block_drops_and_warns
            PartKind::Document => {
                tally.document = true;
                reject_or_drop_unrepresentable(
                    id,
                    strict,
                    &format!("message {msg_idx}"),
                    "document content block (no OpenAI equivalent)",
                )?;
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
///
/// Every `?` and the `_` arm below funnel into ONE caller-side outcome:
/// `rewrite_parts`'s `None` arm, which is where the warn, the counter,
/// and the marker live. Nothing is lost or counted at this level.
// TRANSLATION-DROP: structural -- every exit here returns None to the caller's
// single marked drop arm, which owns the warn and the counter.
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
    #[serial_test::serial(openai_compat_document_block_unrepresentable)]
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

    /// The `(openai-compat, class)` counter's current value, read back
    /// through the public snapshot.
    fn drop_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "openai-compat" && e.drop_class == class)
            .map_or(0, |e| e.drop_count)
    }

    /// The full emitted wire body of a single-user-message request, as the
    /// serialized string an upstream would receive. Asserting against this
    /// -- rather than against a typed view -- is what catches a dropped
    /// shape that actually rode to the upstream nested inside some other
    /// key's payload.
    fn emitted_wire(messages: Value) -> String {
        let obj = run(messages, false).expect("lenient lift must succeed");
        serde_json::to_string(&Value::Object(obj)).expect("wire body serializes")
    }

    /// NEGATIVE CONTROL: a document block is dropped, the drop surfaces
    /// through its WARN with structured fields, and the block's payload is
    /// gone from the EMITTED WIRE BODY -- not merely absent from a typed
    /// view of the parts array.
    #[test]
    #[serial_test::serial(openai_compat_document_block_unrepresentable)]
    fn document_block_drops_and_warns() {
        // Arrange -- a document alongside a representable text sibling.
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "see attached"},
                {"type": "document", "source": {
                    "type": "base64", "media_type": "application/pdf",
                    "data": "TUFSS0VSLURPQ1VNRU5U"
                }}
            ]
        }]);

        // Act
        let before = drop_count("document_block_unrepresentable");
        let mut wire = String::new();
        let events = routectl_testkit::capture_events(|| {
            wire = emitted_wire(messages);
        });
        let after = drop_count("document_block_unrepresentable");

        // Assert 1 -- the WARN fired, with the structured fields naming it.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
        assert_eq!(warn.field("provider"), Some("test"));
        assert_eq!(warn.field("context"), Some("message 0"));
        assert_eq!(
            warn.field("what"),
            Some("document content block (no OpenAI equivalent)")
        );

        // Assert 2 -- the payload is absent from the EMITTED WIRE BODY.
        assert!(
            !wire.contains("TUFSS0VSLURPQ1VNRU5U"),
            "the document payload must not reach the wire in any form, got: {wire}"
        );
        assert!(
            !wire.contains("document"),
            "no document block may survive anywhere in the body, got: {wire}"
        );

        // Assert 3 -- the representable sibling survived in that same body.
        assert!(
            wire.contains("see attached"),
            "the representable text sibling must survive, got: {wire}"
        );

        assert_eq!(
            after - before,
            1,
            "the drop must be counted exactly once for the request"
        );
    }

    /// POSITIVE CONTROL for the fixture above: the same request shape with
    /// the document replaced by a representable image block must warn not
    /// at all and must land BOTH blocks on the wire. Without this, the
    /// negative control's absence assertions could pass on a lift that
    /// drops every block indiscriminately.
    #[test]
    fn representable_blocks_survive_without_warning() {
        // Arrange
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "see attached"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png",
                    "data": "TUFSS0VSLUlNQUdF"
                }}
            ]
        }]);

        // Act
        let mut wire = String::new();
        let events = routectl_testkit::capture_events(|| {
            wire = emitted_wire(messages);
        });

        // Assert
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "a fully representable turn must not warn at all, got: {events:?}"
        );
        assert!(
            wire.contains("see attached") && wire.contains("TUFSS0VSLUlNQUdF"),
            "both representable blocks must reach the wire, got: {wire}"
        );
    }

    /// NEGATIVE CONTROL: an image block whose source shape yields no URL is
    /// dropped, warns with its own structured `what`, and its payload does
    /// not reach the emitted body.
    #[test]
    // Sole holder of this guard name today: exactly one fixture in the crate
    // reaches this arm, so the guard excludes nothing yet. It is here so the
    // NEXT test that constructs this shape shares a name rather than silently
    // making the delta below flaky.
    #[serial_test::serial(openai_compat_image_source_unrepresentable)]
    fn unsupported_image_source_drops_and_warns() {
        // Arrange -- an unrepresentable source alongside a representable
        // image sibling, so the positive control rides in the same body.
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "image", "source": {
                    "type": "future_source_kind", "blob": "TUFSS0VSLUJBRC1TUkM"
                }},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png",
                    "data": "TUFSS0VSLUdPT0Qt"
                }}
            ]
        }]);

        // Act
        let before = drop_count("image_source_unrepresentable");
        let mut wire = String::new();
        let events = routectl_testkit::capture_events(|| {
            wire = emitted_wire(messages);
        });
        let after = drop_count("image_source_unrepresentable");

        // Assert 1 -- the WARN fired with the source-shape wording.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
        assert_eq!(
            warn.field("what"),
            Some("image block with unsupported source shape (expected base64 or url)")
        );

        // Assert 2 -- the unrepresentable block's payload is off the wire.
        assert!(
            !wire.contains("TUFSS0VSLUJBRC1TUkM") && !wire.contains("future_source_kind"),
            "the unrepresentable image must not reach the wire, got: {wire}"
        );

        // Assert 3 -- the representable sibling survived, lifted.
        assert!(
            wire.contains("data:image/png;base64,TUFSS0VSLUdPT0Qt"),
            "the representable image must survive as a data URL, got: {wire}"
        );

        assert_eq!(
            after - before,
            1,
            "the drop must be counted exactly once for the request"
        );
    }

    /// Two dropped documents in ONE request are ONE counted drop event.
    /// Counting per block would make the operator-facing rate report
    /// block volume rather than affected-request volume.
    #[test]
    #[serial_test::serial(openai_compat_document_block_unrepresentable)]
    fn two_dropped_documents_count_as_one_request_drop() {
        // Arrange
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "QQ=="}},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "Qg=="}}
            ]
        }]);

        // Act
        let before = drop_count("document_block_unrepresentable");
        run(messages, false).unwrap();
        let after = drop_count("document_block_unrepresentable");

        // Assert
        assert_eq!(
            after - before,
            1,
            "two dropped blocks in one request must count once"
        );
    }

    #[test]
    #[serial_test::serial(openai_compat_document_block_unrepresentable)]
    fn a_drop_before_a_translation_error_is_still_counted() {
        // The drop happens, THEN strict mode rejects a later block in the same
        // request. The counter must still move: `request::normalize` bumps this
        // lane's denominator before its first fallible step, so a numerator
        // that skipped failed requests would make the drop rate read LOW for
        // exactly the requests that went worst. This is why `lift` flushes the
        // tally outside its fallible body.
        let messages = json!([{
            "role": "user",
            "content": [
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "QQ=="}}
            ]
        }]);

        let before = drop_count("document_block_unrepresentable");
        let res = run(messages, true);
        let after = drop_count("document_block_unrepresentable");

        assert!(res.is_err(), "strict mode must reject the document block");
        assert_eq!(
            after - before,
            1,
            "a drop on a request that then failed must still be counted"
        );
    }

    #[test]
    #[serial_test::serial(openai_compat_document_block_unrepresentable)]
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
