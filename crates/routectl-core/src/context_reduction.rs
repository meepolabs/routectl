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
//! Anthropic-shape `ToolResult.content` and `ToolUse.input` when they are
//! strings, plus the OpenAI-shape `function.arguments` string on each
//! `Message.tool_calls` entry -- not structured Values.
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

use std::sync::Arc;

use serde_json::Value;

use crate::cache_control::mutable_suffix_start;
use crate::content_part::{ContentPart, KnownContentPart};
use crate::schema::{ChatRequest, Message, MessageContent};

/// Divisor for the rough bytes-to-tokens estimate. Four bytes per token is
/// the conventional English-text heuristic; good enough for an
/// operator-facing "tokens saved" signal, not a billing figure.
const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

/// The classification ledger for one minify pass: how much was removed and
/// how every other candidate target was accounted for. Produced for BOTH the
/// applied and the nothing-to-strip outcomes, so a counter consumer never has
/// to reconstruct classifications from a bare outcome. A small owned record
/// the router maps to operator-facing strings.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReductionDelta {
    /// Number of JSON-valued strings that were compacted.
    pub strings_minified: usize,
    /// Number of candidate targets left untouched because they were not
    /// JSON text or were already whitespace-free (a permanent ceiling: no
    /// transform can shrink them).
    pub strings_skipped: usize,
    /// Number of targets that parsed as JSON but were declined by the
    /// re-parse equality guard. A FAIL-CLOSED INVARIANT ALARM, not a
    /// headroom signal: with the current minifier this count is structurally
    /// unreachable, so a nonzero value means the guard caught a rewrite that
    /// changed meaning -- a minifier defect to investigate. Headroom is
    /// `strings_skipped` plus the outcome histogram.
    pub strings_rejected: usize,
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
    /// caller `cache_control` marker, or there are no messages). No candidate
    /// target was ever examined, so there is no classification to report.
    NoMutableTail,
    /// A mutable tail exists but nothing was minified; the request is
    /// byte-identical to its input. Still carries the pass's classification
    /// ledger -- `strings_minified` and `bytes_saved` are zero, while
    /// `strings_skipped` / `strings_rejected` account for every target the
    /// tail did hold.
    NothingToStrip(ReductionDelta),
    /// At least one JSON-valued string was compacted.
    Applied(ReductionDelta),
}

/// Classification of a single candidate string. Distinguishing the two
/// no-op reasons is deliberate: `Skipped` is a permanent ceiling (nothing
/// could ever shrink these bytes) whereas `Rejected` is a fail-closed
/// invariant alarm -- the equality guard caught a rewrite that changed
/// meaning, which this minifier can never legitimately produce.
enum StringMinifyOutcome {
    /// Valid JSON, strictly shorter, and provably equal after re-parse.
    Compressed(String),
    /// Not JSON text, or already whitespace-free.
    Skipped,
    /// Parsed as JSON, but the re-parse equality guard declined the result.
    /// Structurally unreachable for input this lexer handles; reaching it
    /// means a minifier defect.
    Rejected,
}

/// Classify `s` and, when it can be safely compacted, carry the compacted
/// form. Sole owner of the three losslessness guards; every counting and
/// mutating path funnels through it so guard semantics cannot diverge.
fn classify_json_string(s: &str) -> StringMinifyOutcome {
    // Guard (a): non-JSON text has semantic whitespace (source code, logs,
    // prose) -- never touch it.
    let Ok(original) = serde_json::from_str::<Value>(s) else {
        return StringMinifyOutcome::Skipped;
    };

    let Some(minified) = strip_insignificant_whitespace(s) else {
        return StringMinifyOutcome::Skipped;
    };

    // Guard (c): nothing stripped (already compact) -- signal no-op.
    if minified.len() >= s.len() {
        return StringMinifyOutcome::Skipped;
    }

    // Guard (b): the result must parse AND equal the original parsed Value.
    accept_if_lossless(&original, minified)
}

/// Guard (b) in isolation: accept `candidate` as a lossless rewrite of
/// `original` only when it re-parses to an equal `Value`, else decline it.
///
/// Split out of `classify_json_string` so the rejection arm -- structurally
/// unreachable through public input, because the lexer only ever drops
/// insignificant whitespace -- is still exercisable with a deliberately
/// unequal pair.
fn accept_if_lossless(original: &Value, candidate: String) -> StringMinifyOutcome {
    let Ok(reparsed) = serde_json::from_str::<Value>(&candidate) else {
        return StringMinifyOutcome::Rejected;
    };
    if reparsed != *original {
        return StringMinifyOutcome::Rejected;
    }
    StringMinifyOutcome::Compressed(candidate)
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
    match classify_json_string(s) {
        StringMinifyOutcome::Compressed(minified) => Some(minified),
        StringMinifyOutcome::Skipped | StringMinifyOutcome::Rejected => None,
    }
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

/// Running tallies for one `apply_json_minify` pass.
#[derive(Default)]
struct DeltaCounts {
    strings_minified: usize,
    strings_skipped: usize,
    strings_rejected: usize,
    bytes_saved: usize,
}

impl DeltaCounts {
    /// Fold one classification into the tallies. `original_len` is the
    /// candidate's byte length, used only by the compressed arm. Borrows the
    /// outcome so the caller keeps ownership of a compacted form the mutating
    /// pass still needs.
    const fn count_outcome(&mut self, outcome: &StringMinifyOutcome, original_len: usize) {
        match outcome {
            StringMinifyOutcome::Compressed(minified) => {
                self.bytes_saved += original_len - minified.len();
                self.strings_minified += 1;
            }
            StringMinifyOutcome::Skipped => self.strings_skipped += 1,
            StringMinifyOutcome::Rejected => self.strings_rejected += 1,
        }
    }

    const fn into_delta(self) -> ReductionDelta {
        ReductionDelta {
            strings_minified: self.strings_minified,
            strings_skipped: self.strings_skipped,
            strings_rejected: self.strings_rejected,
            bytes_saved: self.bytes_saved,
            est_tokens_saved: self.bytes_saved / BYTES_PER_TOKEN_ESTIMATE,
        }
    }
}

/// The read-only scan's output: the pass tallies plus the compacted forms it
/// already computed, each tagged with its target's ordinal among the tail's
/// candidate targets.
///
/// Carrying the compacted forms is what keeps every target to a single JSON
/// parse: the mutating pass writes what the scan produced instead of
/// classifying the same string again.
#[derive(Default)]
struct MinifyPlan {
    counts: DeltaCounts,
    /// Ascending by ordinal, and only ever compressed classifications. Left
    /// empty by a pass that changes nothing, so the no-op path still
    /// allocates nothing.
    writes: Vec<(usize, StringMinifyOutcome)>,
    /// Candidate targets classified so far -- the ordinal the next one takes.
    seen: usize,
}

impl MinifyPlan {
    /// Classify one candidate target into the tallies and, when it can be
    /// compacted, record the compacted form for the mutating pass. Non-string
    /// targets are structured Values, already whitespace-free on the wire, so
    /// they count as skipped rather than rejected.
    fn classify(&mut self, target: &Value) {
        let ordinal = self.seen;
        self.seen += 1;

        let Value::String(s) = target else {
            self.counts.count_outcome(&StringMinifyOutcome::Skipped, 0);
            return;
        };
        let outcome = classify_json_string(s);
        self.counts.count_outcome(&outcome, s.len());
        if matches!(outcome, StringMinifyOutcome::Compressed(_)) {
            self.writes.push((ordinal, outcome));
        }
    }
}

/// Walks the tail's candidate targets in the SAME navigation order the scan
/// used, handing each the classification the scan recorded for it.
///
/// Positional by ordinal rather than by address, because the scan reads
/// through shared references and the mutation runs after `Arc::make_mut` has
/// possibly moved the buffer.
struct PlanApplier {
    remaining: std::iter::Peekable<std::vec::IntoIter<(usize, StringMinifyOutcome)>>,
    seen: usize,
}

impl PlanApplier {
    fn new(writes: Vec<(usize, StringMinifyOutcome)>) -> Self {
        Self {
            remaining: writes.into_iter().peekable(),
            seen: 0,
        }
    }

    /// Consume the next ordinal and, if the plan holds a classification for
    /// it, write it onto `target`. Ordinals with no plan entry -- non-string
    /// targets, permanent ceilings, guard rejections -- leave the target's
    /// bytes untouched.
    fn apply_next(&mut self, target: &mut Value) {
        let ordinal = self.seen;
        self.seen += 1;
        if let Some((_, outcome)) = self.remaining.next_if(|(i, _)| *i == ordinal) {
            apply_outcome(target, outcome);
        }
    }
}

/// Write a classification's compacted form back onto its target. Every other
/// classification -- including a guard rejection -- leaves the target's bytes
/// exactly as they were.
fn apply_outcome(target: &mut Value, outcome: StringMinifyOutcome) {
    if let StringMinifyOutcome::Compressed(minified) = outcome {
        *target = Value::String(minified);
    }
}

/// The `function.arguments` string of an OpenAI-shape tool_call, if the call
/// is shaped well enough to carry one. A malformed call or a non-function
/// type simply has no target; every navigation step is fallible.
fn tool_call_arguments(call: &Value) -> Option<&Value> {
    call.get("function")?.get("arguments")
}

/// The minify target of an Anthropic-shape content part, if it has one.
/// `ContentPart::Other`, thinking blocks, and text blocks have none.
const fn known_part_target(part: &ContentPart) -> Option<&Value> {
    let ContentPart::Known(known) = part else {
        return None;
    };
    match known {
        KnownContentPart::ToolResult { content, .. } => Some(content),
        KnownContentPart::ToolUse { input, .. } => Some(input),
        _ => None,
    }
}

/// Read-only scan of one message: classify every candidate target into
/// `plan` without touching the request.
///
/// This is the pass's ONLY tally site, for both outcomes. It must navigate
/// exactly the target set [`minify_message_targets`] mutates, in the same
/// order -- the counts would otherwise describe a different set of strings
/// than the one the mutation touched, and the plan's ordinals would address
/// the wrong targets.
fn scan_message_targets(message: &Message, plan: &mut MinifyPlan) {
    if let MessageContent::Parts(parts) = &message.content {
        for target in parts.iter().filter_map(known_part_target) {
            plan.classify(target);
        }
    }

    if let Some(tool_calls) = message.tool_calls.as_ref() {
        for target in tool_calls.iter().filter_map(tool_call_arguments) {
            plan.classify(target);
        }
    }
}

/// Apply the scanned plan to every target of one message in place.
///
/// The OpenAI Chat Completions shape carries assistant tool calls on the
/// separate `Message.tool_calls` field as untyped Values shaped
/// `{"id":..,"type":"function","function":{"name":..,"arguments":"<json>"}}`.
/// The `arguments` value is a STRING that may carry pretty-printed JSON
/// whitespace shipped every turn -- the same minify target as the Anthropic
/// content-part path.
fn minify_message_targets(message: &mut Message, applier: &mut PlanApplier) {
    if let MessageContent::Parts(parts) = &mut message.content {
        for part in parts.iter_mut() {
            let ContentPart::Known(known) = part else {
                continue;
            };
            let target = match known {
                KnownContentPart::ToolResult { content, .. } => content,
                KnownContentPart::ToolUse { input, .. } => input,
                _ => continue,
            };
            applier.apply_next(target);
        }
    }

    if let Some(tool_calls) = message.tool_calls.as_mut() {
        for call in tool_calls.iter_mut() {
            let Some(arguments) = call
                .get_mut("function")
                .and_then(|f| f.get_mut("arguments"))
            else {
                continue;
            };
            applier.apply_next(arguments);
        }
    }
}

/// Minify JSON-valued STRING content in the request's mutable message tail.
///
/// Computes `mutable_suffix_start(req)`; if `None`, returns
/// `NoMutableTail` and leaves `req` untouched. Otherwise, for each message
/// in `req.messages[start..]` it minifies every JSON-valued `Value::String`:
/// `ToolResult.content` and `ToolUse.input` on Anthropic-shape content parts,
/// and `function.arguments` on each OpenAI-shape `Message.tool_calls` entry.
/// Structured (non-string) Values, `ContentPart::Other`, thinking blocks, and
/// anything before `start` are never touched.
///
/// Plan-first: the tail is scanned read-only to build the classification
/// ledger AND the compacted form of every target that provably changes;
/// `Arc::make_mut` is reached for only when that ledger is non-empty -- so
/// the common nothing-to-strip request does not pay a message-buffer copy
/// against the CoW seam documented on [`ChatRequest::messages`]. Both
/// outcomes carry the ledger, so skip / reject counts survive the no-op
/// path, and no target is ever parsed twice.
///
/// Fail-closed: a per-string minify failure simply skips that string (the
/// original is kept); the function never panics. Returns
/// `NothingToStrip(delta)` when no string was changed (request
/// byte-identical), else `Applied(delta)`.
#[must_use]
pub fn apply_json_minify(req: &mut ChatRequest) -> ReductionOutcome {
    let start = match mutable_suffix_start(req) {
        Some(start) => start,
        None => return ReductionOutcome::NoMutableTail,
    };

    let mut plan = MinifyPlan::default();
    for message in req.messages.iter().skip(start) {
        scan_message_targets(message, &mut plan);
    }
    let delta = plan.counts.into_delta();

    if plan.writes.is_empty() {
        return ReductionOutcome::NothingToStrip(delta);
    }

    let mut applier = PlanApplier::new(plan.writes);
    for message in Arc::make_mut(&mut req.messages).iter_mut().skip(start) {
        minify_message_targets(message, &mut applier);
    }

    ReductionOutcome::Applied(delta)
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
                citations: None,
                cache_control: cc,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Build an OpenAI-shape assistant message carrying `tool_calls`. With
    /// `cc == None` the content is `Null` -- the real wire shape for a
    /// tool-call-only assistant turn. With `cc == Some` a Text part carries
    /// the caller cache_control marker so the message freezes (freeze tests).
    fn tool_calls_msg(arguments: Value, cc: Option<CacheControl>) -> Message {
        let content = match cc {
            Some(cc) => MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "calling".into(),
                citations: None,
                cache_control: Some(cc),
            })]),
            None => MessageContent::Null,
        };
        Message {
            refusal: None,
            role: Role::Assistant,
            content,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "search",
                    "arguments": arguments,
                },
            })]),
        }
    }

    #[test]
    fn apply_compacts_pretty_json_tool_result_in_mutable_tail() {
        // Arrange: a tool_result whose content is a pretty JSON STRING.
        let pretty = "{\n  \"rows\": [1, 2, 3]\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(pretty), None)].into(),
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
            }]
            .into(),
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
            ]
            .into(),
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
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
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
            ]
            .into(),
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
            messages: vec![tool_result_msg(json!("just some text output"), None)].into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_structured_object_tool_result_is_nothing_to_strip() {
        // Arrange: content is a Value::Object (already whitespace-free on
        // the wire) -- only Value::String targets are minified.
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!({"rows": [1, 2, 3]}), None)].into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
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
            ]
            .into(),
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
            messages: vec![].into(),
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
            messages: vec![tool_result_msg(json!(pretty), None)].into(),
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
            messages: vec![tool_result_msg(json!(pretty), None)].into(),
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

    // --- apply_json_minify: OpenAI-shape tool_calls arguments ---

    /// Read back `function.arguments` of the first tool_call on message 0.
    fn first_tool_call_arguments(req: &ChatRequest) -> &Value {
        req.messages[0]
            .tool_calls
            .as_ref()
            .expect("expected tool_calls")[0]
            .get("function")
            .expect("expected function")
            .get("arguments")
            .expect("expected arguments")
    }

    #[test]
    fn apply_compacts_pretty_tool_call_arguments_in_mutable_tail() {
        // Arrange: an assistant tool_call whose function.arguments is a pretty
        // JSON STRING in the mutable tail.
        let pretty = "{\n  \"query\": \"rust\",\n  \"limit\": 10\n}";
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![tool_calls_msg(json!(pretty), None)].into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 1);
                assert_eq!(
                    delta.bytes_saved,
                    pretty.len() - "{\"query\":\"rust\",\"limit\":10}".len()
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(
            first_tool_call_arguments(&req),
            &json!("{\"query\":\"rust\",\"limit\":10}")
        );
    }

    #[test]
    fn apply_leaves_frozen_tool_call_arguments_byte_identical() {
        // Arrange: message 0 carries a caller marker (frozen) and a pretty
        // tool_call arguments; message 1 is mutable plain text. The frozen
        // arguments must NOT be compacted.
        let pretty = "{\n  \"frozen\": true\n}";
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![
                tool_calls_msg(json!(pretty), Some(CacheControl::ephemeral_5m())),
                text_msg("hi", None),
            ]
            .into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: nothing in the tail to strip, frozen prefix byte-identical.
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
        assert_eq!(first_tool_call_arguments(&req), &json!(pretty));
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_frozen_tool_call_arguments_unchanged_when_tail_is_compacted() {
        // Arrange: frozen message 0 holds pretty tool_call arguments; mutable
        // message 1 ALSO holds pretty tool_call arguments. Only the tail must
        // change; the serialized frozen prefix bytes must match exactly. This
        // exercises the path the byte-identical guard alone cannot -- where the
        // loop DOES run and must still skip the frozen prefix.
        let frozen_pretty = "{\n  \"frozen\": [1, 2]\n}";
        let tail_pretty = "{\n  \"tail\": [3, 4]\n}";
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![
                tool_calls_msg(json!(frozen_pretty), Some(CacheControl::ephemeral_5m())),
                tool_calls_msg(json!(tail_pretty), None),
            ]
            .into(),
            ..Default::default()
        };
        let frozen_before = serde_json::to_value(&req.messages[0]).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: exactly the tail compacted, frozen prefix byte-identical.
        match outcome {
            ReductionOutcome::Applied(delta) => assert_eq!(delta.strings_minified, 1),
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(&req.messages[0]).unwrap(),
            frozen_before
        );
        assert_eq!(first_tool_call_arguments(&req), &json!(frozen_pretty));
        let tail_args = req.messages[1].tool_calls.as_ref().unwrap()[0]
            .get("function")
            .unwrap()
            .get("arguments")
            .unwrap();
        assert_eq!(tail_args, &json!("{\"tail\":[3,4]}"));
    }

    #[test]
    fn apply_top_level_cache_control_freezes_tool_call_arguments() {
        // Arrange: a top-level caller cache_control freezes the entire
        // prefix, so even a pretty tool_call arguments in the last message
        // must NOT be touched.
        let pretty = "{\n  \"a\": 1\n}";
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![tool_calls_msg(json!(pretty), None)].into(),
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
    fn apply_already_compact_tool_call_arguments_is_nothing_to_strip() {
        // Arrange: arguments is already whitespace-free JSON.
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![tool_calls_msg(json!("{\"q\":\"x\"}"), None)].into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_non_json_tool_call_arguments_is_nothing_to_strip() {
        // Arrange: arguments carries prose, not JSON -- whitespace is
        // semantic and must be preserved.
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![tool_calls_msg(json!("just some text"), None)].into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_tool_call_missing_function_key_is_skipped_safely() {
        // Arrange: a malformed tool_call with no `function` key, plus one
        // with `function` but no `arguments`. Neither must panic; nothing is
        // minified.
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    json!({"id": "call_1", "type": "function"}),
                    json!({"id": "call_2", "function": {"name": "noargs"}}),
                ]),
            }]
            .into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_non_string_tool_call_arguments_is_skipped_safely() {
        // Arrange: `arguments` is an object (not a string) and a number on a
        // second call -- only Value::String targets are minified.
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    json!({"function": {"name": "f", "arguments": {"q": "x"}}}),
                    json!({"function": {"name": "g", "arguments": 42}}),
                ]),
            }]
            .into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn apply_compacts_both_tool_use_part_and_tool_calls() {
        // Arrange: one mutable message carries BOTH an Anthropic ToolUse
        // content part AND an OpenAI tool_calls entry, both pretty.
        let tool_use_pretty = "{\n  \"input\": 1\n}";
        let tool_call_pretty = "{\n  \"args\": 2\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolUse {
                        id: "toolu_1".into(),
                        name: "search".into(),
                        input: json!(tool_use_pretty),
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": tool_call_pretty},
                })]),
            }]
            .into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: both compacted; counts reflect both.
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 2);
                assert_eq!(
                    delta.bytes_saved,
                    (tool_use_pretty.len() - "{\"input\":1}".len())
                        + (tool_call_pretty.len() - "{\"args\":2}".len())
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        let MessageContent::Parts(parts) = &req.messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolUse { input, .. }) = &parts[0] else {
            panic!("expected tool_use");
        };
        assert_eq!(input, &json!("{\"input\":1}"));
        assert_eq!(first_tool_call_arguments(&req), &json!("{\"args\":2}"));
    }

    // --- scan-to-mutation plan alignment ---

    #[test]
    fn applies_each_compacted_form_to_its_own_target_across_a_mixed_tail() {
        // Arrange: the plan addresses targets by ordinal, so a tail that
        // interleaves compactable and non-compactable targets -- across the
        // parts / tool_calls navigation boundary AND across messages -- is
        // what catches an off-by-one. Each compactable target carries a
        // DISTINCT payload, so a misapplied write shows up as the wrong
        // document on the wrong target rather than merely the wrong length.
        let part_pretty = "{\n  \"part\": 1\n}";
        let call_pretty = "{\n  \"call\": 2\n}";
        let later_pretty = "{\n  \"later\": 3\n}";
        let prose = "not json at all";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::ToolResult {
                            tool_use_id: "toolu_skip".into(),
                            content: json!(prose),
                            is_error: None,
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::ToolUse {
                            id: "toolu_1".into(),
                            name: "search".into(),
                            input: json!(part_pretty),
                            cache_control: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![
                        json!({"function": {"name": "structured", "arguments": {"q": "x"}}}),
                        json!({"function": {"name": "pretty", "arguments": call_pretty}}),
                    ]),
                },
                tool_result_msg(json!(later_pretty), None),
            ]
            .into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 3);
                assert_eq!(delta.strings_skipped, 2);
                assert_eq!(delta.strings_rejected, 0);
            }
            other => panic!("expected Applied, got {other:?}"),
        }

        let MessageContent::Parts(parts) = &req.messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!(prose), "skipped target must keep its bytes");
        let ContentPart::Known(KnownContentPart::ToolUse { input, .. }) = &parts[1] else {
            panic!("expected tool_use");
        };
        assert_eq!(input, &json!("{\"part\":1}"));

        let calls = req.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(
            calls[0].get("function").unwrap().get("arguments").unwrap(),
            &json!({"q": "x"}),
            "structured target must stay structured"
        );
        assert_eq!(
            calls[1].get("function").unwrap().get("arguments").unwrap(),
            &json!("{\"call\":2}")
        );

        let MessageContent::Parts(later) = &req.messages[1].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &later[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!("{\"later\":3}"));
    }

    #[test]
    fn frozen_prefix_targets_never_shift_the_plan_ordinals() {
        // Arrange: the scan starts at `start`, so a frozen prefix holding its
        // own compactable target contributes no ordinal. Were the mutation to
        // count from message 0 instead, the tail's write would land on the
        // wrong target -- or on the frozen bytes.
        let frozen_pretty = "{\n  \"frozen\": 1\n}";
        let tail_pretty = "{\n  \"tail\": 2\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                tool_result_msg(json!(frozen_pretty), Some(CacheControl::ephemeral_5m())),
                tool_result_msg(json!(tail_pretty), None),
            ]
            .into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        match outcome {
            ReductionOutcome::Applied(delta) => assert_eq!(delta.strings_minified, 1),
            other => panic!("expected Applied, got {other:?}"),
        }
        let MessageContent::Parts(frozen) = &req.messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &frozen[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!(frozen_pretty));
        let MessageContent::Parts(tail) = &req.messages[1].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &tail[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!("{\"tail\":2}"));
    }

    #[test]
    fn a_plan_with_no_writes_leaves_every_target_untouched() {
        // Arrange: the applier is the mutation's sole write path, so an empty
        // plan must be a no-op even when driven over targets that WOULD
        // compact -- the property that keeps the mutation from re-deriving
        // anything the scan did not sanction.
        let pretty = "{ \"a\": 1 }";
        let mut target = Value::String(pretty.to_string());
        let mut applier = PlanApplier::new(vec![]);

        // Act
        applier.apply_next(&mut target);

        // Assert
        assert_eq!(target, json!(pretty));
    }

    // --- copy-on-write: the no-op pass must not clone the message buffer ---

    #[test]
    fn apply_no_op_preserves_the_shared_message_buffer_allocation() {
        // Arrange: a shared-refcount buffer (the dispatch path clones the
        // request per fallback entry) whose mutable tail has nothing to
        // strip -- the pre-scan must short-circuit before `Arc::make_mut`.
        let messages: Arc<[Message]> =
            vec![tool_result_msg(json!("just some text output"), None)].into();
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: Arc::clone(&messages),
            ..Default::default()
        };
        assert_eq!(Arc::strong_count(&messages), 2);

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: same allocation, still shared, no deep copy paid -- and the
        // classification survives the short-circuit (the counters must be
        // readable without the mutating pass ever running).
        let ReductionOutcome::NothingToStrip(delta) = &outcome else {
            panic!("expected NothingToStrip, got {outcome:?}");
        };
        assert_eq!(delta.strings_minified, 0);
        assert_eq!(delta.bytes_saved, 0);
        assert_eq!(delta.strings_skipped, 1);
        assert_eq!(delta.strings_rejected, 0);
        assert!(Arc::ptr_eq(&messages, &req.messages));
        assert_eq!(Arc::strong_count(&messages), 2);
    }

    #[test]
    fn apply_no_op_preserves_shared_buffer_when_tail_is_already_compact() {
        // Arrange: valid JSON, already whitespace-free -- the other no-op
        // reason. Must also short-circuit.
        let messages: Arc<[Message]> =
            vec![tool_result_msg(json!("{\"a\":1,\"b\":[2,3]}"), None)].into();
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: Arc::clone(&messages),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
        assert!(Arc::ptr_eq(&messages, &req.messages));
    }

    #[test]
    fn apply_over_shared_buffer_leaves_other_clone_pristine_when_compacting() {
        // Arrange: the compacting path DOES copy, and the other holder of the
        // buffer must keep its original bytes (the CoW contract).
        let pretty = "{\n  \"rows\": [1, 2, 3]\n}";
        let messages: Arc<[Message]> = vec![tool_result_msg(json!(pretty), None)].into();
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: Arc::clone(&messages),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: request compacted, the shared original untouched.
        assert!(matches!(outcome, ReductionOutcome::Applied(_)));
        assert!(!Arc::ptr_eq(&messages, &req.messages));
        let MessageContent::Parts(parts) = &messages[0].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!(pretty));
    }

    // --- skip vs reject classification ---

    #[test]
    fn nothing_to_strip_still_reports_every_target_classification() {
        // Arrange: a tail with NO compactable target at all -- a structured
        // Value, plain prose, and already-compact JSON. The counters are the
        // per-dispatch accounting, so they must be reported for this dispatch
        // too, not discarded because no mutation happened.
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                tool_result_msg(json!({"structured": true}), None),
                tool_result_msg(json!("plain prose output"), None),
                tool_result_msg(json!("{\"already\":\"compact\"}"), None),
            ]
            .into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: all three accounted for as skipped, bytes untouched.
        let ReductionOutcome::NothingToStrip(delta) = &outcome else {
            panic!("expected NothingToStrip, got {outcome:?}");
        };
        assert_eq!(delta.strings_minified, 0);
        assert_eq!(delta.bytes_saved, 0);
        assert_eq!(delta.est_tokens_saved, 0);
        assert_eq!(delta.strings_skipped, 3);
        assert_eq!(delta.strings_rejected, 0);
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn nothing_to_strip_counts_tool_call_arguments_targets_too() {
        // Arrange: the OpenAI-shape target set must be classified on the
        // no-op path as well, not just the Anthropic content parts.
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![tool_calls_msg(json!("{\"q\":\"x\"}"), None)].into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        let ReductionOutcome::NothingToStrip(delta) = &outcome else {
            panic!("expected NothingToStrip, got {outcome:?}");
        };
        assert_eq!(delta.strings_skipped, 1);
        assert_eq!(delta.strings_minified, 0);
    }

    #[test]
    fn no_mutable_tail_examines_no_targets() {
        // Arrange: the frozen-prefix outcome carries no ledger by design --
        // no candidate target was ever examined, so a zeroed delta would be
        // indistinguishable from "a tail with nothing in it".
        let pretty = "{\n  \"a\": 1\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(pretty), None)].into(),
            cache_control: Some(CacheControl::ephemeral_5m()),
            ..Default::default()
        };

        // Act / Assert
        assert_eq!(apply_json_minify(&mut req), ReductionOutcome::NoMutableTail);
    }

    #[test]
    fn nothing_to_strip_never_counts_frozen_prefix_targets() {
        // Arrange: message 0 is frozen by a caller marker and holds a pretty
        // (compactable) tool_result; message 1 is the mutable tail and holds
        // prose. Only the tail is a candidate, so the ledger must show
        // exactly one skipped target -- counting the frozen pretty one would
        // both overstate the skip count and imply the minifier looked at
        // bytes it must never touch.
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                tool_result_msg(
                    json!("{\n  \"frozen\": 1\n}"),
                    Some(CacheControl::ephemeral_5m()),
                ),
                tool_result_msg(json!("plain prose"), None),
            ]
            .into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        let ReductionOutcome::NothingToStrip(delta) = &outcome else {
            panic!("expected NothingToStrip, got {outcome:?}");
        };
        assert_eq!(delta.strings_skipped, 1);
        assert_eq!(delta.strings_minified, 0);
    }

    #[test]
    fn classify_skips_non_json_prose() {
        // Arrange / Act / Assert: whitespace is semantic, permanent ceiling.
        assert!(matches!(
            classify_json_string("hello   world"),
            StringMinifyOutcome::Skipped
        ));
    }

    #[test]
    fn classify_skips_already_compact_json() {
        // Arrange / Act / Assert
        assert!(matches!(
            classify_json_string("{\"a\":1}"),
            StringMinifyOutcome::Skipped
        ));
    }

    #[test]
    fn classify_compresses_pretty_json() {
        // Arrange / Act / Assert
        assert!(matches!(
            classify_json_string("{ \"a\": 1 }"),
            StringMinifyOutcome::Compressed(_)
        ));
    }

    #[test]
    fn apply_counts_non_string_target_as_skipped_not_rejected() {
        // Arrange: one compactable string (so the pass runs) plus a
        // structured Value target and a plain-prose target, both of which
        // are permanent ceilings, not invariant alarms.
        let pretty = "{\n  \"rows\": [1, 2]\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                tool_result_msg(json!(pretty), None),
                tool_result_msg(json!({"structured": true}), None),
                tool_result_msg(json!("plain prose output"), None),
            ]
            .into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 1);
                assert_eq!(delta.strings_skipped, 2);
                assert_eq!(delta.strings_rejected, 0);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn apply_counts_already_compact_sibling_as_skipped() {
        // Arrange: a compactable target beside an already-compact one.
        let pretty = "{\n  \"a\": 1\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                tool_result_msg(json!(pretty), None),
                tool_result_msg(json!("{\"b\":2}"), None),
            ]
            .into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 1);
                assert_eq!(delta.strings_skipped, 1);
                assert_eq!(delta.strings_rejected, 0);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn apply_est_tokens_saved_stays_bytes_over_four() {
        // Arrange: est_tokens_saved semantics are unchanged by the widening.
        let pretty = "{\n    \"key\": \"value\",\n    \"n\": 1\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(pretty), None)].into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.est_tokens_saved, delta.bytes_saved / 4);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    // --- guard rejection preserves bytes ---

    #[test]
    fn guard_declined_string_leaves_request_bytes_identical() {
        // Arrange: an unterminated string literal. The document is not valid
        // JSON, so guard (a) declines it before the lexer runs -- had it
        // reached the lexer, the structural whitespace would have been
        // dropped and the shorter result would still be invalid, which is
        // what guard (b) exists to catch.
        let hostile = "{  \"k\": \"unterminated";
        assert!(matches!(
            classify_json_string(hostile),
            StringMinifyOutcome::Skipped
        ));
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(hostile), None)].into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: bytes preserved exactly, nothing applied.
        assert!(matches!(outcome, ReductionOutcome::NothingToStrip(_)));
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn guard_declined_string_beside_a_compacted_one_keeps_its_bytes() {
        // Arrange: the declining path must preserve its own bytes even when
        // the mutating pass DOES run for a sibling target.
        let hostile = "{  \"k\": \"unterminated";
        let pretty = "{\n  \"ok\": 1\n}";
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                tool_result_msg(json!(pretty), None),
                tool_result_msg(json!(hostile), None),
            ]
            .into(),
            ..Default::default()
        };

        // Act
        let outcome = apply_json_minify(&mut req);

        // Assert: exactly one compacted; the declined target byte-identical.
        match outcome {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 1);
                assert_eq!(delta.strings_skipped, 1);
                assert_eq!(delta.strings_rejected, 0);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        let MessageContent::Parts(parts) = &req.messages[1].content else {
            panic!("expected parts");
        };
        let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
            panic!("expected tool_result");
        };
        assert_eq!(content, &json!(hostile), "declined bytes must survive");
    }

    #[test]
    fn valid_json_is_never_rejected_by_the_equality_guard() {
        // Arrange: `strings_rejected` is a fail-closed invariant alarm, so its
        // meaning depends on guard (b) never firing for input this lexer
        // handles -- it drops only ASCII whitespace outside string literals
        // and is escape-aware, so the compacted form always re-parses equal. A
        // nonzero `strings_rejected` in production would therefore mean a
        // real lexer defect, not ordinary traffic.
        let corpus = [
            "{ \"a\": 1.0, \"b\": [true, false, null] }",
            "{ \"a\": 1, \"a\": 2 }",
            "{ \"k\": \"he said \\\"hi\\\"\" }",
            "{ \"k\": \"\\\\\" }",
            "{ \"k\": \"a  b\\n\\tc\" }",
            "[\n  { \"id\": 1 },\n  { \"id\": 2 }\n]",
            "{ \"unicode\": \"caf\\u00e9 \\ud83d\\ude00\" }",
            "{ \"big\": 12345678901234567890, \"exp\": 1e10 }",
        ];

        for doc in corpus {
            // Act
            let outcome = classify_json_string(doc);

            // Assert
            assert!(
                !matches!(outcome, StringMinifyOutcome::Rejected),
                "equality guard unexpectedly declined valid JSON: {doc}"
            );
        }
    }

    #[test]
    fn equality_guard_rejects_an_unequal_candidate_and_preserves_the_target_bytes() {
        // Arrange: the guard's rejection arm cannot be reached through public
        // input (the lexer only ever drops insignificant whitespace), so drive
        // it directly with a candidate that parses but means something else.
        let original: Value = serde_json::from_str("{\"a\":1}").unwrap();
        let mut target = Value::String("{\"a\":1}".to_string());
        let before = target.clone();

        // Act
        let outcome = accept_if_lossless(&original, "{\"a\":2}".to_string());
        let rejected = matches!(outcome, StringMinifyOutcome::Rejected);
        apply_outcome(&mut target, outcome);

        // Assert: declined, and the target's bytes are untouched.
        assert!(rejected, "an unequal candidate must be declined");
        assert_eq!(target, before, "a rejected candidate must not be written");
    }

    #[test]
    fn equality_guard_rejects_an_unparseable_candidate() {
        // Arrange / Act / Assert: a candidate that does not re-parse at all is
        // the guard's other rejection arm.
        let original: Value = serde_json::from_str("{\"a\":1}").unwrap();
        assert!(matches!(
            accept_if_lossless(&original, "{\"a\":".to_string()),
            StringMinifyOutcome::Rejected
        ));
    }

    #[test]
    fn a_rejected_classification_accumulates_into_strings_rejected() {
        // Arrange: the delta path must route a rejection to its own counter,
        // leaving bytes_saved and the other tallies at zero.
        let original: Value = serde_json::from_str("{\"a\":1}").unwrap();
        let mut counts = DeltaCounts::default();

        // Act
        counts.count_outcome(&accept_if_lossless(&original, "{\"a\":2}".to_string()), 7);
        let delta = counts.into_delta();

        // Assert
        assert_eq!(delta.strings_rejected, 1);
        assert_eq!(delta.strings_minified, 0);
        assert_eq!(delta.strings_skipped, 0);
        assert_eq!(delta.bytes_saved, 0);
        assert_eq!(delta.est_tokens_saved, 0);
    }
}
