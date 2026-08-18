//! Canonical `messages[]` -> Anthropic wire `messages[]` translation.
//!
//! Per-role dispatch (`translate_messages`): User content rides through
//! `translate_simple_content`; Assistant turns route through
//! `build_assistant_content` (which threads `reasoning_details` back as
//! Thinking / RedactedThinking blocks and re-emits OpenAI-shape
//! `tool_calls` as ToolUse blocks for multi-turn replay); Role::System
//! is dropped here (already lifted into `req.system` by `system.rs`);
//! Role::Tool becomes a synthesized user-role message carrying a
//! tool_result block, and a run of consecutive Role::Tool turns folds
//! into ONE such message with one block per turn.
//!
//! `normalize_replay_invariants` applies two outgoing invariants before
//! translation: a hard reject for tool_result messages missing a
//! tool_call_id, and (gated on `history_reasoning`) a strip of unsigned
//! Thinking blocks that real Anthropic would 400 on replay. Forward-
//! compat: `ContentPart::Other` passes through verbatim via
//! `ContentBlock::Other`.

use std::borrow::Cow;
use std::collections::HashSet;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use serde_json::{Value, json};

use routectl_core::{
    ChatRequest, ContentPart, CoreHistoryReasoning, Error, KnownContentPart, Message,
    MessageContent, ReasoningDetail, ReasoningDetailKind, Result, Role, sanitize_for_log,
};

use crate::bounded_diagnostics::BoundedLogSample;

use super::envelope_policy::EnvelopeUnwrapTally;
#[cfg(test)]
use super::envelope_policy::passthrough_tally;
use super::parts::{parse_file_document_source, parse_image_url_source, strip_text_after_tool_use};
use super::types::{AnthropicContent, AnthropicMessage, AnthropicRole, ContentBlock};

/// Walk the canonical `ChatRequest` messages and apply two outgoing
/// replay invariants. `history_reasoning` gates ONLY the second
/// (unsigned-thinking strip); the tool_call_id reject is unconditional.
///
/// - Hard-reject (Err) any tool_result message (`Role::Tool`) that
///   lacks a `tool_call_id`. This runs REGARDLESS of `history_reasoning`
///   -- it is a separate correctness invariant, not part of the
///   thinking-strip. Anthropic 400s on such a body and the upstream
///   error doesn't name the bad message; surfacing it locally gives
///   operators a precise field to fix.
/// - STRIP any `Thinking` content block whose `signature` is missing,
///   empty, or not Claude-shaped (a foreign signature minted by another
///   provider on a cross-provider turn) from each message's `Parts`
///   content -- UNLESS `history_reasoning` is `Preserve`. Cross-provider
///   fallback (a prior turn handled by deepseek which signs with its own
///   uuid format, then the next turn falls back to Anthropic) and SDKs
///   that fail to round-trip the signature field would otherwise 400 real
///   Anthropic with a confusing upstream error. Strip drops just the
///   offending block; Claude-signed thinking blocks pass through unchanged
///   and so does every other block type.
///
///   `Preserve` skips the strip entirely: deepseek v4's `/anthropic`
///   endpoint (provider kind anthropic-api) emits unsigned thinking AND
///   400s the next turn unless that thinking is echoed back verbatim
///   (`The content[].thinking in the thinking mode must be passed back
///   to the API.`). `Auto` and the unset/None default both strip --
///   there is no dialect-default concept for this egress, so Auto means
///   strip, which is real-Anthropic-safe. Only explicit `Preserve`
///   changes behavior.
/// - When stripping leaves a message with no content blocks AND no
///   `reasoning_details` AND no `tool_calls`, drop the whole message.
///   Anthropic's wire spec rejects `content: []`; emitting the empty
///   message would just trade one 400 for another. The
///   `build_assistant_content` path still fills the wire content array
///   from `reasoning_details` / `tool_calls` when those are present,
///   so we keep the message in that case. Preserve never strips, so
///   this drop path does not run under Preserve.
///
/// One structured WARN fires per request when stripping occurs,
/// carrying the provider id, the exact count of dropped blocks, the
/// exact count of affected messages, and a bounded sample of the
/// affected message indices flagged when it is only a sample. Block
/// content is never logged (could be reasoning over sensitive data).
/// Preserve strips nothing, so the WARN does not fire under Preserve.
///
/// Returns `Cow::Borrowed(&req.messages)` on the no-strip path (Preserve,
/// or Strip/Auto with nothing to strip) so unmodified requests don't pay
/// a clone.
pub(super) fn normalize_replay_invariants<'a>(
    id: &str,
    req: &'a ChatRequest,
    history_reasoning: CoreHistoryReasoning,
) -> Result<Cow<'a, [Message]>> {
    // Tool-result tool_call_id check stays a hard fail REGARDLESS of
    // history_reasoning -- it is a separate correctness invariant, not
    // part of the thinking-strip. Anthropic 400s a multi-turn body with
    // tool_use ids that lack matching tool_results.
    for (i, msg) in req.messages.iter().enumerate() {
        if matches!(msg.role, Role::Tool) && msg.tool_call_id.as_deref().unwrap_or("").is_empty() {
            return Err(Error::normalize_request(
                id,
                format!(
                    "messages[{i}] is a tool_result (Role::Tool) without tool_call_id; \
                     Anthropic requires the id of the tool_use this is answering",
                ),
            ));
        }
    }

    // Preserve: skip the unsigned-thinking strip and pass the messages
    // through unchanged. deepseek v4's `/anthropic` endpoint emits
    // unsigned thinking AND 400s the next turn unless it is echoed back
    // verbatim, so stripping would break every multi-turn replay. The
    // tool_call_id check above is validation-only (no mutation), so
    // Preserve can borrow; nothing is stripped, so no message-emptying
    // and no WARN.
    match history_reasoning {
        CoreHistoryReasoning::Preserve => {
            return Ok(Cow::Borrowed(&req.messages));
        }
        CoreHistoryReasoning::Auto | CoreHistoryReasoning::Strip => {}
    }

    // Strip / Auto pre-scan: do we need to strip anything? No -> return
    // Borrowed (no clone). Yes -> rebuild on the second pass.
    let needs_strip = req.messages.iter().any(message_has_unsigned_thinking);
    if !needs_strip {
        return Ok(Cow::Borrowed(&req.messages));
    }

    // Rebuild path: walk every message; for Parts, retain non-unsigned-
    // thinking blocks. Drop the message wholesale when stripping leaves
    // nothing the wire can serialize.
    let mut out: Vec<Message> = Vec::with_capacity(req.messages.len());
    let mut dropped_blocks: usize = 0;
    // The samples bound what reaches the log record; the counters beside
    // them stay exact. A sample's stored length is NOT the magnitude once
    // it caps, so no logged count is ever read back off a sample.
    let mut affected_message_count: usize = 0;
    let mut affected_messages: BoundedLogSample<usize> = BoundedLogSample::new();
    let mut dropped_turn_count: usize = 0;
    let mut dropped_turn_indices: BoundedLogSample<usize> = BoundedLogSample::new();
    for (i, msg) in req.messages.iter().enumerate() {
        let MessageContent::Parts(parts) = &msg.content else {
            // Text / Null content cannot carry a Thinking block.
            out.push(msg.clone());
            continue;
        };
        let original_len = parts.len();
        let kept: Vec<ContentPart> = parts
            .iter()
            .filter(|p| !is_unsigned_thinking_part(p))
            .cloned()
            .collect();
        let stripped_here = original_len.saturating_sub(kept.len());
        if stripped_here > 0 {
            dropped_blocks += stripped_here;
            affected_message_count += 1;
            affected_messages.push(i);
        }
        let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
        let has_emittable_reasoning = message_has_emittable_reasoning(msg);
        if kept.is_empty() && !has_tool_calls && !has_emittable_reasoning {
            // Stripping emptied this message and there is no other content
            // the wire can serialize: no tool_calls, and every
            // reasoning_detail is non-emittable (unsigned or foreign
            // format, so `emit_reasoning_blocks` would skip them all).
            // Anthropic's wire spec rejects content: [] for both roles;
            // keeping the message here would just trade the strip 400 for
            // an empty-array 400 in `build_assistant_content`. A message
            // with tool_calls is NEVER dropped -- that would orphan the
            // next tool_result turn.
            dropped_turn_count += 1;
            dropped_turn_indices.push(i);
            continue;
        }
        out.push(Message {
            refusal: None,
            role: msg.role.clone(),
            content: MessageContent::Parts(kept),
            reasoning: msg.reasoning.clone(),
            reasoning_details: msg.reasoning_details.clone(),
            name: msg.name.clone(),
            tool_call_id: msg.tool_call_id.clone(),
            tool_calls: msg.tool_calls.clone(),
        });
    }

    // One structured WARN per request. Block content stays OUT of the
    // log line (could be reasoning over sensitive data); only counts
    // and indices reach the operator. Provider id is always present
    // so an operator triaging a noisy upstream can grep by it.
    tracing::warn!(
        provider = id,
        dropped_blocks,
        affected_messages_count = affected_message_count,
        affected_messages = ?affected_messages.items(),
        affected_messages_truncated = affected_messages.truncated(),
        "stripping unsigned thinking blocks from outgoing request: \
         Anthropic requires a signature on replayed Thinking blocks. \
         Cross-provider fallback or SDKs that fail to round-trip the \
         signature field would otherwise 400 the request. Routectl \
         drops just the unsigned blocks; signed thinking blocks and \
         other content pass through unchanged."
    );

    // Separate aggregated WARN when stripping emptied a whole assistant
    // turn (the per-block WARN above only covers individual dropped
    // blocks). Distinct field/message so operators can tell "some blocks
    // stripped" from "an entire turn omitted". No content is logged.
    if dropped_turn_count > 0 {
        tracing::warn!(
            provider = id,
            dropped_turns = dropped_turn_count,
            dropped_message_indices = ?dropped_turn_indices.items(),
            dropped_message_indices_truncated = dropped_turn_indices.truncated(),
            "dropping assistant turn(s) from outgoing request: stripping \
             left no wire-serializable content (no Anthropic-emittable \
             reasoning and no tool_calls). Emitting content: [] would 400 \
             upstream, so the turn is omitted; tool_result correlation is \
             unaffected because a dropped turn carries no tool_use."
        );
    }

    Ok(Cow::Owned(out))
}

/// True iff `p` is a `Thinking` block whose `signature` cannot ride a
/// real-Anthropic replay: missing, empty, OR present-but-not Claude-
/// shaped (a foreign signature minted by gpt/gemini on a cross-provider
/// turn). Anthropic 400s on every one of these. Pulled out so the
/// pre-scan and the rebuild walk share a single predicate.
fn is_unsigned_thinking_part(p: &ContentPart) -> bool {
    matches!(
        p,
        ContentPart::Known(KnownContentPart::Thinking { signature, .. })
            if !is_claude_shaped_signature(signature.as_deref().unwrap_or(""))
    )
}

/// True iff `sig` has the SHAPE of a genuine Claude thinking-block
/// signature. Real Anthropic accepts only its own signatures on replay;
/// a foreign signature (e.g. a gpt/gemini uuid) 400s the request, so any
/// signature that fails this shape check is stripped upstream.
///
/// Claude signatures are base64. The first char encodes layer depth:
///   - `E`: single-layer base64; decoded payload's first byte is 0x12.
///   - `R`: double-layer; decode once -> the inner string is itself an
///     E-prefixed single-layer Claude signature.
///
/// A `<word>#` cache prefix may precede the E/R marker; strip one such
/// leading segment before inspecting. Anything else -- other prefix,
/// malformed base64, decoded byte0 != 0x12, empty -- is not Claude-shaped.
fn is_claude_shaped_signature(sig: &str) -> bool {
    // A historical cache key (`modelGroup#<sig>`) may prefix the raw
    // signature; inspect only the segment after the first `#`.
    let sig = sig.split_once('#').map_or(sig, |(_, rest)| rest);
    match sig.as_bytes().first() {
        Some(b'E') => is_e_layer_claude_signature(sig),
        Some(b'R') => {
            // Decode the outer layer; the inner bytes must themselves be
            // a UTF-8 E-prefixed single-layer Claude signature.
            let Ok(inner) = B64_STANDARD.decode(sig) else {
                return false;
            };
            match std::str::from_utf8(&inner) {
                Ok(inner_sig) => is_e_layer_claude_signature(inner_sig),
                Err(_) => false,
            }
        }
        _ => false,
    }
}

/// True iff `sig` is an `E`-prefixed single-layer Claude signature: valid
/// base64 whose decoded payload's first byte is 0x12. Non-panicking; any
/// decode failure or non-0x12 leading byte returns false.
fn is_e_layer_claude_signature(sig: &str) -> bool {
    if sig.as_bytes().first() != Some(&b'E') {
        return false;
    }
    match B64_STANDARD.decode(sig) {
        Ok(bytes) => bytes.first() == Some(&0x12),
        Err(_) => false,
    }
}

/// True iff any `Parts` content block on `msg` is an unsigned
/// `Thinking` block.
fn message_has_unsigned_thinking(msg: &Message) -> bool {
    if let MessageContent::Parts(parts) = &msg.content {
        parts.iter().any(is_unsigned_thinking_part)
    } else {
        false
    }
}

/// True iff `detail` will produce an Anthropic content block in
/// `emit_reasoning_blocks`. A `Text` detail needs the
/// `anthropic-claude-v1` format AND a non-empty signature (Anthropic
/// 400s a Thinking block with no signature). An `Encrypted` detail needs
/// only that format (RedactedThinking carries no signature). A `Summary`
/// detail is never an Anthropic block.
///
/// Shared by the `normalize_replay_invariants` keep-decision and the
/// `emit_reasoning_blocks` gate so the two cannot drift: a turn is kept
/// for its reasoning ONLY when that reasoning will actually emit at least
/// one block. Without this, a turn kept because it merely HAS
/// reasoning_details can emit zero blocks (all unsigned or foreign
/// format) and reach the wire as an invalid `content: []`.
fn is_anthropic_emittable_detail(detail: &ReasoningDetail) -> bool {
    match detail.kind {
        ReasoningDetailKind::Text => {
            detail.format.as_deref() == Some(super::ANTHROPIC_FORMAT)
                && detail
                    .payload
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty())
        }
        ReasoningDetailKind::Encrypted => detail.format.as_deref() == Some(super::ANTHROPIC_FORMAT),
        ReasoningDetailKind::Summary => false,
    }
}

/// True iff `msg` carries at least one reasoning detail that
/// `emit_reasoning_blocks` will turn into an Anthropic content block.
fn message_has_emittable_reasoning(msg: &Message) -> bool {
    msg.reasoning_details
        .iter()
        .any(is_anthropic_emittable_detail)
}

fn translate_content_part(p: &ContentPart, envelopes: &mut EnvelopeUnwrapTally) -> ContentBlock {
    match p {
        ContentPart::Known(k) => translate_known_part(k, envelopes),
        ContentPart::Other {
            type_tag,
            cache_control,
            extras,
        } => ContentBlock::Other {
            type_tag: type_tag.clone(),
            cache_control: cache_control.clone(),
            extras: extras.clone(),
        },
    }
}

fn translate_known_part(k: &KnownContentPart, envelopes: &mut EnvelopeUnwrapTally) -> ContentBlock {
    match k {
        KnownContentPart::Text {
            text,
            citations,
            cache_control,
        } => ContentBlock::Text {
            text: text.clone(),
            cache_control: cache_control.clone(),
            citations: citations.clone(),
        },
        KnownContentPart::Image {
            source,
            cache_control,
        } => ContentBlock::Image {
            source: source.clone(),
            cache_control: cache_control.clone(),
        },
        // OpenAI-shape ImageUrl translates to an Anthropic image
        // block. Two URL shapes need different Anthropic source forms:
        //
        //   - HTTPS direct  ->  {type: "url", url: "..."}
        //   - data: URI     ->  {type: "base64", media_type: "...", data: "..."}
        //
        // Bedrock + Anthropic API both reject data: URIs in the URL
        // source form ("URL sources are not supported"); they require
        // the base64 source. OpenAI multimodal clients (claude-code's
        // OpenAI-compat fallback, vanilla OpenAI SDK, etc.) embed
        // images via `data:image/<fmt>;base64,<payload>`, so we parse
        // the data: prefix here and rewrite. Anything else
        // (https://, gs://, malformed) flows through as URL source --
        // upstream will surface a clean error if it isn't supported.
        KnownContentPart::ImageUrl {
            image_url,
            cache_control,
        } => {
            let url = image_url.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let source = parse_image_url_source(url);
            ContentBlock::Image {
                source,
                cache_control: cache_control.clone(),
            }
        }
        KnownContentPart::Document {
            source,
            title,
            citations,
            cache_control,
        } => ContentBlock::Document {
            source: source.clone(),
            title: title.clone(),
            citations: citations.clone(),
            cache_control: cache_control.clone(),
        },
        // OpenAI-shape file part. A base64 PDF upload
        // (`file.file_data` = `data:application/pdf;base64,<b64>`)
        // becomes an Anthropic document block with a base64 source --
        // Bedrock + Anthropic both require this shape and 400 on the
        // raw OpenAI `file` block otherwise. Any part we cannot
        // faithfully translate (file_id-only reference, non-base64
        // file_data, non-PDF media type, empty payload) falls back to
        // re-emitting the original block verbatim as ContentBlock::Other
        // so it still reaches the Anthropic upstream (which surfaces a
        // clean error) rather than being silently dropped here.
        KnownContentPart::File {
            file,
            cache_control,
        } => {
            if let Some((source, title)) = parse_file_document_source(file) {
                ContentBlock::Document {
                    source,
                    title,
                    citations: None,
                    cache_control: cache_control.clone(),
                }
            } else {
                let media_type = file
                    .get("file_data")
                    .and_then(|v| v.as_str())
                    .and_then(|d| d.strip_prefix("data:"))
                    .and_then(|rest| rest.split_once(";base64,"))
                    .map(|(mt, _)| mt.split(';').next().unwrap_or(mt).to_ascii_lowercase());
                let reason = match file.get("file_data").and_then(|v| v.as_str()) {
                    None => "no inline file_data (file_id reference or unsupported shape)",
                    Some(d) if !d.starts_with("data:") || !d.contains(";base64,") => {
                        "file_data is not a base64 data URI"
                    }
                    Some(_) => "file_data media type is not application/pdf",
                };
                tracing::warn!(
                    media_type = media_type.as_deref().unwrap_or("<none>"),
                    reason,
                    "cannot translate OpenAI file part to an Anthropic document; \
                 passing the block through verbatim (upstream will reject if unsupported)"
                );
                let mut extras = serde_json::Map::new();
                extras.insert("file".to_string(), file.clone());
                ContentBlock::Other {
                    type_tag: "file".to_string(),
                    cache_control: cache_control.clone(),
                    extras,
                }
            }
        }
        KnownContentPart::ToolUse {
            id,
            name,
            input,
            cache_control,
        } => ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            cache_control: cache_control.clone(),
        },
        KnownContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control,
        } => ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            cache_control: cache_control.clone(),
            is_error: *is_error,
        },
        KnownContentPart::Thinking {
            thinking,
            signature,
        } => ContentBlock::Thinking {
            thinking: thinking.clone(),
            // Wire requires signature; absent on canonical means we fall
            // back to empty. Multi-turn callers should always set this;
            // build_assistant_content errors when reasoning_details lack
            // a signature.
            signature: signature.clone().unwrap_or_default(),
            cache_control: None,
        },
        KnownContentPart::RedactedThinking { data } => ContentBlock::RedactedThinking {
            data: envelopes.wire_data(data),
            cache_control: None,
        },
    }
}

/// Aggregated reasoning-skip diagnostics for ONE outbound provider
/// attempt: every assistant turn in the request pools into these
/// counters, and `flush` emits at most one WARN per category. Without
/// the pooling a long transcript emitted O(turns) WARN lines for one
/// upstream defect.
///
/// The three categories stay SEPARATE lines because their remediations
/// differ (a missing signature is an upstream streaming defect; a
/// foreign format tag is a cross-provider replay; the backstop is a
/// predicate-drift alarm), so one attempt legitimately emits more than
/// one line. Retries and fallback attempts each build their own tally.
struct ReasoningSkipTally<'a> {
    provider: &'a str,
    unsigned_count: usize,
    unsigned_turns: usize,
    /// Index of the turn the most recent unsigned skip came from, so
    /// `unsigned_turns` counts turns rather than details. Correct because
    /// `translate_messages_threaded` walks the messages in index order --
    /// a turn is never revisited after a later one has been recorded.
    unsigned_last_turn: Option<usize>,
    unsigned_locations: BoundedLogSample<(usize, Option<u32>)>,
    format_count: usize,
    format_values: BoundedLogSample<String>,
    backstop_count: usize,
}

impl<'a> ReasoningSkipTally<'a> {
    fn new(provider: &'a str) -> Self {
        Self {
            provider,
            unsigned_count: 0,
            unsigned_turns: 0,
            unsigned_last_turn: None,
            unsigned_locations: BoundedLogSample::new(),
            format_count: 0,
            format_values: BoundedLogSample::new(),
            backstop_count: 0,
        }
    }

    /// Record one reasoning detail skipped for a missing or empty
    /// signature. `message_index` is the canonical request index and
    /// `detail_index` the upstream-supplied index WITHIN that message --
    /// each message's `reasoning_details` has its own index space, so a
    /// bare detail index pooled across turns cannot be located. A `None`
    /// detail index stays `None` rather than being flattened to a
    /// plausible integer.
    fn record_unsigned(&mut self, message_index: usize, detail_index: Option<u32>) {
        self.unsigned_count = self.unsigned_count.saturating_add(1);
        if self.unsigned_last_turn != Some(message_index) {
            self.unsigned_turns = self.unsigned_turns.saturating_add(1);
            self.unsigned_last_turn = Some(message_index);
        }
        self.unsigned_locations.push((message_index, detail_index));
    }

    /// Record one reasoning detail skipped because its format tag is not
    /// `anthropic-claude-v1`. The tag is caller-supplied, so it is
    /// sanitized BEFORE the distinctness test: that bounds each entry's
    /// length as it is collected and collapses tags differing only in
    /// control characters into one slot instead of letting them each
    /// claim one.
    fn record_format(&mut self, format: Option<&str>) {
        self.format_count = self.format_count.saturating_add(1);
        self.format_values
            .push_distinct(sanitize_for_log(format.unwrap_or("<none>")));
    }

    /// Record one assistant turn whose wire content assembled empty and
    /// needed the empty-text backstop.
    const fn record_backstop(&mut self) {
        self.backstop_count = self.backstop_count.saturating_add(1);
    }

    /// Emit one WARN per non-empty category. Called exactly once, by
    /// `translate_messages`, after the threaded walk returns -- never
    /// from `Drop`, so the emission is explicit and testable.
    ///
    /// No reasoning payload reaches a log field (it could be reasoning
    /// over sensitive data); only counts, canonical indices, and
    /// sanitized format tags do. Every count is its own exact counter --
    /// never a sample's stored length, which caps.
    fn flush(&self) {
        if self.unsigned_count > 0 {
            tracing::warn!(
                provider = self.provider,
                skipped_count = self.unsigned_count,
                turns_affected = self.unsigned_turns,
                skipped_locations = ?self.unsigned_locations.items(),
                skipped_locations_truncated = self.unsigned_locations.truncated(),
                "skipping Thinking blocks on replay: signature missing or empty \
                 (multi-block thinking history is now partially echoed; \
                 see CLAUDE.md \"Anthropic streaming reasoning replay\" residual)"
            );
        }
        if self.format_count > 0 {
            tracing::warn!(
                provider = self.provider,
                skipped_count = self.format_count,
                skipped_formats = ?self.format_values.items(),
                formats_truncated = self.format_values.truncated(),
                "skipping reasoning blocks on replay: format is not anthropic-claude-v1 \
                 (non-Anthropic format details cannot be echoed as Anthropic Thinking blocks)"
            );
        }
        if self.backstop_count > 0 {
            tracing::warn!(
                provider = self.provider,
                event = "empty_content_backstop",
                backstop_count = self.backstop_count,
                "assistant content assembled empty after reasoning/tool_call \
                 emission; inserting one empty text block so an invalid \
                 content: [] never reaches the wire (last-resort backstop)."
            );
        }
    }
}

/// Reconstruct an Anthropic content array for an assistant message that
/// carries reasoning_details (tool-use continuity). thinking blocks with
/// signatures must be passed back verbatim.
///
/// `message_index` is this turn's index in the canonical request array,
/// threaded in so the aggregated diagnostics can name the turn a skipped
/// detail came from. It is never inferred from the output array: system
/// removal and tool-run folding make output positions a different
/// coordinate system.
fn build_assistant_content(
    id: &str,
    message_index: usize,
    msg: &Message,
    envelopes: &mut EnvelopeUnwrapTally,
    skips: &mut ReasoningSkipTally<'_>,
) -> Result<AnthropicContent> {
    let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
    if msg.reasoning_details.is_empty() && !has_tool_calls {
        // No multi-turn reasoning to thread back AND no OpenAI-shape
        // tool_calls field to re-emit; fall through to the generic
        // content translation (Text or Parts), but strip trailing
        // text-after-tool_use first (see helper docstring).
        return Ok(translate_assistant_simple_content(&msg.content, envelopes));
    }

    let mut blocks =
        emit_reasoning_blocks(message_index, &msg.reasoning_details, envelopes, skips)?;
    let emitted_tool_ids = append_assistant_message_blocks(&mut blocks, &msg.content, envelopes);
    if let Some(tool_calls) = msg.tool_calls.as_ref() {
        emit_tool_use_blocks_from_calls(id, tool_calls, &mut blocks, &emitted_tool_ids)?;
    }
    if blocks.is_empty() {
        // Last-resort backstop. `normalize_replay_invariants` drops a
        // reasoning-only turn whose reasoning is non-emittable, but it
        // returns early (Borrowed) on the Preserve and no-strip paths and
        // never inspects those messages -- a Null-content turn carrying
        // only unsigned reasoning_details reaches here with an empty
        // block list. Anthropic 400s on content: [], so insert one empty
        // text block. Should be rare: frequent firing means the
        // emittability predicate drifted from the emit behavior. The WARN
        // is aggregated per attempt via the tally rather than emitted
        // per turn.
        skips.record_backstop();
        blocks.push(ContentBlock::Text {
            text: String::new(),
            citations: None,
            cache_control: None,
        });
    }
    Ok(AnthropicContent::Blocks(blocks))
}

/// Re-emit OpenAI-shape `tool_calls` (the canonical
/// representation produced by `walk_content_blocks` on the
/// response side) as Anthropic `ContentBlock::ToolUse` entries
/// for multi-turn replay. Without this, an OpenAI-ingress
/// request whose assistant history carries `tool_calls` -- or a
/// caller that echoes a canonical Message returned by routectl
/// straight back as a multi-turn turn -- would silently drop the
/// tool_use blocks, and the next user turn's `tool_result` would
/// fail upstream with "tool_use ids were found without
/// preceding tool_use blocks".
///
/// OpenAI shape: `{id, type: "function", function: {name, arguments}}`
/// where `arguments` is a JSON-encoded STRING. Anthropic shape:
/// `ContentBlock::ToolUse { id, name, input: Value }` where
/// `input` is the parsed JSON object. We attempt parsing first
/// and fall back to wrapping the raw string under
/// `{"_arguments": "..."}` so the upstream can return a useful
/// error rather than us silently producing a malformed body.
///
/// `already_emitted` holds the RAW (pre-sanitization) ids of any ToolUse
/// content-part blocks `append_assistant_message_blocks` already pushed. A
/// canonical assistant message produced by the response parser fills BOTH
/// channels for one tool call (a ToolUse content part AND a `tool_calls`
/// entry with the same id), so emitting from both would put two tool_use
/// blocks with the same id on the wire -- which Anthropic rejects. We skip
/// any call whose RAW id is already present; the content-part channel wins
/// because it preserves interleaving with surrounding text. Deduping on the
/// raw id (not the sanitized one) means two distinct calls whose ids differ
/// only by an escaped char (e.g. `call.a` and `call:a`) both survive, and
/// because sanitization is injective they reach the wire under distinct ids.
fn emit_tool_use_blocks_from_calls(
    id: &str,
    tool_calls: &[Value],
    blocks: &mut Vec<ContentBlock>,
    already_emitted: &HashSet<String>,
) -> Result<()> {
    for call in tool_calls {
        let raw_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if already_emitted.contains(raw_id) {
            // Same tool call already emitted from the ToolUse content-part
            // channel; emitting it again would duplicate the id on the wire.
            continue;
        }
        let tool_id = crate::tool_id::sanitize_tool_id(raw_id).into_owned();
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let arguments_raw = function
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        let input = if arguments_raw.is_empty() {
            json!({})
        } else {
            serde_json::from_str(arguments_raw).unwrap_or_else(|e| {
                tracing::warn!(
                    provider = id,
                    tool_id = %sanitize_for_log(&tool_id),
                    error = %e,
                    "tool_call.arguments not valid JSON; wrapping under _arguments for upstream",
                );
                json!({ "_arguments": arguments_raw })
            })
        };
        blocks.push(ContentBlock::ToolUse {
            id: tool_id,
            name,
            input,
            cache_control: None,
        });
    }
    Ok(())
}

/// Translate `reasoning_details` into Anthropic `Thinking` /
/// `RedactedThinking` blocks for echo on a multi-turn assistant turn.
/// Index-ordered so an upstream that re-orders reasoning blocks
/// doesn't surprise the downstream signature check. Anthropic rejects
/// a `Thinking` block on echo without the `signature` field; when a
/// detail's signature is missing or empty (Anthropic 4.5 occasionally
/// omits `signature_delta` on tool-only thinking turns), the detail
/// is logged at WARN and skipped so replay doesn't 400 on a
/// guaranteed-malformed echo. WARN level (not DEBUG) so operators
/// see the partial echo and can correlate with upstream cache misses
/// or quality drift -- mixed signed/unsigned histories lose ordering
/// fidelity. See CLAUDE.md "Anthropic streaming reasoning replay".
///
/// The `format` tag on a `ReasoningDetail` is CLIENT-SUPPLIED on the
/// request schema, so a caller can tag any payload `anthropic-claude-v1`
/// and reach the `RedactedThinking` arm below with arbitrary `data`,
/// wrapped envelope included. `envelopes` therefore applies to this
/// channel exactly as it does to the content-part walk;
/// `is_anthropic_emittable_detail` decides emittability only and is not a
/// barrier here.
///
/// Skips are recorded into `skips` rather than logged here: the WARNs are
/// aggregated across every assistant turn of one outbound attempt and
/// emitted once, by `translate_messages`. `message_index` labels which
/// canonical turn each skip came from.
fn emit_reasoning_blocks(
    message_index: usize,
    details: &[ReasoningDetail],
    envelopes: &mut EnvelopeUnwrapTally,
    skips: &mut ReasoningSkipTally<'_>,
) -> Result<Vec<ContentBlock>> {
    let mut sorted = details.to_vec();
    sorted.sort_by_key(|d| d.index.unwrap_or(0));

    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(sorted.len());
    for detail in &sorted {
        if !is_anthropic_emittable_detail(detail) {
            // Not emittable: categorize for the aggregated WARNs so
            // operators can distinguish a foreign-format drop from an
            // unsigned drop. A non-anthropic-claude-v1 format is a format
            // skip; a Text detail with the right format but an empty
            // signature is an unsigned skip; Summary details are silently
            // non-emittable (never a wire block).
            match detail.kind {
                ReasoningDetailKind::Text | ReasoningDetailKind::Encrypted
                    if detail.format.as_deref() != Some(super::ANTHROPIC_FORMAT) =>
                {
                    skips.record_format(detail.format.as_deref());
                }
                ReasoningDetailKind::Text => {
                    skips.record_unsigned(message_index, detail.index);
                }
                ReasoningDetailKind::Encrypted | ReasoningDetailKind::Summary => {}
            }
            continue;
        }
        match detail.kind {
            ReasoningDetailKind::Text => {
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
                    .unwrap_or("");
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature: signature.to_string(),
                    cache_control: None,
                });
            }
            ReasoningDetailKind::Encrypted => {
                let data = detail
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                blocks.push(ContentBlock::RedactedThinking {
                    data: envelopes.wire_data(data),
                    cache_control: None,
                });
            }
            ReasoningDetailKind::Summary => {}
        }
    }
    Ok(blocks)
}

/// Append the assistant message's text/parts content AFTER the
/// reasoning blocks already pushed. For Text, emits a single Text
/// block (skipped on empty/Null since reasoning-only assistant turns
/// are valid). For Parts, translates each block (after stripping
/// trailing text-after-tool_use, which both Bedrock and Anthropic
/// reject with "tool_use ids were found without tool_result blocks
/// immediately after").
///
/// ToolUse content parts get their id run through `sanitize_tool_id`
/// here -- the same normalization `emit_tool_use_blocks_from_calls`
/// applies -- so a tool call surfacing on both channels cannot reach the
/// wire with two divergent ids. Returns the set of RAW (pre-sanitization)
/// ids emitted as ToolUse blocks so the tool_calls channel can skip only
/// genuine duplicates (same source id), never two distinct calls that
/// merely sanitize to the same value.
fn append_assistant_message_blocks(
    blocks: &mut Vec<ContentBlock>,
    content: &MessageContent,
    envelopes: &mut EnvelopeUnwrapTally,
) -> HashSet<String> {
    let mut emitted_tool_ids: HashSet<String> = HashSet::new();
    match content {
        MessageContent::Text(t) if !t.is_empty() => blocks.push(ContentBlock::Text {
            text: t.clone(),
            cache_control: None,
            citations: None,
        }),
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            for p in &cleaned {
                let (block, tool_id) = translate_assistant_content_part(p, envelopes);
                if let Some(tool_id) = tool_id {
                    emitted_tool_ids.insert(tool_id);
                }
                blocks.push(block);
            }
        }
    }
    emitted_tool_ids
}

/// Translate one assistant content part, sanitizing a ToolUse part's id
/// so it matches the normalization the tool_calls channel and the
/// tool_result correlation site (`build_tool_message`) apply -- an
/// unsanitized tool_use id would orphan its sanitized tool_result.
/// Returns the RAW (pre-sanitization) id when the part is a ToolUse so
/// callers can dedupe against the tool_calls channel on the unambiguous
/// source identity: two distinct raw ids (e.g. `call.a` and `call:a`) are
/// separate tool calls and must both survive. The emitted wire block
/// carries the sanitized id, which is injective, so the two do not
/// collide on the wire either. Non-ToolUse parts delegate to the generic
/// `translate_content_part`.
fn translate_assistant_content_part(
    p: &ContentPart,
    envelopes: &mut EnvelopeUnwrapTally,
) -> (ContentBlock, Option<String>) {
    if let ContentPart::Known(KnownContentPart::ToolUse {
        id,
        name,
        input,
        cache_control,
    }) = p
    {
        let tool_id = crate::tool_id::sanitize_tool_id(id).into_owned();
        let block = ContentBlock::ToolUse {
            id: tool_id,
            name: name.clone(),
            input: input.clone(),
            cache_control: cache_control.clone(),
        };
        (block, Some(id.clone()))
    } else {
        (translate_content_part(p, envelopes), None)
    }
}

/// Assistant-message variant of `translate_simple_content` that strips
/// trailing text-after-tool_use before per-part translation. Called
/// only from `build_assistant_content`. Text/Null arms delegate to
/// `translate_simple_content` so the two stay in lockstep -- only the
/// `Parts` arm needs the strip.
fn translate_assistant_simple_content(
    c: &MessageContent,
    envelopes: &mut EnvelopeUnwrapTally,
) -> AnthropicContent {
    match c {
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            AnthropicContent::Blocks(
                cleaned
                    .iter()
                    .map(|p| translate_assistant_content_part(p, envelopes).0)
                    .collect(),
            )
        }
        // Text/Null arms are identical to `translate_simple_content`;
        // delegate to keep them in one place.
        _ => translate_simple_content(c, envelopes),
    }
}

/// Translate plain message content (no multi-turn reasoning context).
/// Text -> AnthropicContent::Text (cheaper wire form). Parts ->
/// AnthropicContent::Blocks via per-part translation.
fn translate_simple_content(
    c: &MessageContent,
    envelopes: &mut EnvelopeUnwrapTally,
) -> AnthropicContent {
    match c {
        MessageContent::Text(t) => AnthropicContent::Text(t.clone()),
        MessageContent::Null => AnthropicContent::Text(String::new()),
        MessageContent::Parts(parts) => AnthropicContent::Blocks(
            parts
                .iter()
                .map(|p| translate_content_part(p, envelopes))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Tool-role messages
// ---------------------------------------------------------------------------

/// Fold a run of canonical tool-result turns into ONE user-role message
/// carrying one `tool_result` block per turn, in submission order. A
/// single-element run yields exactly the one-block message this seam has
/// always emitted. Mirrors the Bedrock Converse egress, whose
/// `push_or_coalesce` merges the same synthesized user turns: real
/// Anthropic combines consecutive user turns server-side, but a strict
/// Anthropic-compatible gateway rejects them.
fn build_tool_message(run: &[&Message], envelopes: &mut EnvelopeUnwrapTally) -> AnthropicMessage {
    AnthropicMessage {
        role: AnthropicRole::User,
        content: AnthropicContent::Blocks(
            run.iter()
                .copied()
                .map(|m| build_tool_result_block(m, envelopes))
                .collect(),
        ),
    }
}

fn build_tool_result_block(msg: &Message, envelopes: &mut EnvelopeUnwrapTally) -> ContentBlock {
    // Sanitize to the same charset the tool_use emit uses so a result
    // for an OpenAI-origin id (`call.foo:1`) still correlates with its
    // tool_use block after both are mapped to the same wire id.
    let tool_use_id =
        crate::tool_id::sanitize_tool_id(msg.tool_call_id.as_deref().unwrap_or("")).into_owned();
    // Anthropic tool_result.content accepts either a string or an array
    // of content blocks. We honor whichever shape the canonical message
    // carries.
    let content_val = match &msg.content {
        MessageContent::Text(t) => Value::String(t.clone()),
        MessageContent::Parts(parts) => Value::Array(
            parts
                .iter()
                .map(|p| {
                    serde_json::to_value(translate_content_part(p, envelopes))
                        .unwrap_or(Value::Null)
                })
                .collect(),
        ),
        MessageContent::Null => Value::Null,
    };
    ContentBlock::ToolResult {
        tool_use_id,
        content: ensure_min_tool_result_content(content_val),
        cache_control: None,
        is_error: None,
    }
}

/// Anthropic rejects `tool_result.content: null` and `content: []`
/// (content must be a string or a non-empty array of content blocks).
/// Empty tool output is a legal, common shape -- a Null-content or
/// empty-array tool message maps to an empty string. Mirrors the Bedrock
/// Converse egress `ensure_min_tool_result_content` guard so the two
/// Anthropic-shape seams agree on the same canonical input; the string
/// form matches Converse's empty-string Text block.
fn ensure_min_tool_result_content(content: Value) -> Value {
    match &content {
        Value::Null => Value::String(String::new()),
        Value::Array(arr) if arr.is_empty() => Value::String(String::new()),
        _ => content,
    }
}

// ---------------------------------------------------------------------------
// Per-role dispatch
// ---------------------------------------------------------------------------

/// Iterate the canonical messages and produce the Anthropic-shaped
/// per-role list. System messages are intentionally dropped here --
/// they're already lifted into `req.system` (canonical) or by
/// `lift_legacy_system` for direct callers without an ingress, so
/// re-emitting them as messages would duplicate.
///
/// A run of CONSECUTIVE `Role::Tool` turns (the parallel-tool-call reply
/// shape) folds into one user message carrying one `tool_result` block per
/// turn -- see `build_tool_message`. Any non-tool turn ends the run, so
/// tool results separated by an assistant turn stay in separate messages
/// and nothing is reordered across the boundary.
///
/// `envelopes` carries the already-resolved reasoning-envelope policy for
/// this request -- see [`EnvelopeUnwrapTally`]. It is owned by
/// `request::normalize` rather than by this function because the
/// context-management reinjection path constructs `redacted_thinking`
/// blocks too, after this returns, and both channels must share one tally
/// so the aggregated WARN stays at one line per request.
///
/// The reasoning-skip tally, by contrast, is owned HERE: this walk is its
/// only feeder, so a wider owner would spread the flush over call sites
/// that never fill it. Every fallible step lives in
/// `translate_messages_threaded`, so no `?` can return past the flush --
/// the single-emission guarantee is structural rather than a discipline
/// each early return has to remember.
pub(super) fn translate_messages(
    id: &str,
    messages: &[Message],
    envelopes: &mut EnvelopeUnwrapTally,
) -> Result<Vec<AnthropicMessage>> {
    let mut skips = ReasoningSkipTally::new(id);
    let out = translate_messages_threaded(id, messages, envelopes, &mut skips);
    skips.flush();
    out
}

/// The per-role walk itself, threading the reasoning-skip tally through
/// every assistant turn. Holds every `?` in the translation so its caller
/// can flush the tally unconditionally.
fn translate_messages_threaded(
    id: &str,
    messages: &[Message],
    envelopes: &mut EnvelopeUnwrapTally,
    skips: &mut ReasoningSkipTally<'_>,
) -> Result<Vec<AnthropicMessage>> {
    let mut out: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    let mut i = 0usize;
    while i < messages.len() {
        let msg = &messages[i];
        match msg.role {
            Role::System => {
                // Already handled via req.system / lift_legacy_system.
                // Drop here (do not duplicate in the messages array).
                i += 1;
            }
            Role::User => {
                out.push(AnthropicMessage {
                    role: AnthropicRole::User,
                    content: translate_simple_content(&msg.content, envelopes),
                });
                i += 1;
            }
            Role::Assistant => {
                out.push(AnthropicMessage {
                    role: AnthropicRole::Assistant,
                    content: build_assistant_content(id, i, msg, envelopes, skips)?,
                });
                i += 1;
            }
            Role::Tool => {
                let (run, run_end) = collect_tool_run(messages, i);
                out.push(build_tool_message(&run, envelopes));
                i = run_end;
            }
        }
    }
    Ok(out)
}

/// Gather the run of tool turns starting at `start`, returning the turns
/// and the index of the first message that ends the run. `Role::System`
/// emits nothing (it was lifted into `req.system`), so it is transparent
/// here rather than a run boundary -- the Converse egress, which drops
/// System before its coalescing step, behaves the same way. Only a User
/// or Assistant turn ends the run.
fn collect_tool_run(messages: &[Message], start: usize) -> (Vec<&Message>, usize) {
    let mut run: Vec<&Message> = Vec::new();
    let mut i = start;
    while let Some(msg) = messages.get(i) {
        match msg.role {
            Role::Tool => run.push(msg),
            Role::System => {}
            Role::User | Role::Assistant => break,
        }
        i += 1;
    }
    (run, i)
}

#[cfg(test)]
mod translate_file_part_tests {
    use super::ContentBlock;
    use super::{passthrough_tally, translate_content_part};
    use routectl_core::{ContentPart, KnownContentPart};
    use serde_json::json;

    fn file_part(file: serde_json::Value) -> ContentPart {
        ContentPart::Known(KnownContentPart::File {
            file,
            cache_control: None,
        })
    }

    #[test]
    fn pdf_data_uri_translates_to_document_block_with_base64_source() {
        let part = file_part(json!({
            "filename": "draft.pdf",
            "file_data": "data:application/pdf;base64,JVBERi0xLjQ="
        }));
        match translate_content_part(&part, &mut passthrough_tally()) {
            ContentBlock::Document {
                source,
                title,
                citations,
                ..
            } => {
                assert_eq!(source["type"], "base64");
                assert_eq!(source["media_type"], "application/pdf");
                assert_eq!(source["data"], "JVBERi0xLjQ=");
                assert_eq!(title.as_deref(), Some("draft.pdf"));
                assert!(citations.is_none());
            }
            other => panic!("expected Document, got {other:?}"),
        }
    }

    #[test]
    fn file_id_only_falls_back_to_other_passthrough() {
        // No inline bytes -> verbatim passthrough as a `file` block so an
        // Anthropic upstream surfaces a clean error rather than a silent
        // drop. The original nested `file` object is preserved.
        let part = file_part(json!({"file_id": "file-abc"}));
        match translate_content_part(&part, &mut passthrough_tally()) {
            ContentBlock::Other {
                type_tag, extras, ..
            } => {
                assert_eq!(type_tag, "file");
                assert_eq!(extras["file"], json!({"file_id": "file-abc"}));
            }
            other => panic!("expected Other passthrough, got {other:?}"),
        }
    }

    #[test]
    fn non_pdf_media_type_falls_back_to_other_passthrough_without_panic() {
        let part = file_part(json!({
            "filename": "note.txt",
            "file_data": "data:text/plain;base64,aGVsbG8="
        }));
        match translate_content_part(&part, &mut passthrough_tally()) {
            ContentBlock::Other { type_tag, .. } => assert_eq!(type_tag, "file"),
            other => panic!("expected Other passthrough, got {other:?}"),
        }
    }

    #[test]
    fn empty_base64_payload_falls_back_to_other_passthrough() {
        let part = file_part(json!({
            "filename": "draft.pdf",
            "file_data": "data:application/pdf;base64,"
        }));
        match translate_content_part(&part, &mut passthrough_tally()) {
            ContentBlock::Other { type_tag, .. } => assert_eq!(type_tag, "file"),
            other => panic!("expected Other passthrough, got {other:?}"),
        }
    }

    #[test]
    fn pdf_file_part_honors_block_level_cache_control() {
        use routectl_core::CacheControl;
        let part = ContentPart::Known(KnownContentPart::File {
            file: json!({
                "filename": "draft.pdf",
                "file_data": "data:application/pdf;base64,JVBERi0xLjQ="
            }),
            cache_control: Some(CacheControl::ephemeral_5m()),
        });
        match translate_content_part(&part, &mut passthrough_tally()) {
            ContentBlock::Document { cache_control, .. } => {
                assert!(cache_control.is_some());
            }
            other => panic!("expected Document, got {other:?}"),
        }
    }

    // Starts from a RAW Anthropic response body (not a pre-parsed
    // ContentBlock) so a missed deser site cannot be masked. A pure-text
    // response collapses to MessageContent::Text and would not exercise
    // the Part egress, so a tool_use block rides along to force Parts.
    #[test]
    fn text_block_citations_survive_raw_json_round_trip() {
        use routectl_core::MessageContent;

        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-8",
            "content": [
                {
                    "type": "text",
                    "text": "The sky is blue.",
                    "citations": [{
                        "type": "char_location",
                        "cited_text": "sky is blue",
                        "document_index": 0,
                        "document_title": "Colors",
                        "start_char_index": 4,
                        "end_char_index": 15
                    }]
                },
                {
                    "type": "tool_use",
                    "id": "t1",
                    "name": "lookup",
                    "input": {"q": "sky"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });

        let resp = crate::anthropic_api::response::normalize("test", raw).expect("normalize");
        let MessageContent::Parts(parts) = &resp.choices[0].message.content else {
            panic!("expected Parts content for a text + tool_use response");
        };
        let text_part = parts
            .iter()
            .find(|p| matches!(p, ContentPart::Known(KnownContentPart::Text { .. })))
            .expect("text part present");

        let wire =
            serde_json::to_value(translate_content_part(text_part, &mut passthrough_tally()))
                .expect("serialize");

        assert_eq!(wire["type"], "text");
        assert_eq!(wire["text"], "The sky is blue.");
        assert_eq!(
            wire["citations"][0]["cited_text"], "sky is blue",
            "text-block citations must survive ingress -> canonical -> egress"
        );
    }
}

#[cfg(test)]
mod thinking_signature_tests {
    use super::{
        B64_STANDARD, is_claude_shaped_signature, is_unsigned_thinking_part,
        normalize_replay_invariants,
    };
    use base64::Engine;
    use routectl_core::{
        ChatRequest, ContentPart, CoreHistoryReasoning, KnownContentPart, Message, MessageContent,
        Role,
    };

    /// A genuine E-shaped Claude signature: base64 of a payload whose
    /// first byte is 0x12.
    fn e_signature() -> String {
        B64_STANDARD.encode([0x12u8, 0x34, 0x56, 0x78])
    }

    /// A genuine R-shaped Claude signature: base64 of the E-signature's
    /// own bytes (double-layer).
    fn r_signature() -> String {
        B64_STANDARD.encode(e_signature().as_bytes())
    }

    fn thinking_part(signature: Option<String>) -> ContentPart {
        ContentPart::Known(KnownContentPart::Thinking {
            thinking: "step by step".to_string(),
            signature,
        })
    }

    fn assistant_with_parts(parts: Vec<ContentPart>) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(parts),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn e_prefixed_signature_with_0x12_payload_is_claude_shaped() {
        // Arrange
        let sig = e_signature();
        // Act / Assert
        assert!(is_claude_shaped_signature(&sig));
    }

    #[test]
    fn r_prefixed_double_layer_signature_is_claude_shaped() {
        // Arrange
        let sig = r_signature();
        // Act / Assert
        assert!(is_claude_shaped_signature(&sig));
    }

    #[test]
    fn uuid_signature_is_not_claude_shaped() {
        // Arrange -- a gpt/gemini-style uuid, not base64-prefixed by E/R.
        let sig = "550e8400-e29b-41d4-a716-446655440000";
        // Act / Assert
        assert!(!is_claude_shaped_signature(sig));
    }

    #[test]
    fn base64_with_non_0x12_first_byte_is_not_claude_shaped() {
        // Arrange -- E-prefixed valid base64 but decoded byte0 != 0x12.
        let sig = B64_STANDARD.encode([0x99u8, 0x34, 0x56]);
        // a base64 of arbitrary bytes is unlikely to start with 'E';
        // force the E-path with a crafted payload whose base64 begins 'E'.
        // 0x10.. encodes to a leading 'E' in standard base64.
        let crafted = B64_STANDARD.encode([0x10u8, 0x00, 0x00]);
        // Act / Assert
        assert!(!is_claude_shaped_signature(&sig));
        assert!(crafted.starts_with('E'));
        assert!(!is_claude_shaped_signature(&crafted));
    }

    #[test]
    fn malformed_base64_is_not_claude_shaped_without_panic() {
        // Arrange -- E-prefixed but not valid base64 (illegal chars/len).
        let sig = "E!!!not base64!!!";
        // Act / Assert
        assert!(!is_claude_shaped_signature(sig));
    }

    #[test]
    fn empty_signature_is_not_claude_shaped() {
        assert!(!is_claude_shaped_signature(""));
    }

    #[test]
    fn cache_prefixed_e_signature_is_claude_shaped() {
        // Arrange -- a `<word>#` cache prefix precedes the real signature.
        let sig = format!("some-model-group#{}", e_signature());
        // Act / Assert
        assert!(is_claude_shaped_signature(&sig));
    }

    #[test]
    fn predicate_strips_thinking_with_foreign_signature() {
        // Arrange
        let part = thinking_part(Some("550e8400-e29b-41d4-a716-446655440000".to_string()));
        // Act / Assert
        assert!(is_unsigned_thinking_part(&part));
    }

    #[test]
    fn predicate_keeps_thinking_with_e_signature() {
        let part = thinking_part(Some(e_signature()));
        assert!(!is_unsigned_thinking_part(&part));
    }

    #[test]
    fn predicate_strips_thinking_with_empty_signature() {
        let part = thinking_part(Some(String::new()));
        assert!(is_unsigned_thinking_part(&part));
    }

    #[test]
    fn egress_strips_foreign_signed_thinking_preserves_claude_signed() {
        // Arrange -- one foreign-signed (strip), one E-signed and one
        // R-signed thinking block (preserve), plus a text part.
        let foreign = thinking_part(Some("not-a-claude-sig".to_string()));
        let e_kept = thinking_part(Some(e_signature()));
        let r_kept = thinking_part(Some(r_signature()));
        let text = ContentPart::Known(KnownContentPart::Text {
            text: "answer".to_string(),
            citations: None,
            cache_control: None,
        });
        let msg = assistant_with_parts(vec![foreign, e_kept, r_kept, text]);
        let req = ChatRequest {
            messages: vec![msg].into(),
            ..Default::default()
        };

        // Act
        let out = normalize_replay_invariants("anthropic", &req, CoreHistoryReasoning::Auto)
            .expect("strip should not error");

        // Assert -- foreign thinking dropped; both Claude-signed kept.
        let MessageContent::Parts(parts) = &out[0].content else {
            panic!("expected Parts content");
        };
        let thinking_sigs: Vec<&str> = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Known(KnownContentPart::Thinking { signature, .. }) => {
                    Some(signature.as_deref().unwrap_or(""))
                }
                _ => None,
            })
            .collect();
        assert_eq!(thinking_sigs.len(), 2);
        assert!(thinking_sigs.contains(&e_signature().as_str()));
        assert!(thinking_sigs.contains(&r_signature().as_str()));
        // Text part survives.
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn egress_strips_empty_signed_thinking() {
        // Arrange
        let empty = thinking_part(Some(String::new()));
        let text = ContentPart::Known(KnownContentPart::Text {
            text: "answer".to_string(),
            citations: None,
            cache_control: None,
        });
        let req = ChatRequest {
            messages: vec![assistant_with_parts(vec![empty, text])].into(),
            ..Default::default()
        };

        // Act
        let out = normalize_replay_invariants("anthropic", &req, CoreHistoryReasoning::Auto)
            .expect("strip should not error");

        // Assert -- only the text part remains.
        let MessageContent::Parts(parts) = &out[0].content else {
            panic!("expected Parts content");
        };
        assert_eq!(parts.len(), 1);
        assert!(matches!(
            &parts[0],
            ContentPart::Known(KnownContentPart::Text { .. })
        ));
    }
}

#[cfg(test)]
mod tool_id_correlation_tests {
    use super::{ContentBlock, passthrough_tally, translate_messages};
    use crate::anthropic_api::types::{AnthropicContent, AnthropicMessage};
    use routectl_core::{Message, MessageContent, Role};
    use serde_json::json;

    fn user_msg() -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant_with_tool_call(id: &str) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Null,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": id,
                "type": "function",
                "function": {"name": "f", "arguments": "{}"},
            })]),
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            refusal: None,
            role: Role::Tool,
            content: MessageContent::Text("ok".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: Some(id.into()),
            tool_calls: None,
        }
    }

    fn tool_use_id(out: &[AnthropicMessage]) -> String {
        out.iter()
            .find_map(|m| match &m.content {
                AnthropicContent::Blocks(blocks) => blocks.iter().find_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .expect("tool_use block must be present")
    }

    fn tool_result_id(out: &[AnthropicMessage]) -> String {
        out.iter()
            .find_map(|m| match &m.content {
                AnthropicContent::Blocks(blocks) => blocks.iter().find_map(|b| match b {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .expect("tool_result block must be present")
    }

    /// An OpenAI-origin id with `.`/`:` is sanitized identically at the
    /// tool_use emit AND the tool_result correlation site, so the result
    /// is not orphaned.
    #[test]
    fn openai_origin_tool_id_sanitized_consistently_across_anthropic_egress() {
        // Arrange
        let messages = vec![
            user_msg(),
            assistant_with_tool_call("call.foo:1"),
            tool_result("call.foo:1"),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translation must not error");

        // Assert
        assert_eq!(tool_use_id(&out), "esc_call_2efoo_3a1");
        assert_eq!(tool_result_id(&out), "esc_call_2efoo_3a1");
        assert_eq!(tool_use_id(&out), tool_result_id(&out));
    }

    /// A valid id round-trips unchanged through both the tool_use emit and
    /// the tool_result correlation site.
    #[test]
    fn valid_tool_id_round_trips_unchanged_through_anthropic_egress() {
        // Arrange
        let messages = vec![
            user_msg(),
            assistant_with_tool_call("call_abc-1_2"),
            tool_result("call_abc-1_2"),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translation must not error");

        // Assert
        assert_eq!(tool_use_id(&out), "call_abc-1_2");
        assert_eq!(tool_result_id(&out), "call_abc-1_2");
    }

    /// Two DISTINCT source tool ids that differ only in characters the
    /// former lossy sanitizer folded to `_` (`call.a` / `call:a` -> both
    /// `call_a`) must reach the wire as DISTINCT tool_use ids -- Anthropic
    /// rejects a message carrying two tool_use blocks with the same id.
    #[test]
    fn colliding_source_tool_ids_emit_distinct_wire_ids() {
        // Arrange -- one assistant turn carrying both calls.
        let mut assistant = assistant_with_tool_call("call.a");
        let Some(calls) = assistant.tool_calls.as_mut() else {
            panic!("fixture must carry tool_calls");
        };
        calls.push(json!({
            "id": "call:a",
            "type": "function",
            "function": {"name": "f", "arguments": "{}"},
        }));
        let messages = vec![user_msg(), assistant];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translation must not error");

        // Assert
        let ids = tool_use_ids(&out);
        assert_eq!(ids.len(), 2, "both distinct source calls must survive");
        assert_ne!(ids[0], ids[1], "wire ids must not collide");
    }

    /// The collision fix must not break pairing: with a colliding id pair
    /// each tool_result still correlates to exactly one tool_use, and to
    /// the right one.
    #[test]
    fn colliding_source_tool_ids_keep_use_result_correlation() {
        // Arrange
        let mut assistant = assistant_with_tool_call("call.a");
        let Some(calls) = assistant.tool_calls.as_mut() else {
            panic!("fixture must carry tool_calls");
        };
        calls.push(json!({
            "id": "call:a",
            "type": "function",
            "function": {"name": "f", "arguments": "{}"},
        }));
        let messages = vec![
            user_msg(),
            assistant,
            tool_result("call.a"),
            tool_result("call:a"),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translation must not error");

        // Assert -- the two result ids are distinct and are exactly the
        // two emitted tool_use ids, so neither result is orphaned nor
        // mispaired onto the other call.
        let uses = tool_use_ids(&out);
        let results = tool_result_ids(&out);
        assert_eq!(results.len(), 2);
        assert_ne!(results[0], results[1]);
        assert_eq!(uses, results);
    }

    /// A wire-safe id over the 64-byte `toolUseId` ceiling folds to the
    /// digest form at BOTH the tool_use emit and the tool_result
    /// correlation site, so the result is not orphaned. The expected value
    /// is the literal the Bedrock Converse lane pins too -- the fold must
    /// not depend on which lane sanitizes the id, or a fallback chain that
    /// emits the use on one lane and replays the result on another breaks
    /// correlation.
    #[test]
    fn over_long_wire_safe_tool_id_folds_consistently_across_anthropic_egress() {
        // Arrange -- 65 bytes, entirely in the target charset.
        let raw = "a".repeat(65);
        let messages = vec![
            user_msg(),
            assistant_with_tool_call(&raw),
            tool_result(&raw),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translation must not error");

        // Assert
        let expected = format!("esct_{}_8087e9a889f8a14c", "a".repeat(42));
        assert_eq!(tool_use_id(&out), expected);
        assert_eq!(tool_result_id(&out), expected);
        assert_eq!(expected.len(), 64);
    }

    fn tool_use_ids(out: &[AnthropicMessage]) -> Vec<String> {
        collect_block_ids(out, |b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
    }

    fn tool_result_ids(out: &[AnthropicMessage]) -> Vec<String> {
        collect_block_ids(out, |b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
    }

    fn collect_block_ids(
        out: &[AnthropicMessage],
        pick: impl Fn(&ContentBlock) -> Option<String>,
    ) -> Vec<String> {
        out.iter()
            .filter_map(|m| match &m.content {
                AnthropicContent::Blocks(blocks) => Some(blocks),
                _ => None,
            })
            .flatten()
            .filter_map(pick)
            .collect()
    }
}

#[cfg(test)]
mod empty_content_backstop_tests {
    use super::{
        ReasoningSkipTally, build_assistant_content, normalize_replay_invariants,
        passthrough_tally, translate_messages,
    };
    use crate::anthropic_api::ANTHROPIC_FORMAT;
    use crate::anthropic_api::types::{AnthropicContent, ContentBlock};
    use routectl_core::{
        ChatRequest, ContentPart, CoreHistoryReasoning, KnownContentPart, Message, MessageContent,
        ReasoningDetail, ReasoningDetailKind, Role,
    };
    use serde_json::{Value, json};

    /// A `reasoning.text` detail with the given format and signature.
    /// A `None` signature omits the field entirely (unsigned); an empty
    /// string sets it to `""` (also unsigned).
    fn text_detail(format: Option<&str>, signature: Option<&str>) -> ReasoningDetail {
        let mut payload = serde_json::Map::new();
        payload.insert("text".to_string(), json!("chain of thought"));
        if let Some(sig) = signature {
            payload.insert("signature".to_string(), json!(sig));
        }
        ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: None,
            format: format.map(str::to_string),
            index: Some(0),
            payload: Value::Object(payload),
        }
    }

    /// An unsigned Thinking part (signature None) -- foreign to a Claude
    /// replay, so it triggers the unsigned-thinking strip in
    /// `normalize_replay_invariants`.
    fn unsigned_thinking_part() -> ContentPart {
        ContentPart::Known(KnownContentPart::Thinking {
            thinking: "step".to_string(),
            signature: None,
        })
    }

    fn assistant(
        content: MessageContent,
        reasoning_details: Vec<ReasoningDetail>,
        tool_calls: Option<Vec<Value>>,
    ) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content,
            reasoning: None,
            reasoning_details,
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }

    fn tool_call(id: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {"name": "f", "arguments": "{}"},
        })
    }

    /// Unsigned-only reasoning, no residual content after the strip, and
    /// no tool_calls: the whole assistant turn is dropped (emitting
    /// content: [] would 400), and the new aggregated drop WARN fires.
    #[test]
    fn unsigned_only_reasoning_turn_is_dropped_with_aggregated_warn() {
        // Arrange -- Parts hold only an unsigned thinking block (triggers
        // the strip); reasoning_details are unsigned (non-emittable).
        let msg = assistant(
            MessageContent::Parts(vec![unsigned_thinking_part()]),
            vec![text_detail(Some(ANTHROPIC_FORMAT), Some(""))],
            None,
        );
        let req = ChatRequest {
            messages: vec![msg].into(),
            ..Default::default()
        };

        // Act
        let events = routectl_testkit::capture_events(|| {
            let out = normalize_replay_invariants("anthropic", &req, CoreHistoryReasoning::Auto)
                .expect("normalize must not error");
            // Assert -- the turn is gone.
            assert!(
                out.is_empty(),
                "unsigned-only reasoning turn must be dropped"
            );
        });

        // Assert -- the aggregated whole-turn drop WARN fired.
        let drop_warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN && e.field("dropped_turns").is_some())
            .expect("aggregated drop WARN must fire");
        assert_eq!(drop_warn.field("dropped_turns"), Some("1"));
    }

    /// Reasoning details in a non-anthropic format (even with a non-empty
    /// signature) are not emittable, so a reasoning-only turn with them is
    /// dropped rather than emitted as an empty content array.
    #[test]
    fn non_anthropic_format_reasoning_turn_is_dropped() {
        // Arrange
        let msg = assistant(
            MessageContent::Parts(vec![unsigned_thinking_part()]),
            vec![text_detail(
                Some("openai-responses-v1"),
                Some("sig-present"),
            )],
            None,
        );
        let req = ChatRequest {
            messages: vec![msg].into(),
            ..Default::default()
        };

        // Act
        let out = normalize_replay_invariants("anthropic", &req, CoreHistoryReasoning::Auto)
            .expect("normalize must not error");

        // Assert
        assert!(
            out.is_empty(),
            "non-anthropic-format reasoning-only turn must be dropped"
        );
    }

    /// Same non-emittable reasoning shape, but WITH tool_calls: the turn is
    /// KEPT (dropping it would orphan the next tool_result) and its wire
    /// content is non-empty (the tool_call becomes a ToolUse block).
    #[test]
    fn reasoning_only_turn_with_tool_calls_is_kept_with_non_empty_wire_content() {
        // Arrange
        let msg = assistant(
            MessageContent::Parts(vec![unsigned_thinking_part()]),
            vec![text_detail(Some(ANTHROPIC_FORMAT), Some(""))],
            Some(vec![tool_call("call_1")]),
        );
        let req = ChatRequest {
            messages: vec![msg].into(),
            ..Default::default()
        };

        // Act -- normalize keeps it, then translate to the wire shape.
        let normalized = normalize_replay_invariants("anthropic", &req, CoreHistoryReasoning::Auto)
            .expect("normalize must not error");
        assert_eq!(
            normalized.len(),
            1,
            "a turn with tool_calls is never dropped"
        );
        let wire = translate_messages("anthropic", &normalized, &mut passthrough_tally())
            .expect("translate must not error");

        // Assert -- wire content is a non-empty block array with a ToolUse.
        let AnthropicContent::Blocks(blocks) = &wire[0].content else {
            panic!("expected Blocks content");
        };
        assert!(!blocks.is_empty(), "wire content must not be empty");
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
            "tool_call must survive as a ToolUse block"
        );
    }

    /// The backstop path: a Null-content turn carrying only unsigned
    /// reasoning_details never passes through the strip rebuild (Null is
    /// skipped there), so `build_assistant_content` assembles zero blocks.
    /// The backstop inserts one empty text block and records itself in the
    /// per-attempt tally; the WARN fires on the tally's flush, not here.
    #[test]
    fn build_assistant_content_backstops_empty_blocks_with_warn() {
        // Arrange
        let msg = assistant(
            MessageContent::Null,
            vec![text_detail(Some(ANTHROPIC_FORMAT), Some(""))],
            None,
        );

        // Act
        let mut captured = None;
        let mut skips = ReasoningSkipTally::new("anthropic");
        let events = routectl_testkit::capture_events(|| {
            captured = Some(
                build_assistant_content("anthropic", 0, &msg, &mut passthrough_tally(), &mut skips)
                    .expect("build must not error"),
            );
            skips.flush();
        });

        // Assert -- exactly one empty text block, never content: [].
        let AnthropicContent::Blocks(blocks) = captured.expect("content produced") else {
            panic!("expected Blocks content");
        };
        assert_eq!(blocks.len(), 1, "backstop inserts exactly one block");
        assert!(
            matches!(&blocks[0], ContentBlock::Text { text, .. } if text.is_empty()),
            "backstop block is an empty text block"
        );

        // Assert -- the aggregated backstop WARN fired on flush.
        let backstop_warn = events.iter().find(|e| {
            e.level == tracing::Level::WARN && e.field("event") == Some("empty_content_backstop")
        });
        assert!(backstop_warn.is_some(), "backstop WARN must fire");
    }
}

#[cfg(test)]
mod tool_use_dedup_tests {
    use super::{
        AnthropicContent, ContentBlock, ReasoningSkipTally, build_assistant_content,
        build_tool_message, passthrough_tally, translate_messages,
    };
    use crate::anthropic_api::types::AnthropicMessage;
    use routectl_core::{ContentPart, KnownContentPart, Message, MessageContent, Role};
    use serde_json::{Value, json};

    /// `build_assistant_content` with the diagnostics plumbing these
    /// tool_use tests do not exercise: turn index 0 and a throwaway
    /// per-attempt tally that is never flushed (no skip category is
    /// reachable from a tool_use-only message).
    fn assistant_content_of(msg: &Message) -> AnthropicContent {
        let mut skips = ReasoningSkipTally::new("anthropic");
        build_assistant_content("anthropic", 0, msg, &mut passthrough_tally(), &mut skips)
            .expect("build must not error")
    }

    fn tool_use_part(id: &str) -> ContentPart {
        ContentPart::Known(KnownContentPart::ToolUse {
            id: id.to_string(),
            name: "lookup".to_string(),
            input: json!({"q": "sky"}),
            cache_control: None,
        })
    }

    fn tool_call(id: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {"name": "lookup", "arguments": "{\"q\":\"sky\"}"},
        })
    }

    fn assistant(content: MessageContent, tool_calls: Option<Vec<Value>>) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }

    fn tool_use_blocks(content: &AnthropicContent) -> Vec<(&String, &Value)> {
        let AnthropicContent::Blocks(blocks) = content else {
            panic!("expected Blocks content");
        };
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, input, .. } => Some((id, input)),
                _ => None,
            })
            .collect()
    }

    /// A message carrying BOTH a ToolUse content part and a tool_calls
    /// entry for the SAME id emits exactly one tool_use block -- the
    /// content-part channel wins and the tool_calls channel skips the dup.
    #[test]
    fn both_channels_same_id_emits_single_tool_use_block() {
        // Arrange
        let msg = assistant(
            MessageContent::Parts(vec![tool_use_part("t1")]),
            Some(vec![tool_call("t1")]),
        );

        // Act
        let content = assistant_content_of(&msg);

        // Assert
        let uses = tool_use_blocks(&content);
        assert_eq!(uses.len(), 1, "one tool call must emit exactly one block");
        assert_eq!(uses[0].0, "t1");
    }

    /// The ToolUse content-part channel runs sanitize_tool_id, so an
    /// OpenAI-origin id (`call.foo:1`) surfacing on the content-part
    /// channel alone lands on the same wire id the tool_calls channel
    /// would produce -- ids cannot diverge by source channel.
    #[test]
    fn tool_use_content_part_id_is_sanitized() {
        // Arrange -- only the content-part channel carries the tool call.
        let msg = assistant(
            MessageContent::Parts(vec![tool_use_part("call.foo:1")]),
            None,
        );

        // Act
        let content = assistant_content_of(&msg);

        // Assert
        let uses = tool_use_blocks(&content);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].0, "esc_call_2efoo_3a1");
    }

    /// Both channels carry the SAME logical tool call under an
    /// OpenAI-origin id. Sanitized identically on both sides, they collapse
    /// to one block -- no divergent-id double emission.
    #[test]
    fn both_channels_openai_id_sanitized_and_deduped() {
        // Arrange
        let msg = assistant(
            MessageContent::Parts(vec![tool_use_part("call.foo:1")]),
            Some(vec![tool_call("call.foo:1")]),
        );

        // Act
        let content = assistant_content_of(&msg);

        // Assert
        let uses = tool_use_blocks(&content);
        assert_eq!(uses.len(), 1, "divergent-id double emission must not occur");
        assert_eq!(uses[0].0, "esc_call_2efoo_3a1");
    }

    /// Non-overlapping ids on the two channels both survive: only matching
    /// ids are deduped.
    #[test]
    fn distinct_ids_on_each_channel_both_emit() {
        // Arrange
        let msg = assistant(
            MessageContent::Parts(vec![tool_use_part("t1")]),
            Some(vec![tool_call("t2")]),
        );

        // Act
        let content = assistant_content_of(&msg);

        // Assert
        let uses = tool_use_blocks(&content);
        assert_eq!(uses.len(), 2, "distinct tool calls both emit");
        let ids: Vec<&str> = uses.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"t1") && ids.contains(&"t2"));
    }

    /// Round-trip: an Anthropic tool_use response, normalized to canonical
    /// (which fills both the `parts` and `tool_calls` channels), then fed
    /// back as the next request's history, produces a single valid
    /// tool_use block rather than a duplicate-id body.
    #[test]
    fn anthropic_tool_use_response_round_trips_to_single_block() {
        // Arrange -- raw Anthropic response with a text + tool_use content.
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-8",
            "content": [
                {"type": "text", "text": "looking it up"},
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "sky"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let resp = crate::anthropic_api::response::normalize("test", raw).expect("normalize");
        let assistant_msg = resp.choices[0].message.clone();
        // Precondition: the parser filled both channels.
        assert!(
            assistant_msg
                .tool_calls
                .as_ref()
                .is_some_and(|tc| !tc.is_empty()),
            "response parser must fill tool_calls"
        );
        assert!(
            matches!(&assistant_msg.content, MessageContent::Parts(p)
                if p.iter().any(|part| matches!(part,
                    ContentPart::Known(KnownContentPart::ToolUse { .. })))),
            "response parser must fill a ToolUse content part"
        );

        // Act -- thread it back through the egress.
        let out = translate_messages("anthropic", &[assistant_msg], &mut passthrough_tally())
            .expect("translate");

        // Assert -- exactly one tool_use block, id preserved.
        let uses = tool_use_blocks(&out[0].content);
        assert_eq!(uses.len(), 1, "round-trip must not duplicate the tool_use");
        assert_eq!(uses[0].0, "toolu_1");
    }

    /// Two DISTINCT tool calls whose raw ids differ only by a char the
    /// former lossy sanitizer folded to `_` (`call.a` on the content-part
    /// channel, `call:a` on the tool_calls channel) are separate calls:
    /// both must be emitted, AND under DISTINCT wire ids. Deduping on the
    /// sanitized id would collapse them and silently drop one; emitting
    /// both under one wire id is itself a 400.
    #[test]
    fn distinct_raw_ids_that_formerly_sanitized_equal_emit_distinct_wire_ids() {
        // Arrange
        let msg = assistant(
            MessageContent::Parts(vec![tool_use_part("call.a")]),
            Some(vec![tool_call("call:a")]),
        );

        // Act
        let content = assistant_content_of(&msg);

        // Assert: both survive, under ids that do not collide.
        let uses = tool_use_blocks(&content);
        assert_eq!(
            uses.len(),
            2,
            "distinct raw ids must both emit, neither dropped by dedup"
        );
        assert_ne!(uses[0].0, uses[1].0, "wire ids must not collide");
    }

    fn tool_msg(content: MessageContent) -> Message {
        Message {
            refusal: None,
            role: Role::Tool,
            content,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: Some("toolu_1".to_string()),
            tool_calls: None,
        }
    }

    fn tool_result_content(m: &AnthropicMessage) -> &Value {
        let AnthropicContent::Blocks(blocks) = &m.content else {
            panic!("expected Blocks content");
        };
        blocks
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .expect("tool_result block present")
    }

    /// A Null-content tool message maps to an empty string, never JSON
    /// null (which api.anthropic.com rejects) -- mirrors the Converse
    /// ensure_min_tool_result_content guard.
    #[test]
    fn null_content_tool_result_maps_to_empty_string() {
        // Arrange / Act
        let m = build_tool_message(&[&tool_msg(MessageContent::Null)], &mut passthrough_tally());

        // Assert
        assert_eq!(tool_result_content(&m), &Value::String(String::new()));
    }

    /// An empty Parts array would serialize to `content: []`, also
    /// rejected; the guard collapses it to an empty string too.
    #[test]
    fn empty_parts_tool_result_maps_to_empty_string() {
        // Arrange / Act
        let m = build_tool_message(
            &[&tool_msg(MessageContent::Parts(Vec::new()))],
            &mut passthrough_tally(),
        );

        // Assert
        assert_eq!(tool_result_content(&m), &Value::String(String::new()));
    }

    /// Non-empty text content is untouched by the guard.
    #[test]
    fn non_empty_text_tool_result_is_preserved() {
        // Arrange / Act
        let m = build_tool_message(
            &[&tool_msg(MessageContent::Text("ok".to_string()))],
            &mut passthrough_tally(),
        );

        // Assert
        assert_eq!(tool_result_content(&m), &Value::String("ok".to_string()));
    }
}

#[cfg(test)]
mod tool_result_coalescing_tests {
    use super::{ContentBlock, passthrough_tally, translate_messages};
    use crate::anthropic_api::types::{AnthropicContent, AnthropicMessage, AnthropicRole};
    use routectl_core::{
        CacheControl, ContentPart, KnownContentPart, Message, MessageContent, Role,
    };

    fn user(text: &str) -> Message {
        plain(Role::User, MessageContent::Text(text.to_string()), None)
    }

    fn assistant(text: &str) -> Message {
        plain(
            Role::Assistant,
            MessageContent::Text(text.to_string()),
            None,
        )
    }

    fn tool_result(id: &str, content: MessageContent) -> Message {
        plain(Role::Tool, content, Some(id.to_string()))
    }

    fn plain(role: Role, content: MessageContent, tool_call_id: Option<String>) -> Message {
        Message {
            refusal: None,
            role,
            content,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id,
            tool_calls: None,
        }
    }

    fn text(s: &str) -> MessageContent {
        MessageContent::Text(s.to_string())
    }

    fn roles(out: &[AnthropicMessage]) -> Vec<&'static str> {
        out.iter()
            .map(|m| match m.role {
                AnthropicRole::User => "user",
                AnthropicRole::Assistant => "assistant",
            })
            .collect()
    }

    fn tool_result_ids(m: &AnthropicMessage) -> Vec<&str> {
        let AnthropicContent::Blocks(blocks) = &m.content else {
            panic!("expected Blocks content");
        };
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Three parallel tool results fold into ONE user message carrying
    /// three tool_result blocks in submission order, each keeping its own
    /// tool_use_id. A strict Anthropic-compatible gateway rejects the
    /// consecutive user turns the unmerged shape would emit.
    #[test]
    fn parallel_tool_results_coalesce_into_one_user_message() {
        // Arrange
        let messages = vec![
            user("weather?"),
            assistant("looking it up"),
            tool_result("call_1", text("sunny")),
            tool_result("call_2", text("noon")),
            tool_result("call_3", text("72F")),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translate");

        // Assert
        assert_eq!(roles(&out), vec!["user", "assistant", "user"]);
        assert_eq!(
            tool_result_ids(&out[2]),
            vec!["call_1", "call_2", "call_3"],
            "every tool_use_id must survive in submission order"
        );
    }

    /// A lone tool result still emits exactly one user message with one
    /// tool_result block -- the merge must not change the single-result
    /// wire shape.
    #[test]
    fn single_tool_result_emits_one_block_unchanged() {
        // Arrange
        let messages = vec![
            user("weather?"),
            assistant("looking it up"),
            tool_result("call_1", text("sunny")),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translate");

        // Assert
        assert_eq!(roles(&out), vec!["user", "assistant", "user"]);
        assert_eq!(tool_result_ids(&out[2]), vec!["call_1"]);
    }

    /// An intervening assistant turn ends the run: two tool-result groups
    /// separated by an assistant turn stay two user messages, and no
    /// result migrates across the boundary.
    #[test]
    fn tool_results_separated_by_assistant_turn_stay_separate_messages() {
        // Arrange
        let messages = vec![
            user("weather?"),
            assistant("first call"),
            tool_result("call_1", text("sunny")),
            assistant("second call"),
            tool_result("call_2", text("noon")),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translate");

        // Assert
        assert_eq!(
            roles(&out),
            vec!["user", "assistant", "user", "assistant", "user"]
        );
        assert_eq!(tool_result_ids(&out[2]), vec!["call_1"]);
        assert_eq!(tool_result_ids(&out[4]), vec!["call_2"]);
    }

    /// A user turn between two tool results is likewise a run boundary --
    /// coalescing never reorders a tool result past a caller turn.
    #[test]
    fn tool_results_separated_by_user_turn_stay_separate_messages() {
        // Arrange
        let messages = vec![
            assistant("call one"),
            tool_result("call_1", text("sunny")),
            user("and the time?"),
            tool_result("call_2", text("noon")),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translate");

        // Assert
        assert_eq!(roles(&out), vec!["assistant", "user", "user", "user"]);
        assert_eq!(tool_result_ids(&out[1]), vec!["call_1"]);
        assert_eq!(tool_result_ids(&out[3]), vec!["call_2"]);
    }

    /// Per-block metadata on a coalesced tool result's nested content
    /// survives the merge: a cache_control marker on a Part inside the
    /// second of two results still ships.
    #[test]
    fn nested_block_cache_control_survives_coalescing() {
        // Arrange
        let marked = MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
            text: "cached output".to_string(),
            cache_control: Some(CacheControl::ephemeral_5m()),
            citations: None,
        })]);
        let messages = vec![
            assistant("two calls"),
            tool_result("call_1", text("sunny")),
            tool_result("call_2", marked),
        ];

        // Act
        let out = translate_messages("anthropic", &messages, &mut passthrough_tally())
            .expect("translate");

        // Assert
        let AnthropicContent::Blocks(blocks) = &out[1].content else {
            panic!("expected Blocks content");
        };
        assert_eq!(blocks.len(), 2, "both results must ride one message");
        let ContentBlock::ToolResult { content, .. } = &blocks[1] else {
            panic!("expected a tool_result block");
        };
        assert!(
            content[0]["cache_control"].is_object(),
            "nested per-block cache_control must survive, got: {content}"
        );
    }
}

#[cfg(test)]
#[path = "messages_tests.rs"]
mod sidecar_tests;
