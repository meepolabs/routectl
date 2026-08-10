//! Canonical `messages[]` -> Converse `messages[]` translation.
//!
//! Per-role dispatch: User and Assistant turns ride through
//! `build_content_blocks`; Role::System is dropped here (lifted into
//! the top-level `system` array by `system.rs`); Role::Tool becomes a
//! synthesized user-role message carrying a `toolResult` block.
//!
//! Forward-compat catchalls: `ContentPart::Other` re-wraps as the AWS
//! single-key union and passes through, so an unmodeled block preserved
//! on a prior response turn replays losslessly. Unsupported known parts
//! (e.g. `Document` when the canonical title/source can't be mapped, or
//! a URL-shape image the JSON wire can't carry) drop with a tracing
//! diagnostic; the caller sees a partial body rather than a translation
//! failure. Cache breakpoints survive as sibling `{cachePoint}` entries.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use serde_json::{Value, json};

use routectl_core::{
    ContentPart, Error, KnownContentPart, Message, MessageContent, ReasoningDetail,
    ReasoningDetailKind, Result, Role,
};

use crate::anthropic_api::parts::strip_text_after_tool_use;
use crate::bounded_diagnostics::BoundedLogSample;

use super::types::{
    CachePoint, ConverseCitationsConfig, ConverseContentBlock, ConverseDocument,
    ConverseDocumentSource, ConverseImage, ConverseImageSource, ConverseMessage,
    ConverseRequestReasoningBlock, ConverseRequestReasoningText, ConverseToolResult,
    ConverseToolResultContent, ConverseToolUse,
};

/// Translate every message in `req.messages` into a `ConverseMessage`,
/// dropping Role::System (handled by the top-level `system` array) and
/// rejecting Role::Tool messages without a `tool_call_id` (AWS rejects
/// empty `toolUseId` with a 400). Messages whose translated content
/// vec is empty (canonical Null content, or every typed Part dropped
/// during translation) are skipped entirely -- AWS Converse rejects
/// `content: []` with "Member must have at least 1 element."
///
/// Adjacent turns that translate to the same Converse role are coalesced
/// into one message (see `push_or_coalesce`): parallel tool results each
/// synthesize a user turn, and Converse 400s on consecutive same-role
/// turns.
///
/// A `CitationsDropTally` threads through every document-bearing path so
/// malformed `citations` values across the whole request collapse into one
/// aggregated WARN instead of one per document (see
/// `translate_document_citations`). A `ReasoningSkipTally` threads through
/// the assistant path for the same reason, collapsing unsigned-reasoning
/// skips across every turn into one WARN.
pub(super) fn build_messages(id: &str, messages: &[Message]) -> Result<Vec<ConverseMessage>> {
    let mut tally = CitationsDropTally::new(id);
    let mut reasoning = ReasoningSkipTally::new(id);
    let translated = translate_messages(id, messages, &mut tally, &mut reasoning);
    // Flush on both arms: a request that records a drop and only then
    // hits a translation error still owes the operator its aggregate WARN.
    tally.flush();
    reasoning.flush();
    translated
}

fn translate_messages(
    id: &str,
    messages: &[Message],
    tally: &mut CitationsDropTally<'_>,
    reasoning: &mut ReasoningSkipTally<'_>,
) -> Result<Vec<ConverseMessage>> {
    let mut out: Vec<ConverseMessage> = Vec::with_capacity(messages.len());
    for (i, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::System => {
                // System lives in the top-level `system` array. Drop here
                // so we don't duplicate; direct callers without an
                // ingress have already had their System messages lifted
                // by `build_system` via `lift_legacy_system`.
            }
            Role::User => {
                let mut blocks = build_user_content_blocks(id, &msg.content, tally)?;
                ensure_document_has_text_sibling(&mut blocks);
                if blocks.is_empty() {
                    tracing::debug!(
                        provider = id,
                        role = "user",
                        "skipping empty message after translation"
                    );
                    continue;
                }
                push_or_coalesce(&mut out, "user", blocks);
            }
            Role::Assistant => {
                let blocks = build_assistant_content_blocks(id, i, msg, tally, reasoning)?;
                if blocks.is_empty() {
                    tracing::debug!(
                        provider = id,
                        role = "assistant",
                        "skipping empty message after translation"
                    );
                    continue;
                }
                push_or_coalesce(&mut out, "assistant", blocks);
            }
            Role::Tool => {
                let tool_msg = build_tool_message(msg, tally)?;
                push_or_coalesce(&mut out, "user", tool_msg.content);
            }
        }
    }
    Ok(out)
}

/// Append a translated turn's content to `out`, merging into the
/// previous message when it carries the same role. AWS Converse requires
/// strict user/assistant alternation and 400s on consecutive same-role
/// turns; N parallel tool results (each a synthesized user turn) would
/// otherwise emit N consecutive user messages. Merging preserves block
/// order and each `toolResult` block's tool-use correlation id.
fn push_or_coalesce(
    out: &mut Vec<ConverseMessage>,
    role: &str,
    mut blocks: Vec<ConverseContentBlock>,
) {
    if let Some(last) = out.last_mut()
        && last.role == role
    {
        last.content.append(&mut blocks);
        return;
    }
    out.push(ConverseMessage {
        role: role.to_string(),
        content: blocks,
    });
}

/// AWS Converse requires a companion `{text}` block in any message
/// that includes a `{document}` block. When translation produces a
/// Document without a sibling Text, prepend an empty-string Text so
/// AWS accepts the shape. Forward-compat over rejection: a caller
/// that doesn't know about this constraint gets their document
/// shipped instead of a confusing local 400.
fn ensure_document_has_text_sibling(blocks: &mut Vec<ConverseContentBlock>) {
    let has_document = blocks
        .iter()
        .any(|b| matches!(b, ConverseContentBlock::Document { .. }));
    if !has_document {
        return;
    }
    let has_text = blocks
        .iter()
        .any(|b| matches!(b, ConverseContentBlock::Text { .. }));
    if !has_text {
        blocks.insert(
            0,
            ConverseContentBlock::Text {
                text: String::new(),
            },
        );
    }
}

/// User-role content. Plain text -> `[{text}]`; null -> empty (AWS
/// rejects `[]` so the caller will skip the message if necessary, but
/// most user turns carry text); typed parts -> per-block translation
/// with cache_point interleave.
fn build_user_content_blocks(
    id: &str,
    content: &MessageContent,
    tally: &mut CitationsDropTally<'_>,
) -> Result<Vec<ConverseContentBlock>> {
    content_blocks_with_cache_control(id, content, tally)
}

/// Assistant-role content with text-after-tool_use cleanup. Bedrock and
/// Anthropic both reject `[Text, ToolUse, Text]` shape echoed on a
/// multi-turn replay (the trailing transition Text after the last
/// ToolUse). Mirrors `anthropic_api::messages::append_assistant_message_blocks`
/// behavior so the Converse path doesn't silently 400 upstream.
///
/// When `msg.reasoning_details` is non-empty (canonical multi-turn
/// channel populated by the streaming decoder), emit Converse
/// `ReasoningContent` blocks first (only `anthropic-claude-v1` format),
/// then append the remaining content. Mirrors the Anthropic-API egress
/// `emit_reasoning_blocks` + `append_assistant_message_blocks` split.
/// The two sources (`reasoning_details` vs `KnownContentPart::Thinking`
/// in `content.Parts`) are mutually exclusive by design: the streaming
/// decoder puts thinking into `reasoning_details`, not `content.Parts`.
///
/// After the content blocks are built, any OpenAI-shape
/// `Message.tool_calls` (populated by the OpenAI ingress instead of
/// `KnownContentPart::ToolUse` content parts) are re-emitted as Converse
/// `toolUse` blocks via `append_tool_use_blocks_from_calls`, so the turn
/// is no longer empty/skipped and the toolUse precedes the next turn's
/// toolResult.
fn build_assistant_content_blocks(
    id: &str,
    message_index: usize,
    msg: &Message,
    tally: &mut CitationsDropTally<'_>,
    reasoning: &mut ReasoningSkipTally<'_>,
) -> Result<Vec<ConverseContentBlock>> {
    let mut blocks = if !msg.reasoning_details.is_empty() {
        let mut blocks =
            emit_reasoning_blocks_converse(message_index, &msg.reasoning_details, reasoning)?;
        append_converse_content_blocks(id, &msg.content, &mut blocks, tally)?;
        blocks
    } else if let MessageContent::Parts(parts) = &msg.content {
        let cleaned = strip_text_after_tool_use(parts);
        content_blocks_from_parts(id, &cleaned, tally)?
    } else {
        content_blocks_with_cache_control(id, &msg.content, tally)?
    };
    append_tool_use_blocks_from_calls(id, msg, &mut blocks);
    Ok(blocks)
}

/// Re-emit OpenAI-shape `Message.tool_calls` as Converse `toolUse`
/// blocks. The OpenAI ingress populates `tool_calls` rather than emitting
/// `KnownContentPart::ToolUse` content parts; without this re-emission an
/// assistant turn whose calls live only on `tool_calls` produces no
/// `toolUse` block, the message is skipped as empty, and the following
/// `toolResult` turn is orphaned ("tool_use ids ... without preceding
/// tool_use blocks").
///
/// The guard skips re-emission when the content already carries
/// `ToolUse` parts: a caller that put ToolUse in content already got
/// those blocks via `content_blocks_from_parts`, and re-emitting from
/// `tool_calls` would double the toolUse blocks.
fn append_tool_use_blocks_from_calls(
    id: &str,
    msg: &Message,
    blocks: &mut Vec<ConverseContentBlock>,
) {
    let Some(tool_calls) = msg.tool_calls.as_ref().filter(|tc| !tc.is_empty()) else {
        return;
    };
    if message_content_has_tool_use(&msg.content) {
        return;
    }
    for call in crate::tool_calls::normalize_tool_calls(id, tool_calls) {
        blocks.push(ConverseContentBlock::ToolUse {
            tool_use: ConverseToolUse {
                tool_use_id: call.id,
                name: call.name,
                input: call.arguments,
            },
        });
    }
}

/// True iff the assistant content already carries a `ToolUse` part. Used
/// to avoid double-emitting tool-use blocks when both `msg.tool_calls`
/// and content `ToolUse` parts are set on the same turn.
fn message_content_has_tool_use(content: &MessageContent) -> bool {
    let MessageContent::Parts(parts) = content else {
        return false;
    };
    parts
        .iter()
        .any(|p| matches!(p, ContentPart::Known(KnownContentPart::ToolUse { .. })))
}

/// Translate `reasoning_details` into Bedrock Converse `ReasoningContent`
/// blocks for echo on a multi-turn assistant turn. Index-ordered so an
/// upstream that re-orders reasoning blocks doesn't surprise the
/// downstream signature check. Only `anthropic-claude-v1` format details
/// are emitted; others (e.g. OpenAI-format) are skipped -- they have no
/// Converse wire equivalent. Bedrock validates the signature on multi-turn
/// replay identical to direct Anthropic; a missing signature 400s with
/// "invalid reasoning content". Unsigned blocks are skipped and recorded on
/// the `ReasoningSkipTally`, which aggregates every turn's skips into a
/// single per-request WARN so the operator can correlate without per-detail
/// log spam. The provider id rides on the tally, which owns the WARN.
fn emit_reasoning_blocks_converse(
    message_index: usize,
    details: &[ReasoningDetail],
    reasoning: &mut ReasoningSkipTally<'_>,
) -> Result<Vec<ConverseContentBlock>> {
    let mut sorted = details.to_vec();
    sorted.sort_by_key(|d| d.index.unwrap_or(0));

    let mut blocks: Vec<ConverseContentBlock> = Vec::with_capacity(sorted.len());
    for detail in &sorted {
        match detail.kind {
            ReasoningDetailKind::Text => {
                if detail.format.as_deref() != Some(crate::anthropic_api::ANTHROPIC_FORMAT) {
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
                    .unwrap_or("");
                if signature.is_empty() {
                    // Bedrock Converse validates the signature on multi-turn
                    // replay and 400s without it. Skip the block so replay
                    // doesn't fail on a guaranteed-bad echo; aggregate the
                    // WARN to avoid per-detail log spam.
                    reasoning.record_unsigned(message_index, detail.index);
                    continue;
                }
                blocks.push(ConverseContentBlock::ReasoningContent {
                    reasoning_content: ConverseRequestReasoningBlock::ReasoningText {
                        reasoning_text: ConverseRequestReasoningText {
                            text: thinking,
                            signature: Some(signature.to_string()),
                        },
                    },
                });
            }
            ReasoningDetailKind::Encrypted => {
                if detail.format.as_deref() != Some(crate::anthropic_api::ANTHROPIC_FORMAT) {
                    continue;
                }
                let data = detail
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                blocks.push(ConverseContentBlock::ReasoningContent {
                    reasoning_content: ConverseRequestReasoningBlock::RedactedContent {
                        redacted_content: data,
                    },
                });
            }
            ReasoningDetailKind::Summary => {
                // Not a Bedrock Converse block type; skip.
            }
        }
    }
    Ok(blocks)
}

/// Per-request tally of reasoning details skipped on the Converse egress
/// because their signature was missing or empty. Threaded through the
/// assistant path from `build_messages` so a history with several unsigned
/// turns emits ONE WARN instead of one per turn. Mirrors
/// `anthropic_api::messages::ReasoningSkipTally`, minus its foreign-format
/// category: a non-`anthropic-claude-v1` detail has no Converse wire
/// equivalent and drops silently here.
///
/// `skipped_count` and `turns_affected` are exact; `skipped_locations` is a
/// bounded SAMPLE and its `truncated()` flag -- never a count comparison --
/// says whether anything was dropped from it. Each location is
/// `(message_index, detail_index)`: every message's `reasoning_details`
/// carries its own index space, so a bare detail index pooled across turns
/// would render identically for two unrelated details and the operator
/// could not tell a contiguous tail from a scattered set. The detail slot
/// stays `Option<u32>` so an index the upstream never supplied reads as
/// `None` rather than as a plausible 0.
struct ReasoningSkipTally<'a> {
    provider: &'a str,
    skipped_count: usize,
    turns_affected: usize,
    last_turn: Option<usize>,
    skipped_locations: BoundedLogSample<(usize, Option<u32>)>,
}

impl<'a> ReasoningSkipTally<'a> {
    fn new(provider: &'a str) -> Self {
        Self {
            provider,
            skipped_count: 0,
            turns_affected: 0,
            last_turn: None,
            skipped_locations: BoundedLogSample::new(),
        }
    }

    /// Record one unsigned skip at `message_index`. Turns are visited in
    /// order and every skip within a turn arrives consecutively, so a
    /// change of `message_index` is a new affected turn.
    fn record_unsigned(&mut self, message_index: usize, detail_index: Option<u32>) {
        self.skipped_count = self.skipped_count.saturating_add(1);
        if self.last_turn != Some(message_index) {
            self.turns_affected = self.turns_affected.saturating_add(1);
            self.last_turn = Some(message_index);
        }
        self.skipped_locations.push((message_index, detail_index));
    }

    /// Emit the aggregated WARN, if anything was skipped. Called once per
    /// request from `build_messages`, on both the Ok and the Err arm.
    fn flush(&self) {
        if self.skipped_count > 0 {
            tracing::warn!(
                provider = self.provider,
                skipped_count = self.skipped_count,
                turns_affected = self.turns_affected,
                skipped_locations = ?self.skipped_locations.items(),
                skipped_locations_truncated = self.skipped_locations.truncated(),
                "skipping Thinking blocks on Converse replay: signature missing or empty; \
                 Bedrock Converse requires a signature on replayed reasoningContent blocks"
            );
        }
    }
}

/// Append the assistant message's text/parts content AFTER the reasoning
/// blocks already pushed. Mirrors
/// `anthropic_api::messages::append_assistant_message_blocks`. For Text,
/// emits a single Text block (skipped on empty/Null since reasoning-only
/// assistant turns are valid). For Parts, translates each block after
/// stripping trailing text-after-tool_use.
fn append_converse_content_blocks(
    id: &str,
    content: &MessageContent,
    blocks: &mut Vec<ConverseContentBlock>,
    tally: &mut CitationsDropTally<'_>,
) -> Result<()> {
    match content {
        MessageContent::Text(t) if !t.is_empty() => {
            blocks.push(ConverseContentBlock::Text { text: t.clone() });
        }
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            let more = content_blocks_from_parts(id, &cleaned, tally)?;
            blocks.extend(more);
        }
    }
    Ok(())
}

fn content_blocks_with_cache_control(
    id: &str,
    content: &MessageContent,
    tally: &mut CitationsDropTally<'_>,
) -> Result<Vec<ConverseContentBlock>> {
    match content {
        MessageContent::Text(t) => Ok(vec![ConverseContentBlock::Text { text: t.clone() }]),
        MessageContent::Null => Ok(Vec::new()),
        MessageContent::Parts(parts) => content_blocks_from_parts(id, parts, tally),
    }
}

/// Walk a slice of canonical `ContentPart` into Converse blocks. When a
/// part translates successfully AND carries a `cache_control` marker, a
/// sibling `{cachePoint}` block is emitted IMMEDIATELY AFTER the
/// translated block (avoids the orphan-cachePoint shape that AWS
/// rejects when a translation drops the underlying block).
fn content_blocks_from_parts(
    id: &str,
    parts: &[ContentPart],
    tally: &mut CitationsDropTally<'_>,
) -> Result<Vec<ConverseContentBlock>> {
    let mut out: Vec<ConverseContentBlock> = Vec::with_capacity(parts.len());
    for p in parts {
        if let Some(block) = translate_content_part(id, p, tally)? {
            let cc = p.cache_control().cloned();
            out.push(block);
            if let Some(cc) = cc {
                out.push(ConverseContentBlock::CachePoint {
                    cache_point: CachePoint::default_with_ttl(Some(cc.effective_ttl().to_string())),
                });
            }
        }
        // A translation that returns Ok(None) deliberately drops the
        // block (e.g. unmodellable image_url on the Converse wire). We
        // must NOT emit an orphan cachePoint for it -- AWS rejects a
        // cachePoint without a preceding content block.
    }
    Ok(out)
}

/// Translate one canonical ContentPart -> Converse content block.
/// Returns Ok(None) when the block has no Converse equivalent and is
/// dropped (with a tracing diagnostic). Returns Err only on hard
/// translation failures (e.g. thinking block without a signature, which
/// would 400 AWS on multi-turn replay).
fn translate_content_part(
    id: &str,
    p: &ContentPart,
    tally: &mut CitationsDropTally<'_>,
) -> Result<Option<ConverseContentBlock>> {
    match p {
        ContentPart::Known(k) => translate_known_part(id, k, tally),
        // Re-wrap the catchall as the AWS single-key union -- the exact
        // inverse of the response decoder's tag/extras split -- so an
        // unmodeled Converse block preserved on a prior response turn
        // replays losslessly next turn instead of being silently
        // deleted. cache_control is handled by the caller
        // (`content_blocks_from_parts` emits the sibling cachePoint), so
        // it stays out of the raw union payload.
        ContentPart::Other {
            type_tag, extras, ..
        } => {
            let mut wrapper = serde_json::Map::new();
            wrapper.insert(type_tag.clone(), Value::Object(extras.clone()));
            tracing::debug!(
                provider = id,
                type_tag = %type_tag,
                "passing ContentPart::Other through Converse egress as single-key union"
            );
            Ok(Some(ConverseContentBlock::Other(Value::Object(wrapper))))
        }
    }
}

fn translate_known_part(
    id: &str,
    k: &KnownContentPart,
    tally: &mut CitationsDropTally<'_>,
) -> Result<Option<ConverseContentBlock>> {
    match k {
        KnownContentPart::Text { text, .. } => {
            Ok(Some(ConverseContentBlock::Text { text: text.clone() }))
        }
        KnownContentPart::Image { source, .. } => Ok(translate_image_source(id, source)),
        KnownContentPart::ImageUrl { image_url, .. } => Ok(translate_image_url(id, image_url)),
        KnownContentPart::Document {
            source,
            title,
            citations,
            ..
        } => Ok(translate_document(
            id,
            source,
            title.as_deref(),
            citations.as_ref(),
            tally,
        )),
        // OpenAI-shape file part. Reuse the document translator by first
        // rewriting the base64 `file_data` data URI into the canonical
        // Anthropic document source shape. Non-translatable shapes
        // (file_id-only reference, non-base64 file_data, unmapped media
        // type) drop with a WARN -- the JSON Converse wire cannot carry a
        // raw OpenAI file block, so passthrough is not an option here
        // (mirrors how `translate_image_url` drops unsupported refs).
        KnownContentPart::File { file, .. } => {
            if let Some((source, title)) = file_data_to_document_source(file) {
                // An OpenAI-shape file part has no citations carrier.
                Ok(translate_document(
                    id,
                    &source,
                    title.as_deref(),
                    None,
                    tally,
                ))
            } else {
                tracing::warn!(
                    provider = id,
                    "dropping file part on Converse egress; only base64 PDF data URIs are supported"
                );
                Ok(None)
            }
        }
        KnownContentPart::ToolUse {
            id: tu_id,
            name,
            input,
            ..
        } => Ok(Some(ConverseContentBlock::ToolUse {
            tool_use: ConverseToolUse {
                tool_use_id: crate::tool_id::sanitize_tool_id(tu_id).into_owned(),
                name: name.clone(),
                input: input.clone(),
            },
        })),
        KnownContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            let mut result_content = translate_tool_result_content(content, tally);
            ensure_min_tool_result_content(&mut result_content);
            Ok(Some(ConverseContentBlock::ToolResult {
                tool_result: ConverseToolResult {
                    tool_use_id: crate::tool_id::sanitize_tool_id(tool_use_id).into_owned(),
                    content: result_content,
                    status: is_error.map(|e| if e { "error".into() } else { "success".into() }),
                },
            }))
        }
        KnownContentPart::Thinking {
            thinking,
            signature,
        } => {
            // Multi-turn replay against thinking-enabled Claude on
            // Converse REQUIRES the signature -- AWS validates that
            // each `reasoningText` block carries the upstream-supplied
            // signature, and 400s with a confusing
            // "validation: invalid reasoning content" otherwise.
            // Surface the missing signature locally so the operator
            // sees the precise field to fix instead of a vague AWS
            // error on the second turn.
            let Some(sig) = signature.as_ref().filter(|s| !s.is_empty()).cloned() else {
                return Err(Error::normalize_request(
                    id,
                    "thinking block on Converse egress missing signature; \
                     cannot replay (Anthropic/Bedrock requires the \
                     upstream-supplied signature on every reasoningContent \
                     block in a multi-turn request)",
                ));
            };
            Ok(Some(ConverseContentBlock::ReasoningContent {
                reasoning_content: ConverseRequestReasoningBlock::ReasoningText {
                    reasoning_text: ConverseRequestReasoningText {
                        text: thinking.clone(),
                        signature: Some(sig),
                    },
                },
            }))
        }
        KnownContentPart::RedactedThinking { data } => {
            // Pass-through verbatim: canonical schema already holds the
            // base64 string, AWS expects a base64 string. AWS accepts
            // empty/short strings here so no validation needed.
            Ok(Some(ConverseContentBlock::ReasoningContent {
                reasoning_content: ConverseRequestReasoningBlock::RedactedContent {
                    redacted_content: data.clone(),
                },
            }))
        }
    }
}

/// Convert a canonical Anthropic-shape image `source` (`{type: "base64",
/// media_type, data}`) into a `ConverseContentBlock::Image`. Returns
/// None for URL-shape sources or unknown formats.
fn translate_image_source(id: &str, source: &Value) -> Option<ConverseContentBlock> {
    let Some(obj) = source.as_object() else {
        tracing::warn!(
            provider = id,
            "dropping image with non-object source on Converse egress"
        );
        return None;
    };
    let Some(kind) = obj.get("type").and_then(|v| v.as_str()) else {
        tracing::warn!(
            provider = id,
            "dropping image source missing `type` on Converse egress"
        );
        return None;
    };
    if kind != "base64" {
        tracing::warn!(
            provider = id,
            source_type = %kind,
            "dropping non-base64 image source on Converse egress"
        );
        return None;
    }
    let media_type = obj.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
    let data = obj.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let Some(format) = media_type_to_image_format(media_type) else {
        tracing::warn!(
            provider = id,
            media_type = %media_type,
            "dropping image with unmapped media_type on Converse egress"
        );
        return None;
    };
    Some(ConverseContentBlock::Image {
        image: ConverseImage {
            format,
            source: ConverseImageSource {
                bytes: data.to_string(),
            },
        },
    })
}

/// Convert an OpenAI-shape `image_url.url` data URI into a Converse
/// Image block. Non-data-URI image refs (https://...) cannot ride the
/// JSON Converse wire; drop with a WARN.
fn translate_image_url(id: &str, image_url: &Value) -> Option<ConverseContentBlock> {
    let url = image_url.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((mt, b64)) = rest.split_once(";base64,")
        && let Some(format) = media_type_to_image_format(mt)
    {
        return Some(ConverseContentBlock::Image {
            image: ConverseImage {
                format,
                source: ConverseImageSource {
                    bytes: b64.to_string(),
                },
            },
        });
    }
    tracing::warn!(
        provider = id,
        "dropping image_url on Converse egress; only base64 data URIs are supported"
    );
    None
}

/// Rewrite an OpenAI-shape `file` part (`{filename, file_data}`) into the
/// canonical Anthropic document `source` plus an optional title, so the
/// Converse `translate_document` path can consume it. Returns None for
/// every shape that has no inline base64 PDF bytes to translate (the
/// `file_id`-only reference form, a non-`data:...;base64,` `file_data`,
/// an empty payload, or a non-PDF media type).
///
/// Scope is application/pdf only, matching the Anthropic egress helper
/// (`anthropic_api::parts::parse_file_document_source`). Keeping both
/// egresses PDF-only avoids a surprising asymmetry where the same
/// canonical `file` part would ship as a document on one backend and be
/// dropped on the other depending on which target a fallback chain picks.
/// Widening to other Converse document formats (txt, csv, ...) would be a
/// deliberate, symmetric change across both egresses, not an accident of
/// the downstream format table.
fn file_data_to_document_source(file: &Value) -> Option<(Value, Option<String>)> {
    let file_data = file.get("file_data").and_then(|v| v.as_str())?;
    let rest = file_data.strip_prefix("data:")?;
    let (mt_with_params, b64) = rest.split_once(";base64,")?;
    if b64.is_empty() {
        return None;
    }
    let raw_media_type = mt_with_params.split(';').next().unwrap_or(mt_with_params);
    let media_type_lc = raw_media_type.to_ascii_lowercase();
    if media_type_lc != "application/pdf" {
        return None;
    }
    let title = file
        .get("filename")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let source = json!({
        "type": "base64",
        "media_type": media_type_lc,
        "data": b64,
    });
    Some((source, title))
}

fn media_type_to_image_format(mt: &str) -> Option<String> {
    match mt.to_ascii_lowercase().as_str() {
        "image/png" => Some("png".to_string()),
        "image/jpeg" | "image/jpg" => Some("jpeg".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// Translate canonical `Document` part to AWS `{document: ...}` block.
/// Canonical `source` shape (Anthropic-style):
///   - `{type: "base64", media_type: "application/pdf", data: "<b64>"}`
///   - `{type: "text", media_type: "text/plain", data: "..."}` (plain
///     text body; AWS doesn't require base64 for text formats but we
///     normalize to base64 for one-shape simplicity).
///
/// Returns None when the source shape is unrecognized (URL refs aren't
/// supported on the JSON Converse wire) or the media type doesn't map
/// to an AWS-validated `format` value.
fn translate_document(
    id: &str,
    source: &Value,
    title: Option<&str>,
    citations: Option<&Value>,
    tally: &mut CitationsDropTally<'_>,
) -> Option<ConverseContentBlock> {
    let Some(obj) = source.as_object() else {
        tracing::warn!(
            provider = id,
            "dropping document with non-object source on Converse egress"
        );
        return None;
    };
    let Some(kind) = obj.get("type").and_then(|v| v.as_str()) else {
        tracing::warn!(
            provider = id,
            "dropping document source missing `type` on Converse egress"
        );
        return None;
    };
    let media_type = obj.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
    let raw_data = obj.get("data").and_then(|v| v.as_str()).unwrap_or("");
    // AWS Converse's JSON wire only accepts base64-encoded source bytes.
    // A canonical text-source document carries a plain UTF-8 body, so we
    // base64-encode it here -- a valid Anthropic shape would otherwise be
    // dropped rather than forwarded to the model.
    let Some(bytes) = normalize_document_source_bytes(kind, raw_data) else {
        tracing::warn!(
            provider = id,
            source_type = %kind,
            "dropping unsupported document source type on Converse egress; \
             AWS Converse JSON wire accepts only base64 or text sources"
        );
        return None;
    };
    let Some(format) = media_type_to_document_format(media_type) else {
        tracing::warn!(
            provider = id,
            media_type = %media_type,
            "dropping document with unmapped media_type on Converse egress"
        );
        return None;
    };
    let name = sanitize_document_name(title);
    Some(ConverseContentBlock::Document {
        document: ConverseDocument {
            format,
            name,
            source: ConverseDocumentSource { bytes },
            citations: translate_document_citations(citations, tally)
                .map(|enabled| ConverseCitationsConfig { enabled }),
        },
    })
}

/// Lift a canonical document `citations` value onto the AWS
/// `CitationsConfig` shape. The canonical field is an opaque `Value`
/// carrying Anthropic's `{enabled: bool}` object verbatim, and AWS's
/// `CitationsConfig` has exactly the same single member, so this is a bool
/// lift rather than a structural translation.
///
/// Returns `Some(true)` only for an explicit `{"enabled": true}`. An
/// absent value, or an explicit `false`, returns None so the optional
/// member is omitted -- `{enabled: false}` is indistinguishable from
/// absence in behavior and only adds wire noise. Any other shape returns
/// None and is counted on `tally`: the canonical field is opaque because
/// ingresses forward it verbatim, so guessing an interpretation would
/// silently invent a citation setting the caller never asked for.
///
/// The per-document event `tally.record` emits is DEBUG and the
/// operator-facing loss is the single aggregated WARN `tally` emits at end
/// of request. A request may
/// carry an unbounded number of document elements (a raw tool_result
/// content array is an opaque `Value`), so a per-document WARN is a
/// log-volume amplifier driven by request content.
fn translate_document_citations(
    citations: Option<&Value>,
    tally: &mut CitationsDropTally<'_>,
) -> Option<bool> {
    let citations = citations?;
    match citations.as_object().and_then(|o| o.get("enabled")) {
        Some(Value::Bool(true)) => Some(true),
        Some(Value::Bool(false)) => None,
        _ => {
            tally.record();
            None
        }
    }
}

/// Per-request counter of documents whose `citations` value was
/// unrecognized and therefore dropped. Threaded through every
/// document-bearing translation path (message content, canonical
/// tool_result Parts, and the raw Anthropic-shape tool_result content
/// array) so one request emits at most one WARN regardless of how many
/// documents it carries. Carries the provider id so both the per-document
/// DEBUG event and the aggregate WARN are attributable on a request that
/// fans out over more than one provider.
struct CitationsDropTally<'a> {
    provider: &'a str,
    dropped: usize,
}

impl<'a> CitationsDropTally<'a> {
    const fn new(provider: &'a str) -> Self {
        Self {
            provider,
            dropped: 0,
        }
    }

    fn record(&mut self) {
        self.dropped += 1;
        tracing::debug!(
            provider = self.provider,
            "dropping unrecognized document citations value on Converse egress; \
             expected an object with a boolean `enabled` member"
        );
    }

    /// Emit the aggregated WARN, if anything was dropped. Called once per
    /// request from `build_messages`.
    fn flush(&self) {
        if self.dropped > 0 {
            tracing::warn!(
                provider = self.provider,
                dropped_count = self.dropped,
                "dropping unrecognized document citations value on Converse egress; \
                 expected an object with a boolean `enabled` member"
            );
        }
    }
}

/// AWS Converse document `format` allowlist. Mirrors AWS docs as of
/// 2026-05-10; any new entries here must also be valid on the wire or
/// upstream rejects the request.
fn media_type_to_document_format(mt: &str) -> Option<String> {
    match mt.to_ascii_lowercase().as_str() {
        "application/pdf" => Some("pdf".to_string()),
        "text/csv" => Some("csv".to_string()),
        "application/msword" => Some("doc".to_string()),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Some("docx".to_string())
        }
        "application/vnd.ms-excel" => Some("xls".to_string()),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            Some("xlsx".to_string())
        }
        "text/html" => Some("html".to_string()),
        "text/plain" => Some("txt".to_string()),
        "text/markdown" => Some("md".to_string()),
        _ => None,
    }
}

/// Map a canonical document `title` to a name AWS Converse accepts.
///
/// AWS expresses the `document.name` charset as prose, not a pattern:
/// "The name can only contain the following characters: Alphanumeric
/// characters; Whitespace characters (no more than one in a row); Hyphens;
/// Parentheses; Square brackets", with a length of 1 to 200. Underscore is
/// absent from that list, so it is neither kept nor used as a replacement.
///
/// Disallowed characters map to `-`, whitespace runs collapse to a single
/// space, the result is trimmed and truncated to 200 characters, and a
/// missing or fully scrubbed title falls back to `"document"` (AWS rejects
/// an empty name).
fn sanitize_document_name(title: Option<&str>) -> String {
    let mut collapsed = String::new();
    let mut pending_space = false;
    for c in title.unwrap_or("").chars() {
        if c.is_whitespace() {
            // Leading whitespace is dropped rather than collapsed; a run is
            // only emitted once a keepable character follows it.
            pending_space = !collapsed.is_empty();
            continue;
        }
        if pending_space {
            collapsed.push(' ');
            pending_space = false;
        }
        collapsed.push(
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '(' | ')' | '[' | ']') {
                c
            } else {
                '-'
            },
        );
    }
    let truncated = collapsed
        .chars()
        .take(DOCUMENT_NAME_MAX_LEN)
        .collect::<String>();
    // Truncation can strand the space that separated two collapsed words.
    let name = truncated.trim_end();
    if name.is_empty() {
        DOCUMENT_NAME_FALLBACK.to_string()
    } else {
        name.to_string()
    }
}

/// AWS `document.name` length ceiling (`Maximum length of 200`).
const DOCUMENT_NAME_MAX_LEN: usize = 200;

/// Emitted when a title is absent or leaves nothing after sanitizing --
/// AWS requires a minimum length of 1.
const DOCUMENT_NAME_FALLBACK: &str = "document";

/// Normalize a canonical document source's bytes to the base64 form AWS
/// Converse's JSON wire requires. `base64` sources pass through verbatim;
/// `text` sources are base64-encoded (a plain UTF-8 body would otherwise
/// be rejected); any other source type returns None (caller drops the
/// document or falls back to a JSON wrap). Shared by `translate_document`
/// (request blocks) and both tool_result document paths so the three
/// cannot drift on encoding.
fn normalize_document_source_bytes(kind: &str, data: &str) -> Option<String> {
    match kind {
        "base64" => Some(data.to_string()),
        "text" => Some(B64_STANDARD.encode(data.as_bytes())),
        _ => None,
    }
}

fn translate_tool_result_content(
    content: &Value,
    tally: &mut CitationsDropTally<'_>,
) -> Vec<ConverseToolResultContent> {
    match content {
        Value::String(s) => vec![ConverseToolResultContent::Text { text: s.clone() }],
        Value::Array(arr) => arr
            .iter()
            .map(|v| translate_tool_result_array_element(v, tally))
            .collect(),
        Value::Null => Vec::new(),
        other => vec![ConverseToolResultContent::Json {
            json: other.clone(),
        }],
    }
}

/// AWS Converse rejects `toolResult.content: []` ("Member must have at
/// least 1 element"). Empty tool output is a legal, common shape, so the
/// emitted content vector gets a single empty-string Text block when it
/// would otherwise be empty. Applied to the EMITTED Converse vector after
/// translation so nonempty-but-unsupported content keeps its JSON fallback.
fn ensure_min_tool_result_content(content: &mut Vec<ConverseToolResultContent>) {
    if content.is_empty() {
        content.push(ConverseToolResultContent::Text {
            text: String::new(),
        });
    }
}

/// Translate one element from an Anthropic-shape tool_result content
/// array. Anthropic clients send blocks like `{"type":"text","text":"..."}`
/// or `{"type":"image","source":{...}}`. The naive `Json` wrap loses
/// type discrimination and AWS rejects multimodal tool results on
/// Claude 3+. Dispatch on the `type` tag so each shape lands in the
/// correct AWS variant; bare strings stay as Text; unknown shapes fall
/// to Json.
fn translate_tool_result_array_element(
    v: &Value,
    tally: &mut CitationsDropTally<'_>,
) -> ConverseToolResultContent {
    if let Value::String(s) = v {
        return ConverseToolResultContent::Text { text: s.clone() };
    }
    let Some(obj) = v.as_object() else {
        return ConverseToolResultContent::Json { json: v.clone() };
    };
    let kind = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "text" => {
            let text = obj
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            ConverseToolResultContent::Text { text }
        }
        "image" => {
            let Some(source) = obj.get("source") else {
                return ConverseToolResultContent::Json { json: v.clone() };
            };
            let s_obj = source.as_object();
            let media_type = s_obj
                .and_then(|m| m.get("media_type"))
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let data = s_obj
                .and_then(|m| m.get("data"))
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let Some(format) = media_type_to_image_format(media_type) else {
                return ConverseToolResultContent::Json { json: v.clone() };
            };
            ConverseToolResultContent::Image {
                image: ConverseImage {
                    format,
                    source: ConverseImageSource {
                        bytes: data.to_string(),
                    },
                },
            }
        }
        "document" => {
            let Some(source) = obj.get("source") else {
                return ConverseToolResultContent::Json { json: v.clone() };
            };
            let s_obj = source.as_object();
            let kind = s_obj
                .and_then(|m| m.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("base64");
            let media_type = s_obj
                .and_then(|m| m.get("media_type"))
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let data = s_obj
                .and_then(|m| m.get("data"))
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let Some(format) = media_type_to_document_format(media_type) else {
                return ConverseToolResultContent::Json { json: v.clone() };
            };
            // AWS Converse's JSON wire only accepts base64 source bytes;
            // a text source must be encoded (shared with translate_document
            // and document_to_tool_result via normalize_document_source_bytes).
            let Some(bytes) = normalize_document_source_bytes(kind, data) else {
                return ConverseToolResultContent::Json { json: v.clone() };
            };
            let title = obj.get("title").and_then(|t| t.as_str());
            ConverseToolResultContent::Document {
                document: tool_result_document_value(
                    format,
                    title,
                    bytes,
                    obj.get("citations"),
                    tally,
                ),
            }
        }
        _ => ConverseToolResultContent::Json { json: v.clone() },
    }
}

/// Build a synthetic user-role message from a canonical `Role::Tool`
/// turn. Returns an error when `tool_call_id` is missing -- AWS rejects
/// `toolResult.toolUseId == ""` and the silent fallback that produced
/// an empty string upstream-failed with a vague 400.
fn build_tool_message(
    msg: &Message,
    tally: &mut CitationsDropTally<'_>,
) -> Result<ConverseMessage> {
    let Some(tool_use_id) = msg.tool_call_id.as_ref().filter(|s| !s.is_empty()).cloned() else {
        return Err(routectl_core::Error::NormalizeRequest(
            "bedrock-converse".to_string(),
            "tool message missing tool_call_id (Role::Tool requires \
             non-empty toolUseId for AWS Converse)"
                .to_string(),
        ));
    };
    // Sanitize to the charset the toolUse emit uses (via
    // `normalize_tool_calls`) so a result for an OpenAI-origin id still
    // correlates after both map to the same `[a-zA-Z0-9_-]+` value.
    let tool_use_id = crate::tool_id::sanitize_tool_id(&tool_use_id).into_owned();
    let mut content = match &msg.content {
        MessageContent::Text(t) => vec![ConverseToolResultContent::Text { text: t.clone() }],
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|p| translate_part_for_tool_result(p, tally))
            .collect(),
        MessageContent::Null => Vec::new(),
    };
    // AWS Converse requires at least 1 element in toolResult.content.
    // Null content (and the degenerate empty-Parts case) default to a
    // single empty-string text block rather than producing `content: []`
    // which AWS rejects.
    ensure_min_tool_result_content(&mut content);
    Ok(ConverseMessage {
        role: "user".to_string(),
        content: vec![ConverseContentBlock::ToolResult {
            tool_result: ConverseToolResult {
                tool_use_id,
                content,
                status: None,
            },
        }],
    })
}

/// Translate one canonical `ContentPart` into a `ConverseToolResultContent`
/// variant, using the same typed dispatch as
/// `translate_tool_result_array_element`. Without this, multimodal
/// parts (image / document) wrap as `{"json": {"type":"tool_use",...}}`
/// and Claude 3+ on Converse rejects the malformed shape -- the model
/// gets the canonical schema instead of the AWS image/document block.
fn translate_part_for_tool_result(
    p: &ContentPart,
    tally: &mut CitationsDropTally<'_>,
) -> ConverseToolResultContent {
    match p {
        ContentPart::Known(KnownContentPart::Text { text, .. }) => {
            ConverseToolResultContent::Text { text: text.clone() }
        }
        ContentPart::Known(KnownContentPart::Image { source, .. }) => {
            image_source_to_tool_result(source).unwrap_or_else(|| content_part_to_json_fallback(p))
        }
        ContentPart::Known(KnownContentPart::Document {
            source,
            title,
            citations,
            ..
        }) => document_to_tool_result(source, title.as_deref(), citations.as_ref(), tally)
            .unwrap_or_else(|| content_part_to_json_fallback(p)),
        _ => {
            tracing::debug!(
                "tool_result Parts element falls back to Json wrap; \
                 canonical part type has no AWS toolResult variant"
            );
            content_part_to_json_fallback(p)
        }
    }
}

fn content_part_to_json_fallback(p: &ContentPart) -> ConverseToolResultContent {
    ConverseToolResultContent::Json {
        json: serde_json::to_value(p).unwrap_or(Value::Null),
    }
}

/// Translate a canonical Anthropic-shape image source into the AWS
/// toolResult `Image` variant. Returns None when the source isn't
/// base64-shape or the media type isn't AWS-validated; caller falls
/// back to the JSON wrap.
fn image_source_to_tool_result(source: &Value) -> Option<ConverseToolResultContent> {
    let obj = source.as_object()?;
    let kind = obj.get("type").and_then(|v| v.as_str())?;
    if kind != "base64" {
        return None;
    }
    let media_type = obj.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
    let data = obj.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let format = media_type_to_image_format(media_type)?;
    Some(ConverseToolResultContent::Image {
        image: ConverseImage {
            format,
            source: ConverseImageSource {
                bytes: data.to_string(),
            },
        },
    })
}

/// Translate a canonical Document part (source + title + citations) into
/// the AWS toolResult `Document` variant. Returns None for unmappable
/// media types or unsupported source types. Text sources are base64-encoded
/// (shared with `translate_document` and the tool_result array path via
/// `normalize_document_source_bytes`), and the emitted wire value comes
/// from `tool_result_document_value` so both tool_result paths agree.
fn document_to_tool_result(
    source: &Value,
    title: Option<&str>,
    citations: Option<&Value>,
    tally: &mut CitationsDropTally<'_>,
) -> Option<ConverseToolResultContent> {
    let obj = source.as_object()?;
    let kind = obj.get("type").and_then(|v| v.as_str())?;
    let media_type = obj.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
    let data = obj.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let format = media_type_to_document_format(media_type)?;
    let bytes = normalize_document_source_bytes(kind, data)?;
    Some(ConverseToolResultContent::Document {
        document: tool_result_document_value(format, title, bytes, citations, tally),
    })
}

/// Assemble the `toolResult.content[].document` wire value shared by both
/// tool_result document paths -- the canonical Parts path and the raw
/// Anthropic-shape array path. Both emit the same members as
/// `ConverseDocument`, and citations lift through the same
/// `translate_document_citations` mapping the message-content path uses, so
/// a document behaves identically wherever it appears.
fn tool_result_document_value(
    format: String,
    title: Option<&str>,
    bytes: String,
    citations: Option<&Value>,
    tally: &mut CitationsDropTally<'_>,
) -> Value {
    let mut document = serde_json::json!({
        "format": format,
        "name": sanitize_document_name(title),
        "source": {"bytes": bytes},
    });
    if let Some(enabled) = translate_document_citations(citations, tally)
        && let Some(map) = document.as_object_mut()
    {
        map.insert(
            "citations".to_string(),
            serde_json::json!({"enabled": enabled}),
        );
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ReasoningDetail, ReasoningDetailKind};
    use serde_json::json;
    use tracing_test::traced_test;

    /// Provider id passed to translators under test; only reaches log fields.
    const TEST_ID: &str = "test";

    fn user_msg() -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// An assistant message carrying `reasoning_details` (anthropic-claude-v1
    /// format, Text kind) must produce a `ReasoningContent` block with the
    /// correct text and signature on the Converse request.
    #[test]
    fn assistant_reasoning_details_text_produces_reasoning_content_block() {
        // Arrange
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some("rd-1".into()),
            format: Some(crate::anthropic_api::ANTHROPIC_FORMAT.to_string()),
            index: Some(0),
            payload: json!({"text": "my reasoning", "signature": "sig_abc"}),
        };
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("sure".into()),
                reasoning: None,
                reasoning_details: vec![detail],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let assistant = result
            .iter()
            .find(|m| m.role == "assistant")
            .expect("assistant message must be present");
        let reasoning_block = assistant
            .content
            .iter()
            .find(|b| matches!(b, ConverseContentBlock::ReasoningContent { .. }));
        assert!(
            reasoning_block.is_some(),
            "assistant message carrying reasoning_details must produce a \
             ReasoningContent block on the Converse request, got: {:?}",
            assistant.content
        );
        match reasoning_block.unwrap() {
            ConverseContentBlock::ReasoningContent { reasoning_content } => match reasoning_content
            {
                ConverseRequestReasoningBlock::ReasoningText { reasoning_text } => {
                    assert_eq!(reasoning_text.text, "my reasoning");
                    assert_eq!(reasoning_text.signature.as_deref(), Some("sig_abc"));
                }
                other => panic!("expected ReasoningText, got {other:?}"),
            },
            _ => panic!("expected ReasoningContent block"),
        }
        // The trailing text content must also be present after the reasoning block.
        let text_block = assistant
            .content
            .iter()
            .find(|b| matches!(b, ConverseContentBlock::Text { .. }));
        assert!(
            text_block.is_some(),
            "text content must survive alongside reasoning_details"
        );
    }

    /// Encrypted reasoning (ReasoningDetailKind::Encrypted) must produce a
    /// RedactedContent block on the Converse egress.
    #[test]
    fn assistant_encrypted_reasoning_detail_produces_redacted_block() {
        // Arrange
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Encrypted,
            id: Some("rd-2".into()),
            format: Some(crate::anthropic_api::ANTHROPIC_FORMAT.to_string()),
            index: Some(0),
            payload: json!({"data": "base64data=="}),
        };
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("here".into()),
                reasoning: None,
                reasoning_details: vec![detail],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let assistant = result
            .iter()
            .find(|m| m.role == "assistant")
            .expect("assistant message must be present");
        let redacted = assistant.content.iter().find(|b| {
            matches!(
                b,
                ConverseContentBlock::ReasoningContent {
                    reasoning_content: ConverseRequestReasoningBlock::RedactedContent { .. },
                }
            )
        });
        assert!(
            redacted.is_some(),
            "encrypted reasoning_detail must produce a RedactedContent block, \
             got: {:?}",
            assistant.content
        );
    }

    /// Non-anthropic-claude-v1 format reasoning details must be ignored.
    #[test]
    fn non_anthropic_format_reasoning_detail_is_skipped() {
        // Arrange
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some("rd-3".into()),
            format: Some("openai-v1".into()),
            index: Some(0),
            payload: json!({"text": "other reasoning", "signature": "sig_x"}),
        };
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("response".into()),
                reasoning: None,
                reasoning_details: vec![detail],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let assistant = result
            .iter()
            .find(|m| m.role == "assistant")
            .expect("assistant message must be present");
        let has_reasoning = assistant
            .content
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::ReasoningContent { .. }));
        assert!(
            !has_reasoning,
            "non-anthropic-claude-v1 reasoning_detail must not produce a Converse block, \
             got: {:?}",
            assistant.content
        );
    }

    /// When reasoning_details is empty, KnownContentPart::Thinking in content
    /// still produces a ReasoningContent block (existing path, regression guard).
    #[test]
    fn thinking_in_content_parts_still_works_when_no_reasoning_details() {
        // Arrange
        use routectl_core::KnownContentPart;
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "content-path thinking".into(),
                        signature: Some("sig_content".into()),
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "result".into(),
                        citations: None,
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let assistant = result
            .iter()
            .find(|m| m.role == "assistant")
            .expect("assistant message must be present");
        let has_reasoning = assistant
            .content
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::ReasoningContent { .. }));
        assert!(
            has_reasoning,
            "KnownContentPart::Thinking in content.Parts must still produce a \
             ReasoningContent block when reasoning_details is empty, got: {:?}",
            assistant.content
        );
    }

    /// A canonical text-source document (`{type:"text", media_type, data}`)
    /// is a valid Anthropic shape and must survive translation as a base64
    /// Converse document block -- the plain-text body gets base64-encoded
    /// rather than dropped.
    #[test]
    fn text_source_document_survives_as_base64_document_block() {
        // Arrange
        use routectl_core::KnownContentPart;
        let body = "the quick brown fox";
        let messages = vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Document {
                source: json!({
                    "type": "text",
                    "media_type": "text/plain",
                    "data": body,
                }),
                title: Some("notes".into()),
                citations: None,
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let user = result
            .iter()
            .find(|m| m.role == "user")
            .expect("user message must survive a text-source document");
        let doc = user
            .content
            .iter()
            .find_map(|b| match b {
                ConverseContentBlock::Document { document } => Some(document),
                _ => None,
            })
            .expect("text-source document must produce a Document block");
        assert_eq!(doc.format, "txt", "text/plain maps to the txt format");
        assert_eq!(
            doc.source.bytes,
            B64_STANDARD.encode(body.as_bytes()),
            "text-source body must be base64-encoded onto the Converse wire"
        );
    }

    /// An OpenAI-shape file part carrying a base64 PDF data URI must
    /// survive translation as a Converse Document block (the file_data is
    /// rewritten into the canonical Anthropic source the document
    /// translator consumes).
    #[test]
    fn pdf_file_part_survives_as_converse_document_block() {
        // Arrange
        use routectl_core::KnownContentPart;
        let messages = vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::File {
                file: json!({
                    "filename": "draft.pdf",
                    "file_data": "data:application/pdf;base64,JVBERi0xLjQ=",
                }),
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let user = result
            .iter()
            .find(|m| m.role == "user")
            .expect("user message must survive a base64 PDF file part");
        let doc = user
            .content
            .iter()
            .find_map(|b| match b {
                ConverseContentBlock::Document { document } => Some(document),
                _ => None,
            })
            .expect("PDF file part must produce a Document block");
        assert_eq!(doc.format, "pdf");
        assert_eq!(doc.source.bytes, "JVBERi0xLjQ=");
    }

    /// A file_id-only reference has no inline bytes the JSON Converse wire
    /// can carry, so it is dropped with a diagnostic. A sibling Text block
    /// confirms only the file part was dropped.
    #[test]
    fn file_id_only_part_is_dropped_on_converse() {
        // Arrange
        use routectl_core::KnownContentPart;
        let messages = vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "see attached".into(),
                    citations: None,
                    cache_control: None,
                }),
                ContentPart::Known(KnownContentPart::File {
                    file: json!({"file_id": "file-abc"}),
                    cache_control: None,
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let user = result
            .iter()
            .find(|m| m.role == "user")
            .expect("user message must survive on the sibling Text block");
        let has_document = user
            .content
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::Document { .. }));
        assert!(
            !has_document,
            "file_id-only part must be dropped, got: {:?}",
            user.content
        );
    }

    /// A non-PDF file part (e.g. a text/plain base64 data URI) is dropped
    /// on the Converse egress, matching the PDF-only scope of the Anthropic
    /// egress helper. A sibling Text block confirms only the file part was
    /// dropped, not the whole message.
    #[test]
    fn non_pdf_file_part_is_dropped_on_converse() {
        // Arrange
        use routectl_core::KnownContentPart;
        let messages = vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "see attached".into(),
                    citations: None,
                    cache_control: None,
                }),
                ContentPart::Known(KnownContentPart::File {
                    file: json!({
                        "filename": "note.txt",
                        "file_data": "data:text/plain;base64,aGVsbG8=",
                    }),
                    cache_control: None,
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let user = result
            .iter()
            .find(|m| m.role == "user")
            .expect("user message must survive on the sibling Text block");
        let has_document = user
            .content
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::Document { .. }));
        assert!(
            !has_document,
            "non-PDF file part must be dropped, got: {:?}",
            user.content
        );
    }

    /// An image whose media_type doesn't map to an AWS image format is
    /// dropped (the caller-contract promises a tracing diagnostic on every
    /// drop). A sibling Text block confirms only the image was dropped.
    #[test]
    fn image_with_unmapped_media_type_is_dropped() {
        // Arrange
        use routectl_core::KnownContentPart;
        let messages = vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "look at this".into(),
                    citations: None,
                    cache_control: None,
                }),
                ContentPart::Known(KnownContentPart::Image {
                    source: json!({
                        "type": "base64",
                        "media_type": "image/tiff",
                        "data": "AAAA",
                    }),
                    cache_control: None,
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let user = result
            .iter()
            .find(|m| m.role == "user")
            .expect("user message must survive on the sibling Text block");
        let has_image = user
            .content
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::Image { .. }));
        assert!(
            !has_image,
            "image with an unmapped media_type must be dropped, got: {:?}",
            user.content
        );
        let has_text = user
            .content
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::Text { .. }));
        assert!(has_text, "the sibling Text block must survive");
    }

    /// An assistant turn whose tool call rides ONLY on the OpenAI-shape
    /// `tool_calls` field (content null/empty, no ToolUse content part)
    /// must re-emit a Converse `toolUse` block carrying the call id, name,
    /// and parsed arguments -- so the following toolResult turn has a
    /// preceding toolUse with a matching id and is not orphaned.
    #[test]
    fn assistant_openai_tool_calls_field_emits_converse_tool_use_block() {
        // Arrange
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"},
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("sunny".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("call_1".into()),
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert: the assistant turn survives with a toolUse block.
        let assistant = result
            .iter()
            .find(|m| m.role == "assistant")
            .expect("assistant message carrying tool_calls must not be skipped");
        let tool_use = assistant
            .content
            .iter()
            .find_map(|b| match b {
                ConverseContentBlock::ToolUse { tool_use } => Some(tool_use),
                _ => None,
            })
            .expect("tool_calls field must produce a toolUse block");
        assert_eq!(tool_use.tool_use_id, "call_1");
        assert_eq!(tool_use.name, "get_weather");
        assert_eq!(tool_use.input, json!({"city": "SF"}));

        // The toolResult turn references the same id and is preceded by
        // the toolUse (the assistant turn appears before the tool turn).
        let assistant_idx = result.iter().position(|m| m.role == "assistant").unwrap();
        let tool_result_idx = result
            .iter()
            .position(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ConverseContentBlock::ToolResult { .. }))
            })
            .expect("toolResult turn must be present");
        assert!(
            assistant_idx < tool_result_idx,
            "toolUse must precede the toolResult"
        );
    }

    /// A tool call with a missing id is synthesized to a non-empty
    /// toolUseId so AWS does not reject the empty-id toolUse block.
    #[test]
    fn assistant_tool_call_missing_id_is_synthesized_on_converse() {
        // Arrange
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "function": {"name": "f", "arguments": "{}"},
                })]),
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let assistant = result.iter().find(|m| m.role == "assistant").unwrap();
        let tool_use = assistant
            .content
            .iter()
            .find_map(|b| match b {
                ConverseContentBlock::ToolUse { tool_use } => Some(tool_use),
                _ => None,
            })
            .expect("missing-id tool call must still produce a toolUse block");
        assert!(
            !tool_use.tool_use_id.is_empty(),
            "missing id must be synthesized non-empty, got empty"
        );
    }

    /// When the assistant turn ALREADY carries a ToolUse content part,
    /// setting `tool_calls` as well must NOT double-emit the toolUse block
    /// (the content-part path already emitted it).
    #[test]
    fn assistant_tool_use_content_part_not_doubled_by_tool_calls_field() {
        // Arrange
        use routectl_core::KnownContentPart;
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolUse {
                        id: "call_1".into(),
                        name: "get_weather".into(),
                        input: json!({"city": "SF"}),
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
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"},
                })]),
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert: exactly one toolUse block, not two.
        let assistant = result.iter().find(|m| m.role == "assistant").unwrap();
        let tool_use_count = assistant
            .content
            .iter()
            .filter(|b| matches!(b, ConverseContentBlock::ToolUse { .. }))
            .count();
        assert_eq!(
            tool_use_count, 1,
            "ToolUse must not be doubled when both content part and tool_calls are set"
        );
    }

    /// A single-turn assistant message with no tool_calls and no ToolUse
    /// content is unchanged: a plain text turn produces exactly one Text
    /// block and nothing else.
    #[test]
    fn assistant_plain_text_turn_unchanged_without_tool_calls() {
        // Arrange
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("just text".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let assistant = result.iter().find(|m| m.role == "assistant").unwrap();
        assert_eq!(assistant.content.len(), 1, "exactly one block expected");
        match &assistant.content[0] {
            ConverseContentBlock::Text { text } => assert_eq!(text, "just text"),
            other => panic!("expected a single Text block, got {other:?}"),
        }
    }

    /// An OpenAI-origin id with `.`/`:` is sanitized identically at the
    /// toolUse emit AND the toolResult correlation site, so the result is
    /// not orphaned.
    #[test]
    fn openai_origin_tool_id_sanitized_consistently_across_converse_egress() {
        // Arrange
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call.foo:1",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{}"},
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("call.foo:1".into()),
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert -- emitted toolUse id and toolResult id are the same
        // sanitized value, so the result is not orphaned.
        let tool_use_id = result
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ConverseContentBlock::ToolUse { tool_use } => {
                        Some(tool_use.tool_use_id.clone())
                    }
                    _ => None,
                })
            })
            .expect("toolUse block must be present");
        let tool_result_id = result
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ConverseContentBlock::ToolResult { tool_result } => {
                        Some(tool_result.tool_use_id.clone())
                    }
                    _ => None,
                })
            })
            .expect("toolResult block must be present");
        assert_eq!(tool_use_id, "esc_call_2efoo_3a1");
        assert_eq!(tool_result_id, "esc_call_2efoo_3a1");
        assert_eq!(tool_use_id, tool_result_id);
    }

    /// The CANONICAL-part path (an Anthropic-shape assistant `ToolUse`
    /// block plus a user `ToolResult` block) is a different code path from
    /// `Message.tool_calls` + `Role::Tool`, and it bypassed the sanitizer
    /// entirely until 2026-08-01: an over-ceiling id reached the wire raw.
    /// Raw/raw still CORRELATES, which is why the id-collision tests never
    /// caught it -- the defect is that Bedrock rejects a `toolUseId` over
    /// its documented 64-byte maximum.
    #[test]
    fn over_long_tool_id_on_canonical_parts_folds_within_the_ceiling() {
        // Arrange -- 65 bytes, entirely inside the target charset.
        let raw = "a".repeat(65);
        let messages = vec![
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolUse {
                        id: raw.clone(),
                        name: "f".into(),
                        input: json!({}),
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolResult {
                        tool_use_id: raw.clone(),
                        content: json!("ok"),
                        is_error: None,
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert -- both sides fold to the SAME 64-byte digest as every
        // other lane, so the result still correlates AND the wire is legal.
        let expected = format!("esct_{}_8087e9a889f8a14c", "a".repeat(42));
        assert_eq!(expected.len(), 64, "the fold must sit at the ceiling");
        let use_id = result
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ConverseContentBlock::ToolUse { tool_use } => {
                        Some(tool_use.tool_use_id.clone())
                    }
                    _ => None,
                })
            })
            .expect("toolUse block must be present");
        let result_id = result
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ConverseContentBlock::ToolResult { tool_result } => {
                        Some(tool_result.tool_use_id.clone())
                    }
                    _ => None,
                })
            })
            .expect("toolResult block must be present");
        assert_eq!(use_id, expected, "canonical ToolUse id must fold");
        assert_eq!(result_id, expected, "canonical ToolResult id must fold");
    }

    /// A wire-safe id over the documented 64-byte `toolUseId` ceiling
    /// folds to the digest form at BOTH the toolUse emit and the toolResult
    /// correlation site. The expected literal is the same one the Anthropic
    /// lane pins, which is the cross-lane agreement the fold rests on: an
    /// id sanitized on one lane and replayed on another must land on the
    /// same value.
    #[test]
    fn over_long_wire_safe_tool_id_folds_consistently_across_converse_egress() {
        // Arrange -- 65 bytes, entirely in the target charset.
        let raw = "a".repeat(65);
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": raw,
                    "type": "function",
                    "function": {"name": "f", "arguments": "{}"},
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some(raw.clone()),
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let expected = format!("esct_{}_8087e9a889f8a14c", "a".repeat(42));
        assert_eq!(expected.len(), 64, "the fold must sit at the ceiling");
        let tool_use_id = result
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ConverseContentBlock::ToolUse { tool_use } => {
                        Some(tool_use.tool_use_id.clone())
                    }
                    _ => None,
                })
            })
            .expect("toolUse block must be present");
        let tool_result_id = result
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ConverseContentBlock::ToolResult { tool_result } => {
                        Some(tool_result.tool_use_id.clone())
                    }
                    _ => None,
                })
            })
            .expect("toolResult block must be present");
        assert_eq!(tool_use_id, expected);
        assert_eq!(tool_result_id, expected);
    }

    /// A valid id round-trips unchanged through both the toolUse emit and
    /// the toolResult correlation site.
    #[test]
    fn valid_tool_id_round_trips_unchanged_through_converse_egress() {
        // Arrange
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_abc-1_2",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{}"},
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("call_abc-1_2".into()),
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let tool_use_id = result
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ConverseContentBlock::ToolUse { tool_use } => {
                        Some(tool_use.tool_use_id.clone())
                    }
                    _ => None,
                })
            })
            .expect("toolUse block must be present");
        let tool_result_id = result
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ConverseContentBlock::ToolResult { tool_result } => {
                        Some(tool_result.tool_use_id.clone())
                    }
                    _ => None,
                })
            })
            .expect("toolResult block must be present");
        assert_eq!(tool_use_id, "call_abc-1_2");
        assert_eq!(tool_result_id, "call_abc-1_2");
    }

    /// Two DISTINCT source tool ids that differ only in characters the
    /// former lossy sanitizer folded to `_` must reach the wire as
    /// DISTINCT toolUseIds, with each toolResult still correlating to its
    /// own toolUse.
    #[test]
    fn colliding_source_tool_ids_stay_distinct_and_paired_on_converse_egress() {
        // Arrange
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    json!({
                        "id": "call.a",
                        "type": "function",
                        "function": {"name": "f", "arguments": "{}"},
                    }),
                    json!({
                        "id": "call:a",
                        "type": "function",
                        "function": {"name": "f", "arguments": "{}"},
                    }),
                ]),
            },
            tool_msg("call.a"),
            tool_msg("call:a"),
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert
        let uses: Vec<String> = result
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ConverseContentBlock::ToolUse { tool_use } => Some(tool_use.tool_use_id.clone()),
                _ => None,
            })
            .collect();
        let results: Vec<String> = result
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ConverseContentBlock::ToolResult { tool_result } => {
                    Some(tool_result.tool_use_id.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(uses.len(), 2, "both distinct source calls must survive");
        assert_ne!(uses[0], uses[1], "toolUseIds must not collide");
        assert_eq!(uses, results, "each result must pair with its own use");
    }

    fn tool_msg(id: &str) -> Message {
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

    /// LOW-5 fix: a `Role::Tool` message with `MessageContent::Null` must
    /// emit a `toolResult.content` carrying exactly ONE empty-string text
    /// block, not an empty array. AWS Converse rejects
    /// `toolResult.content: []` ("Member must have at least 1 element").
    /// This matches the anthropic-api egress, which emits an empty-string
    /// text block for the same Null case.
    #[test]
    fn build_tool_message_null_content_emits_single_empty_text_block() {
        // Arrange: a tool message with Null content and a valid id.
        let msg = Message {
            refusal: None,
            role: Role::Tool,
            content: MessageContent::Null,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: Some("tu_null".into()),
            tool_calls: None,
        };

        // Act
        let result = build_tool_message(&msg, &mut CitationsDropTally::new("test"))
            .expect("Null-content tool message must translate");

        // Assert
        let tool_result = result
            .content
            .iter()
            .find_map(|b| match b {
                ConverseContentBlock::ToolResult { tool_result } => Some(tool_result),
                _ => None,
            })
            .expect("toolResult block must be present");
        assert_eq!(
            tool_result.content.len(),
            1,
            "Null content must yield exactly one content element (AWS requires >=1), \
             got: {:?}",
            tool_result.content
        );
        match &tool_result.content[0] {
            ConverseToolResultContent::Text { text } => {
                assert_eq!(
                    text, "",
                    "the single element must be an empty-string text block"
                );
            }
            other => panic!("expected an empty Text block, got {other:?}"),
        }
    }

    /// Helper: run `translate_known_part` on a ToolResult part and return
    /// its emitted `toolResult.content` vector.
    fn tool_result_content_of(content: Value) -> Vec<ConverseToolResultContent> {
        use routectl_core::KnownContentPart;
        let part = KnownContentPart::ToolResult {
            tool_use_id: "tu_1".into(),
            content,
            is_error: None,
            cache_control: None,
        };
        let block = translate_known_part("test", &part, &mut CitationsDropTally::new("test"))
            .expect("ToolResult part must translate")
            .expect("ToolResult part must produce a block");
        match block {
            ConverseContentBlock::ToolResult { tool_result } => tool_result.content,
            other => panic!("expected a ToolResult block, got {other:?}"),
        }
    }

    /// The `KnownContentPart::ToolResult` arm must guard empty content the
    /// same way `build_tool_message` does: `Value::Null` content emits
    /// exactly one empty-string Text block, never `content: []` (which AWS
    /// Converse rejects with "Member must have at least 1 element").
    #[test]
    fn tool_result_part_null_content_emits_single_empty_text_block() {
        let content = tool_result_content_of(Value::Null);
        assert_eq!(
            content.len(),
            1,
            "Null content must yield exactly one element (AWS requires >=1), got: {content:?}"
        );
        match &content[0] {
            ConverseToolResultContent::Text { text } => {
                assert_eq!(
                    text, "",
                    "the single element must be an empty-string text block"
                );
            }
            other => panic!("expected an empty Text block, got {other:?}"),
        }
    }

    /// The `KnownContentPart::ToolResult` arm must also guard an empty
    /// content array: `[]` emits exactly one empty-string Text block.
    #[test]
    fn tool_result_part_empty_array_content_emits_single_empty_text_block() {
        let content = tool_result_content_of(json!([]));
        assert_eq!(
            content.len(),
            1,
            "empty-array content must yield exactly one element (AWS requires >=1), got: {content:?}"
        );
        match &content[0] {
            ConverseToolResultContent::Text { text } => {
                assert_eq!(
                    text, "",
                    "the single element must be an empty-string text block"
                );
            }
            other => panic!("expected an empty Text block, got {other:?}"),
        }
    }

    /// Nonempty-but-unsupported content must NOT be collapsed by the guard:
    /// a bare JSON object (no `type` tag) round-trips through the Json
    /// fallback rather than being replaced by an empty Text block.
    #[test]
    fn tool_result_part_nonempty_unsupported_content_keeps_json_fallback() {
        let payload = json!({"result": 42, "ok": true});
        let content = tool_result_content_of(payload.clone());
        assert_eq!(
            content.len(),
            1,
            "unsupported object content maps to exactly one Json element, got: {content:?}"
        );
        match &content[0] {
            ConverseToolResultContent::Json { json } => {
                assert_eq!(
                    json, &payload,
                    "unsupported content must survive as its JSON fallback"
                );
            }
            other => panic!("expected a Json fallback block, got {other:?}"),
        }
    }

    /// A `{type:"text"}` document source inside a tool_result content
    /// ARRAY must be base64-encoded on the wire (AWS Converse JSON only
    /// accepts base64 source bytes), matching `translate_document` for
    /// the same input. Previously the array path forwarded the raw text
    /// verbatim -> AWS rejected the malformed source.
    #[test]
    fn text_source_document_in_tool_result_array_is_base64_encoded() {
        let element = json!({
            "type": "document",
            "source": {"type": "text", "media_type": "text/plain", "data": "hello"},
            "title": "notes",
        });
        let out =
            translate_tool_result_array_element(&element, &mut CitationsDropTally::new("test"));
        let ConverseToolResultContent::Document { document } = out else {
            panic!("expected a Document toolResult variant, got: {out:?}");
        };
        assert_eq!(
            document["source"]["bytes"],
            B64_STANDARD.encode("hello"),
            "text source must be base64-encoded to match translate_document: {document}"
        );
    }

    /// `document_to_tool_result` (the canonical Parts path) must apply
    /// the same text-source base64 normalization. Previously a text
    /// source returned None (dropped to the JSON fallback).
    #[test]
    fn text_source_document_to_tool_result_is_base64_encoded() {
        let source = json!({"type": "text", "media_type": "text/plain", "data": "hello"});
        let out = document_to_tool_result(
            &source,
            Some("notes"),
            None,
            &mut CitationsDropTally::new("test"),
        )
        .expect("text source must now translate to a Document variant, not None");
        let ConverseToolResultContent::Document { document } = out else {
            panic!("expected a Document toolResult variant, got: {out:?}");
        };
        assert_eq!(
            document["source"]["bytes"],
            B64_STANDARD.encode("hello"),
            "text source must be base64-encoded: {document}"
        );
    }

    /// A `base64` source must NOT be double-encoded on either
    /// tool_result path -- the bytes pass through verbatim.
    #[test]
    fn base64_source_document_not_double_encoded_on_tool_result_paths() {
        let already = B64_STANDARD.encode("hello");

        let element = json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "text/plain", "data": already},
            "title": "notes",
        });
        let out =
            translate_tool_result_array_element(&element, &mut CitationsDropTally::new("test"));
        let ConverseToolResultContent::Document { document } = out else {
            panic!("array path: expected Document variant, got: {out:?}");
        };
        assert_eq!(
            document["source"]["bytes"], already,
            "array path double-encoded a base64 source: {document}"
        );

        let source = json!({"type": "base64", "media_type": "text/plain", "data": already});
        let out = document_to_tool_result(
            &source,
            Some("notes"),
            None,
            &mut CitationsDropTally::new("test"),
        )
        .expect("base64 source must translate to a Document variant");
        let ConverseToolResultContent::Document { document } = out else {
            panic!("Parts path: expected Document variant, got: {out:?}");
        };
        assert_eq!(
            document["source"]["bytes"], already,
            "Parts path double-encoded a base64 source: {document}"
        );
    }

    fn assistant_two_tool_calls() -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Null,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![
                json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"},
                }),
                json!({
                    "id": "call_2",
                    "type": "function",
                    "function": {"name": "get_time", "arguments": "{\"tz\":\"PST\"}"},
                }),
            ]),
        }
    }

    fn tool_result_msg(id: &str, text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::Tool,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: Some(id.into()),
            tool_calls: None,
        }
    }

    /// Two parallel tool results (consecutive Role::Tool turns) coalesce
    /// into a SINGLE Converse user message carrying both toolResult blocks
    /// in order -- Converse 400s on consecutive same-role turns.
    #[test]
    fn parallel_tool_results_coalesce_into_one_user_turn() {
        // Arrange
        let messages = vec![
            user_msg(),
            assistant_two_tool_calls(),
            tool_result_msg("call_1", "sunny"),
            tool_result_msg("call_2", "noon"),
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert -- exactly one user turn follows the assistant turn, and it
        // carries both toolResult blocks in submission order.
        let tool_turns: Vec<&ConverseMessage> = result
            .iter()
            .filter(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ConverseContentBlock::ToolResult { .. }))
            })
            .collect();
        assert_eq!(
            tool_turns.len(),
            1,
            "the two tool results must coalesce into one Converse message, got: {result:?}"
        );
        let ids: Vec<&str> = tool_turns[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ConverseContentBlock::ToolResult { tool_result } => {
                    Some(tool_result.tool_use_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["call_1", "call_2"],
            "both toolResult blocks must survive in order with correlation ids preserved"
        );
    }

    /// Roles must strictly alternate across an interleaved
    /// assistant/tool/user sequence: coalescing the two tool turns yields
    /// assistant -> user -> assistant, never two consecutive user turns.
    #[test]
    fn interleaved_assistant_tool_user_still_alternates() {
        // Arrange
        let messages = vec![
            user_msg(),
            assistant_two_tool_calls(),
            tool_result_msg("call_1", "sunny"),
            tool_result_msg("call_2", "noon"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("all set".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert -- no two adjacent messages share a role.
        let roles: Vec<&str> = result.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "assistant"],
            "roles must strictly alternate after coalescing, got: {roles:?}"
        );
    }

    /// A single tool result after an assistant turn is unchanged: it still
    /// produces exactly one user message with one toolResult block.
    #[test]
    fn single_tool_result_unchanged() {
        // Arrange
        let messages = vec![
            user_msg(),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"},
                })]),
            },
            tool_result_msg("call_1", "sunny"),
        ];

        // Act
        let result = build_messages("test", &messages).unwrap();

        // Assert -- one user turn carrying exactly one toolResult block.
        let tool_turns: Vec<&ConverseMessage> = result
            .iter()
            .filter(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ConverseContentBlock::ToolResult { .. }))
            })
            .collect();
        assert_eq!(tool_turns.len(), 1, "single tool result -> one user turn");
        let block_count = tool_turns[0]
            .content
            .iter()
            .filter(|b| matches!(b, ConverseContentBlock::ToolResult { .. }))
            .count();
        assert_eq!(block_count, 1, "exactly one toolResult block expected");
    }

    /// AWS's prose for `document.name` omits underscore, so an underscored
    /// filename must not keep its underscores nor gain new ones from the
    /// replacement of the disallowed dot.
    #[test]
    fn document_name_maps_underscores_and_dots_to_hyphens() {
        // Arrange / Act
        let name = sanitize_document_name(Some("report_v2.pdf"));

        // Assert
        assert_eq!(name, "report-v2-pdf");
        assert!(!name.contains('_'), "underscore is not in AWS's charset");
    }

    /// AWS allows whitespace but "no more than one in a row", so a run must
    /// collapse to a single space.
    #[test]
    fn document_name_collapses_whitespace_runs_to_one_space() {
        // Arrange / Act
        let name = sanitize_document_name(Some("my   notes.pdf"));

        // Assert
        assert_eq!(name, "my notes-pdf");
    }

    /// Tabs and newlines are whitespace under AWS's prose, so they collapse
    /// into the same single space rather than becoming hyphens.
    #[test]
    fn document_name_collapses_mixed_whitespace_kinds() {
        // Arrange / Act
        let name = sanitize_document_name(Some("a \t\n b"));

        // Assert
        assert_eq!(name, "a b");
    }

    /// A name already inside AWS's documented charset must survive verbatim.
    #[test]
    fn document_name_passes_through_already_valid_names() {
        // Arrange
        let valid = "Q3 Report (final) [v2] - 2026";

        // Act
        let name = sanitize_document_name(Some(valid));

        // Assert
        assert_eq!(name, valid);
    }

    /// Surrounding whitespace is dropped rather than collapsed to a leading
    /// or trailing space.
    #[test]
    fn document_name_trims_surrounding_whitespace() {
        // Arrange / Act
        let name = sanitize_document_name(Some("  spaced  "));

        // Assert
        assert_eq!(name, "spaced");
    }

    /// AWS requires a minimum length of 1, so a missing title or one whose
    /// every character is whitespace must yield the deterministic fallback.
    #[test]
    fn document_name_falls_back_when_nothing_survives() {
        // Arrange / Act / Assert
        assert_eq!(sanitize_document_name(None), "document");
        assert_eq!(sanitize_document_name(Some("")), "document");
        assert_eq!(sanitize_document_name(Some("   \t ")), "document");
    }

    /// Non-whitespace characters outside the charset are replaced, not
    /// dropped, so an all-disallowed title stays non-empty.
    #[test]
    fn document_name_replaces_rather_than_drops_disallowed_characters() {
        // Arrange / Act
        let name = sanitize_document_name(Some("***"));

        // Assert
        assert_eq!(name, "---");
    }

    /// AWS caps the name at 200 characters, and truncation must not leave a
    /// trailing space that would read as a violating boundary.
    #[test]
    fn document_name_truncates_at_two_hundred_characters() {
        // Arrange -- 199 'a's then a space then more, so the 200th char is a space.
        let title = format!("{} tail", "a".repeat(199));

        // Act
        let name = sanitize_document_name(Some(&title));

        // Assert
        assert_eq!(name.chars().count(), 199);
        assert_eq!(name, "a".repeat(199));
    }

    /// Build a user message carrying one canonical Document part with the
    /// given `citations` value.
    fn document_message(citations: Option<Value>) -> Message {
        use routectl_core::KnownContentPart;
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Document {
                source: json!({
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": "JVBERi0xLjQ=",
                }),
                title: Some("notes".into()),
                citations,
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Build a `Role::Tool` message whose Parts content carries one
    /// canonical Document part with the given `citations` value.
    fn document_tool_message(citations: Option<Value>) -> Message {
        Message {
            role: Role::Tool,
            tool_call_id: Some("tu_1".into()),
            ..document_message(citations)
        }
    }

    /// Serialize the single Document block out of a translated message and
    /// return it as JSON, so assertions see the exact wire shape.
    fn document_block_json(messages: &[Message]) -> Value {
        let out = build_messages(TEST_ID, messages).expect("messages must translate");
        let block = out
            .iter()
            .flat_map(|m| m.content.iter())
            .find(|b| matches!(b, ConverseContentBlock::Document { .. }))
            .expect("a Document block must be present");
        serde_json::to_value(block).expect("block must serialize")["document"].clone()
    }

    /// Same, for the tool-result path: the Document rides inside
    /// `toolResult.content`.
    fn tool_result_document_json(messages: &[Message]) -> Value {
        let out = build_messages(TEST_ID, messages).expect("messages must translate");
        let block = out
            .iter()
            .flat_map(|m| m.content.iter())
            .find(|b| matches!(b, ConverseContentBlock::ToolResult { .. }))
            .expect("a ToolResult block must be present");
        let json = serde_json::to_value(block).expect("block must serialize");
        json["toolResult"]["content"]
            .as_array()
            .expect("toolResult.content must be an array")
            .iter()
            .find_map(|c| c.get("document").cloned())
            .expect("a document toolResult element must be present")
    }

    /// An Anthropic-origin document with citations ENABLED must reach the
    /// Converse wire carrying `citations: {enabled: true}` -- previously the
    /// call site discarded the field and the model returned uncited prose.
    #[test]
    fn document_citations_enabled_reaches_wire_from_message_content() {
        // Arrange
        let messages = vec![document_message(Some(json!({"enabled": true})))];

        // Act
        let document = document_block_json(&messages);

        // Assert
        assert_eq!(
            document["citations"],
            json!({"enabled": true}),
            "message-content document must carry citations onto the wire: {document}"
        );
    }

    /// The same document appearing in a tool result must behave identically
    /// -- a citations config that survives in message content and vanishes
    /// in a tool result is the asymmetry this fix exists to prevent.
    #[test]
    fn document_citations_enabled_reaches_wire_from_tool_result() {
        // Arrange
        let messages = vec![document_tool_message(Some(json!({"enabled": true})))];

        // Act
        let document = tool_result_document_json(&messages);

        // Assert
        assert_eq!(
            document["citations"],
            json!({"enabled": true}),
            "tool-result document must carry citations onto the wire: {document}"
        );
    }

    /// A document with no citations value emits no `citations` member on
    /// either path -- the field is optional on `DocumentBlock`.
    #[test]
    fn document_without_citations_omits_the_member_on_both_paths() {
        // Arrange / Act
        let from_content = document_block_json(&[document_message(None)]);
        let from_tool_result = tool_result_document_json(&[document_tool_message(None)]);

        // Assert
        assert!(
            from_content.get("citations").is_none(),
            "absent citations must not emit the member: {from_content}"
        );
        assert!(
            from_tool_result.get("citations").is_none(),
            "absent citations must not emit the member: {from_tool_result}"
        );
        assert_eq!(
            from_content["format"], "pdf",
            "the rest of the document block is unchanged: {from_content}"
        );
    }

    /// An explicit `{enabled: false}` is behaviorally identical to absence,
    /// so the optional member is omitted rather than emitted as wire noise.
    #[test]
    fn document_citations_disabled_omits_the_member_on_both_paths() {
        // Arrange
        let disabled = json!({"enabled": false});

        // Act
        let from_content = document_block_json(&[document_message(Some(disabled.clone()))]);
        let from_tool_result = tool_result_document_json(&[document_tool_message(Some(disabled))]);

        // Assert
        assert!(
            from_content.get("citations").is_none(),
            "citations:false must not emit the member: {from_content}"
        );
        assert!(
            from_tool_result.get("citations").is_none(),
            "citations:false must not emit the member: {from_tool_result}"
        );
    }

    /// The canonical citations field is opaque, so a value that is not an
    /// object with a boolean `enabled` gets no guessed interpretation: the
    /// member is omitted and the loss is logged (per-document at DEBUG, plus
    /// the aggregated per-request WARN).
    #[traced_test]
    #[test]
    fn malformed_document_citations_omits_the_member_and_logs() {
        // Arrange
        let messages = vec![document_message(Some(json!("yes")))];

        // Act
        let document = document_block_json(&messages);

        // Assert
        assert!(
            document.get("citations").is_none(),
            "a malformed citations value must not be guessed at: {document}"
        );
        assert!(
            logs_contain("dropping unrecognized document citations value"),
            "the dropped citations config must be observable in the logs"
        );
    }

    /// A message-content document is one of three paths that can carry a
    /// malformed citations value; a request can hold an unbounded number of
    /// them, so the operator-facing WARN is aggregated per request rather
    /// than emitted per document. N malformed documents -> exactly one WARN.
    #[test]
    fn many_malformed_document_citations_emit_one_aggregated_warn() {
        // Arrange
        const N: usize = 50;
        let messages: Vec<Message> = (0..N)
            .map(|_| document_message(Some(json!("yes"))))
            .collect();

        // Act
        let events = routectl_testkit::capture_events(|| {
            build_messages(TEST_ID, &messages).expect("messages must translate");
        });

        // Assert
        let warns: Vec<_> = events
            .iter()
            .filter(|e| {
                e.level == tracing::Level::WARN
                    && e.message
                        .contains("dropping unrecognized document citations value")
            })
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "{N} malformed-citations documents must emit exactly one aggregated WARN"
        );
        assert_eq!(
            warns[0].field("dropped_count"),
            Some(N.to_string().as_str()),
            "the aggregated WARN must carry the dropped count"
        );
    }

    /// Same bound on the tool_result paths: a `Role::Tool` turn whose Parts
    /// carry N malformed-citations documents, plus a raw Anthropic-shape
    /// tool_result content array carrying N more, still emit one WARN for
    /// the whole request.
    #[test]
    fn many_malformed_citations_across_tool_result_paths_emit_one_aggregated_warn() {
        // Arrange
        const N: usize = 40;
        let malformed = json!("yes");
        let malformed_document_part = || {
            ContentPart::Known(routectl_core::KnownContentPart::Document {
                source: json!({
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": "JVBERi0xLjQ=",
                }),
                title: Some("notes".into()),
                citations: Some(malformed.clone()),
                cache_control: None,
            })
        };
        let canonical_parts_turn = Message {
            role: Role::Tool,
            tool_call_id: Some("tu_1".into()),
            content: MessageContent::Parts((0..N).map(|_| malformed_document_part()).collect()),
            ..user_msg()
        };
        let raw_array_turn = Message {
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                routectl_core::KnownContentPart::ToolResult {
                    tool_use_id: "tu_2".into(),
                    content: Value::Array(
                        (0..N)
                            .map(|_| raw_document_element(Some(malformed.clone())))
                            .collect(),
                    ),
                    is_error: None,
                    cache_control: None,
                },
            )]),
            ..user_msg()
        };
        let messages = vec![canonical_parts_turn, raw_array_turn];

        // Act
        let events = routectl_testkit::capture_events(|| {
            build_messages(TEST_ID, &messages).expect("messages must translate");
        });

        // Assert
        let warns: Vec<_> = events
            .iter()
            .filter(|e| {
                e.level == tracing::Level::WARN
                    && e.message
                        .contains("dropping unrecognized document citations value")
            })
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "both tool_result document paths must share the one per-request WARN"
        );
        assert_eq!(
            warns[0].field("dropped_count"),
            Some((2 * N).to_string().as_str()),
            "the aggregated count must span every document path in the request"
        );
    }

    /// The aggregate WARN is the only operator-facing record of the loss, so
    /// it must survive a later translation failure: a recorded drop followed
    /// by a `Role::Tool` turn missing its `tool_call_id` still emits exactly
    /// one WARN carrying the count observed before the error.
    #[test]
    fn recorded_drop_still_warns_when_a_later_message_fails_to_translate() {
        // Arrange
        let messages = vec![
            document_message(Some(json!("yes"))),
            Message {
                role: Role::Tool,
                tool_call_id: None,
                content: MessageContent::Text("result".into()),
                ..user_msg()
            },
        ];

        // Act
        let events = routectl_testkit::capture_events(|| {
            build_messages(TEST_ID, &messages)
                .expect_err("a tool message without tool_call_id must fail translation");
        });

        // Assert
        let warns: Vec<_> = events
            .iter()
            .filter(|e| {
                e.level == tracing::Level::WARN
                    && e.message
                        .contains("dropping unrecognized document citations value")
            })
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "a translation error must not swallow the aggregated citations WARN"
        );
        assert_eq!(
            warns[0].field("dropped_count"),
            Some("1"),
            "the aggregated WARN must carry the count recorded before the error"
        );
    }

    /// Build a RAW Anthropic-shape document element for a tool_result
    /// content array with the given `citations` value.
    fn raw_document_element(citations: Option<Value>) -> Value {
        let mut element = json!({
            "type": "document",
            "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": "JVBERi0xLjQ=",
            },
            "title": "notes",
        });
        if let Some(citations) = citations {
            element["citations"] = citations;
        }
        element
    }

    /// Serialize the document emitted by the raw tool_result array path so
    /// assertions see the exact wire shape.
    fn raw_element_document_json(citations: Option<Value>) -> Value {
        let out = translate_tool_result_array_element(
            &raw_document_element(citations),
            &mut CitationsDropTally::new("test"),
        );
        let ConverseToolResultContent::Document { document } = out else {
            panic!("expected a Document toolResult variant, got: {out:?}");
        };
        document
    }

    /// A RAW Anthropic-shape document inside a tool_result content array
    /// must carry its citations config onto the wire, exactly like the two
    /// canonical `KnownContentPart::Document` paths -- previously this path
    /// never read the sibling `citations` key and silently dropped it.
    #[test]
    fn raw_tool_result_document_citations_enabled_reaches_wire() {
        // Arrange / Act
        let document = raw_element_document_json(Some(json!({"enabled": true})));

        // Assert
        assert_eq!(
            document["citations"],
            json!({"enabled": true}),
            "raw tool_result document must carry citations onto the wire: {document}"
        );
    }

    /// The raw path emits the same wire bytes as the canonical tool_result
    /// path for an equivalent input -- one shape, one helper, no drift.
    #[test]
    fn raw_tool_result_document_matches_canonical_wire_shape() {
        // Arrange
        let citations = json!({"enabled": true});

        // Act
        let from_raw = raw_element_document_json(Some(citations.clone()));
        let from_canonical = tool_result_document_json(&[document_tool_message(Some(citations))]);

        // Assert
        assert_eq!(
            from_raw, from_canonical,
            "raw and canonical tool_result paths must emit identical documents"
        );
    }

    /// Absent citations omit the optional member on the raw path, matching
    /// the canonical paths (same helper, no second mapping).
    #[test]
    fn raw_tool_result_document_without_citations_omits_the_member() {
        // Arrange / Act
        let document = raw_element_document_json(None);

        // Assert
        assert!(
            document.get("citations").is_none(),
            "absent citations must not emit the member: {document}"
        );
        assert_eq!(
            document["format"], "pdf",
            "the rest of the document block is unchanged: {document}"
        );
    }

    /// An explicit `{enabled: false}` is behaviorally identical to absence
    /// on the raw path too, so the member is omitted rather than emitted.
    #[test]
    fn raw_tool_result_document_citations_disabled_omits_the_member() {
        // Arrange / Act
        let document = raw_element_document_json(Some(json!({"enabled": false})));

        // Assert
        assert!(
            document.get("citations").is_none(),
            "citations:false must not emit the member: {document}"
        );
    }

    /// The emitted document object is closed: exactly `format`, `name` and
    /// `source`, plus `citations` only when citations are enabled. The
    /// single shared constructor merges no caller-supplied keys, and this
    /// assertion is what pins that -- the variant carries an opaque `Value`,
    /// so no type would catch a stray member leaking onto the wire.
    #[test]
    fn tool_result_document_member_set_is_closed() {
        // Arrange
        let expected_base = ["format", "name", "source"];

        // Act
        let without = raw_element_document_json(None);
        let disabled = raw_element_document_json(Some(json!({"enabled": false})));
        let enabled = raw_element_document_json(Some(json!({"enabled": true})));

        // Assert. Keys are sorted before comparing so this pins the member
        // SET, not the map's iteration order -- the invariant is which
        // members reach the wire.
        let sorted_members = |label: &str, document: &Value| -> Vec<String> {
            let mut members: Vec<String> = document
                .as_object()
                .unwrap_or_else(|| panic!("{label} document must be an object: {document}"))
                .keys()
                .cloned()
                .collect();
            members.sort_unstable();
            members
        };

        for (label, document) in [("absent", &without), ("disabled", &disabled)] {
            assert_eq!(
                sorted_members(label, document),
                expected_base,
                "{label} citations must emit exactly {expected_base:?}: {document}"
            );
        }

        assert_eq!(
            sorted_members("enabled", &enabled),
            ["citations", "format", "name", "source"],
            "enabled citations add exactly one member: {enabled}"
        );
    }

    /// A malformed citations value on the raw path gets no guessed
    /// interpretation: the member is omitted and the loss is logged, via the
    /// same shared helper the canonical paths use.
    #[traced_test]
    #[test]
    fn raw_tool_result_document_malformed_citations_omits_the_member_and_logs() {
        // Arrange / Act
        let document = raw_element_document_json(Some(json!("yes")));

        // Assert
        assert!(
            document.get("citations").is_none(),
            "a malformed citations value must not be guessed at: {document}"
        );
        assert!(
            logs_contain("dropping unrecognized document citations value"),
            "the dropped citations config must be observable in the logs"
        );
    }
}

#[cfg(test)]
#[path = "messages_tests.rs"]
mod sidecar_tests;
