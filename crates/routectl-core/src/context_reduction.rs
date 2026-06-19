//! Lossless, cache-safe JSON-whitespace minifier over a request's mutable
//! message tail.
//!
//! WHY THIS SHAPE (the scope is non-obvious): routectl parses every JSON
//! body into canonical types and re-serializes compactly with serde_json,
//! so a structured `Value::Object` / `Value::Array` is ALREADY
//! whitespace-free on the wire -- minifying it is a no-op. Whitespace
//! survives ONLY inside a `Value::String` (serde preserves string contents
//! verbatim). A tool that returns pretty-printed JSON as TEXT (e.g.
//! `json.dumps(x, indent=2)`) ships that whitespace to the model. This
//! transform therefore targets JSON-valued `Value::String` payloads --
//! `ToolResult.content` and `ToolUse.input` when they are strings -- not
//! structured Values.
//!
//! CACHE SAFETY: the transform only touches messages at or after
//! `mutable_suffix_start` (the boundary after the last caller
//! `cache_control` marker). Frozen-prefix bytes are never changed, so no
//! caller prompt-cache breakpoint is invalidated.
//!
//! LOSSLESSNESS: `minify_json_whitespace` is a custom byte lexer that drops
//! only insignificant whitespace OUTSIDE string literals and copies every
//! byte inside string literals verbatim (escape-aware). It never
//! reparses-and-reserializes, so numbers (`1.0` stays `1.0`), key order,
//! and duplicate keys are byte-preserved. Three guards make losslessness a
//! hard constraint: the input must parse as JSON, the output must parse and
//! equal the original parsed `Value`, and the output must be strictly
//! shorter (else there was nothing to strip).

use serde_json::Value;

use crate::cache_control::mutable_suffix_start;
use crate::content_part::{ContentPart, KnownContentPart};
use crate::schema::{ChatRequest, MessageContent};

/// Divisor for the rough bytes-to-tokens estimate. Four bytes per token is
/// the conventional English-text heuristic; good enough for an
/// operator-facing "tokens saved" signal, not a billing figure.
const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

/// How much a minify pass removed. Mirrors EPIC-A's `CacheInjection` style:
/// a small owned outcome record the router maps to operator-facing strings.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReductionDelta {
    /// Number of JSON-valued strings that were compacted.
    pub strings_minified: usize,
    /// Total bytes removed across all compacted strings.
    pub bytes_saved: usize,
    /// Rough token-savings estimate (`bytes_saved / 4`).
    pub est_tokens_saved: usize,
}

/// Outcome of an `apply_json_minify` pass. The router maps these to
/// operator-facing strategy strings; usage-DB strings do not belong here.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReductionOutcome {
    /// There is no mutable tail to operate on (every message is frozen by a
    /// caller `cache_control` marker, or there are no messages).
    NoMutableTail,
    /// A mutable tail exists but nothing was minified; the request is
    /// byte-identical to its input.
    NothingToStrip,
    /// At least one JSON-valued string was compacted.
    Applied(ReductionDelta),
}

/// Strip insignificant whitespace from a JSON document held as a string.
///
/// Returns `Some(minified)` ONLY when `s` is valid JSON, minification
/// removed at least one byte, AND the result provably re-parses to the same
/// `Value`; otherwise `None` (the caller keeps the original string).
///
/// The lexer toggles an in-string-literal flag on each unescaped `"`. Inside
/// a string literal every byte is copied verbatim; on a backslash the
/// backslash AND the next byte are copied together, so `\"` does not end the
/// string and `\\` is not misread as an escape of the following byte.
/// Outside string literals, the four insignificant whitespace bytes (space,
/// tab, LF, CR) are dropped and all other structural bytes copied verbatim.
/// Numbers, key order, and duplicate keys are byte-preserved because the
/// document is never reparsed-and-reserialized.
#[must_use]
pub fn minify_json_whitespace(s: &str) -> Option<String> {
    // Guard (a): non-JSON text has semantic whitespace (source code, logs,
    // prose) -- never touch it.
    let original: Value = serde_json::from_str(s).ok()?;

    let minified = strip_insignificant_whitespace(s)?;

    // Guard (c): nothing stripped (already compact) -- signal no-op.
    if minified.len() >= s.len() {
        return None;
    }

    // Guard (b): the result must parse AND equal the original parsed Value.
    let reparsed: Value = serde_json::from_str(&minified).ok()?;
    if reparsed != original {
        return None;
    }

    Some(minified)
}

/// The whitespace-only lexer. Pure string transform; correctness of the
/// in-string / escape handling is enforced by the re-parse guard in
/// `minify_json_whitespace`.
fn strip_insignificant_whitespace(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if in_string {
            out.push(b);
            if b == b'\\' {
                // Copy the escaped byte verbatim so `\"` / `\\` are not
                // misread. A trailing backslash (malformed) just copies
                // nothing more; the re-parse guard rejects the result.
                if i + 1 < bytes.len() {
                    out.push(bytes[i + 1]);
                    i += 2;
                    continue;
                }
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => {
                in_string = true;
                out.push(b);
            }
            b' ' | b'\t' | b'\n' | b'\r' => {
                // Insignificant whitespace outside a string literal: drop.
            }
            _ => out.push(b),
        }
        i += 1;
    }

    // Only ASCII whitespace (never a UTF-8 continuation byte) is dropped from
    // already-valid UTF-8 input, so this conversion cannot fail in practice.
    // On the impossible failure we return None and the caller keeps the
    // original string (fail-closed; never panics).
    String::from_utf8(out).ok()
}

/// Minify JSON-valued STRING content in the request's mutable message tail.
///
/// Computes `mutable_suffix_start(req)`; if `None`, returns
/// `NoMutableTail` and leaves `req` untouched. Otherwise, for each message
/// in `req.messages[start..]` whose content is `MessageContent::Parts`, it
/// minifies every `ToolResult.content` and `ToolUse.input` that is a
/// `Value::String`. Structured (non-string) Values, `ContentPart::Other`,
/// thinking blocks, and anything before `start` are never touched.
///
/// Fail-closed: a per-string minify failure simply skips that string (the
/// original is kept); the function never panics. Returns `NothingToStrip`
/// when no string was changed (request byte-identical), else
/// `Applied(delta)`.
#[must_use]
pub fn apply_json_minify(req: &mut ChatRequest) -> ReductionOutcome {
    let start = match mutable_suffix_start(req) {
        Some(start) => start,
        None => return ReductionOutcome::NoMutableTail,
    };

    let mut strings_minified = 0usize;
    let mut bytes_saved = 0usize;

    for message in req.messages.iter_mut().skip(start) {
        let MessageContent::Parts(parts) = &mut message.content else {
            continue;
        };
        for part in parts.iter_mut() {
            let ContentPart::Known(known) = part else {
                continue;
            };
            let target = match known {
                KnownContentPart::ToolResult { content, .. } => content,
                KnownContentPart::ToolUse { input, .. } => input,
                _ => continue,
            };
            if let Value::String(s) = target {
                if let Some(minified) = minify_json_whitespace(s) {
                    bytes_saved += s.len() - minified.len();
                    strings_minified += 1;
                    *target = Value::String(minified);
                }
            }
        }
    }

    if strings_minified == 0 {
        return ReductionOutcome::NothingToStrip;
    }

    ReductionOutcome::Applied(ReductionDelta {
        strings_minified,
        bytes_saved,
        est_tokens_saved: bytes_saved / BYTES_PER_TOKEN_ESTIMATE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_control::CacheControl;
    use crate::schema::{Message, Role};
    use serde_json::json;

    // --- minify_json_whitespace lexer ---

    #[test]
    fn minify_compacts_pretty_json_object_to_whitespace_free() {
        // Arrange
        let pretty = "{\n  \"a\": 1,\n  \"b\": 2\n}";

        // Act
        let out = minify_json_whitespace(pretty).unwrap();

        // Assert
        assert_eq!(out, "{\"a\":1,\"b\":2}");
    }

    #[test]
    fn minify_preserves_spaces_inside_string_value() {
        // Arrange: the inner double space is part of the string value.
        let pretty = "{ \"k\": \"a  b\" }";

        // Act
        let out = minify_json_whitespace(pretty).unwrap();

        // Assert: structural whitespace gone, inner "a  b" intact.
        assert_eq!(out, "{\"k\":\"a  b\"}");
    }

    #[test]
    fn minify_preserves_number_formatting_one_point_zero() {
        // Arrange: 1.0 must NOT normalize to 1 (no reserialize).
        let pretty = "{ \"x\": 1.0 }";

        // Act
        let out = minify_json_whitespace(pretty).unwrap();

        // Assert
        assert_eq!(out, "{\"x\":1.0}");
    }

    #[test]
    fn minify_preserves_duplicate_keys() {
        // Arrange: both `a` keys must survive byte-for-byte.
        let pretty = "{ \"a\": 1, \"a\": 2 }";

        // Act
        let out = minify_json_whitespace(pretty).unwrap();

        // Assert
        assert_eq!(out, "{\"a\":1,\"a\":2}");
    }

    #[test]
    fn minify_handles_escaped_quote_inside_string() {
        // Arrange: the escaped quotes must not end the string early.
        let pretty = "{ \"k\": \"he said \\\"hi\\\"\" }";

        // Act
        let out = minify_json_whitespace(pretty).unwrap();

        // Assert
        assert_eq!(out, "{\"k\":\"he said \\\"hi\\\"\"}");
    }

    #[test]
    fn minify_handles_escaped_backslash_then_quote() {
        // Arrange: value is a single backslash, then the string closes. A
        // naive escape walker could mistake the closing quote for escaped.
        let pretty = "{ \"k\": \"\\\\\" }";

        // Act
        let out = minify_json_whitespace(pretty).unwrap();

        // Assert
        assert_eq!(out, "{\"k\":\"\\\\\"}");
    }

    #[test]
    fn minify_returns_none_for_non_json_prose() {
        // Arrange: raw prose is not JSON; its whitespace is semantic.
        let prose = "hello world";

        // Act
        let out = minify_json_whitespace(prose);

        // Assert
        assert_eq!(out, None);
    }

    #[test]
    fn minify_returns_none_for_source_code_text() {
        // Arrange: source code is not a JSON document.
        let code = "fn main() {\n    println!(\"hi\");\n}";

        // Act
        let out = minify_json_whitespace(code);

        // Assert
        assert_eq!(out, None);
    }

    #[test]
    fn minify_returns_none_for_already_compact_json() {
        // Arrange: nothing to strip.
        let compact = "{\"a\":1,\"b\":[2,3]}";

        // Act
        let out = minify_json_whitespace(compact);

        // Assert
        assert_eq!(out, None);
    }

    #[test]
    fn minify_returns_none_for_malformed_json() {
        // Arrange: trailing comma is invalid JSON.
        let bad = "{ \"a\": 1, }";

        // Act
        let out = minify_json_whitespace(bad);

        // Assert
        assert_eq!(out, None);
    }

    #[test]
    fn minify_compacts_nested_array_of_objects() {
        // Arrange
        let pretty = "[\n  { \"id\": 1 },\n  { \"id\": 2 }\n]";

        // Act
        let out = minify_json_whitespace(pretty).unwrap();

        // Assert
        assert_eq!(out, "[{\"id\":1},{\"id\":2}]");
    }

    // --- losslessness property ---

    #[test]
    fn minify_is_lossless_across_several_pretty_documents() {
        // Arrange: each input parses to the same Value before and after.
        let pretties = [
            "{\n  \"name\": \"alice\",\n  \"age\": 30\n}",
            "[\n  1,\n  2,\n  3\n]",
            "{ \"nested\": { \"k\": [true, false, null] } }",
            "{ \"price\": 9.90, \"qty\": 100 }",
            "{ \"msg\": \"line1\\nline2\", \"tab\": \"a\\tb\" }",
        ];

        for pretty in pretties {
            // Act
            let minified = minify_json_whitespace(pretty).unwrap();

            // Assert
            let before: Value = serde_json::from_str(pretty).unwrap();
            let after: Value = serde_json::from_str(&minified).unwrap();
            assert_eq!(before, after, "lossless failure for: {pretty}");
        }
    }

    // --- apply_json_minify ---

    fn tool_result_msg(content: Value, cc: Option<CacheControl>) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content,
                    is_error: None,
                    cache_control: cc,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn text_msg(text: &str, cc: Option<CacheControl>) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: text.into(),
                cache_control: cc,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn apply_compacts_pretty_json_tool_result_in_mutable_tail() {
        // Arrange: a tool_result whose content is a pretty JSON STRING.
        let pretty = "{\n  \"rows\": [1, 2, 3]\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(pretty), None)],
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 1);
                assert!(delta.bytes_saved > 0, "expected bytes saved");
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        let MessageContent::Parts(parts) = &req.messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!("{\"rows\":[1,2,3]}"));
    }

    #[test]
    fn apply_compacts_pretty_json_tool_use_input_string() {
        // Arrange: tool_use.input as a pretty JSON STRING (not an object).
        let pretty = "{\n  \"query\": \"rust\"\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolUse {
                        id: "toolu_1".into(),
                        name: "search".into(),
                        input: json!(pretty),
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert!(matches!(outcome, ReductionOutcome::Applied(_)));
        let MessageContent::Parts(parts) = &req.messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolUse { input, .. }) = &parts[0] else {
            panic!("expected tool_use");
        };
        assert_eq!(input, &json!("{\"query\":\"rust\"}"));
    }

    #[test]
    fn apply_leaves_frozen_prefix_byte_identical() {
        // Arrange: message 0 carries a caller marker (frozen) with a pretty
        // JSON tool_result; message 1 is mutable plain text. The frozen
        // tool_result must NOT be compacted.
        let pretty = "{\n  \"frozen\": true\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                tool_result_msg(json!(pretty), Some(CacheControl::ephemeral_5m())),
                text_msg("hi", None),
            ],
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: the frozen tool_result string is unchanged.
        let MessageContent::Parts(parts) = &req.messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!(pretty));
        // The marker sits on message 0; start = 1; message 1 has no JSON
        // string to strip, so the whole request is byte-identical.
        assert_eq!(outcome, ReductionOutcome::NothingToStrip);
        let after = serde_json::to_value(&req).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn apply_frozen_prefix_bytes_unchanged_when_tail_is_compacted() {
        // Arrange: frozen message 0 holds a pretty JSON tool_result;
        // mutable message 1 ALSO holds a pretty JSON tool_result. Only the
        // tail must change; the serialized frozen prefix bytes must match.
        let frozen_pretty = "{\n  \"frozen\": [1, 2]\n}";
        let tail_pretty = "{\n  \"tail\": [3, 4]\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                tool_result_msg(json!(frozen_pretty), Some(CacheControl::ephemeral_5m())),
                tool_result_msg(json!(tail_pretty), None),
            ],
            ..Default::default()
        };
        let frozen_before = serde_json::to_value(&req.messages[0]).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: tail compacted, frozen prefix byte-identical.
        assert!(matches!(outcome, ReductionOutcome::Applied(_)));
        let frozen_after = serde_json::to_value(&req.messages[0]).unwrap();
        assert_eq!(frozen_before, frozen_after);
        let MessageContent::Parts(parts) = &req.messages[1].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!("{\"tail\":[3,4]}"));
    }

    #[test]
    fn apply_plain_text_tool_result_is_nothing_to_strip() {
        // Arrange: tool_result content is plain prose, not JSON.
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!("just some text output"), None)],
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert_eq!(outcome, ReductionOutcome::NothingToStrip);
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_structured_object_tool_result_is_nothing_to_strip() {
        // Arrange: content is a Value::Object (already whitespace-free on
        // the wire) -- only Value::String targets are minified.
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!({"rows": [1, 2, 3]}), None)],
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert_eq!(outcome, ReductionOutcome::NothingToStrip);
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_no_mutable_tail_when_last_marker_on_final_message() {
        // Arrange: the only caller marker sits on the final message, so
        // there is no mutable tail.
        let pretty = "{\n  \"a\": 1\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                text_msg("hello", None),
                tool_result_msg(json!(pretty), Some(CacheControl::ephemeral_5m())),
            ],
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: untouched.
        assert_eq!(outcome, ReductionOutcome::NoMutableTail);
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_empty_messages_is_no_mutable_tail() {
        // Arrange
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![],
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert_eq!(outcome, ReductionOutcome::NoMutableTail);
    }

    #[test]
    fn apply_top_level_cache_control_freezes_whole_prefix() {
        // Arrange: a top-level caller cache_control selects Anthropic
        // automatic caching, which freezes the ENTIRE prefix -- so even a
        // pretty JSON tool_result in the (otherwise mutable) last message
        // must NOT be touched.
        let pretty = "{\n  \"a\": 1\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(pretty), None)],
            cache_control: Some(CacheControl::ephemeral_5m()),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: untouched under a top-level breakpoint.
        assert_eq!(outcome, ReductionOutcome::NoMutableTail);
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_delta_counts_match_savings() {
        // Arrange: a single pretty document.
        let pretty = "{\n    \"k\": \"v\"\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(pretty), None)],
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: derive the compact length from the ACTUAL minified content
        // so the test stays self-consistent if `pretty` ever changes.
        let MessageContent::Parts(parts) = &req.messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
            panic!("expected tool_result");
        };
        let Value::String(compact) = content else {
            panic!("expected string content");
        };
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 1);
                assert_eq!(delta.bytes_saved, pretty.len() - compact.len());
                assert_eq!(delta.est_tokens_saved, delta.bytes_saved / 4);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }
}
