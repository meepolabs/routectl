//! Canonical `messages[]` -> Converse `messages[]` translation.
//!
//! Per-role dispatch: User and Assistant turns ride through
//! `build_content_blocks`; Role::System is dropped here (lifted into
//! the top-level `system` array by `system.rs`); Role::Tool becomes a
//! synthesized user-role message carrying a `toolResult` block.
//!
//! Forward-compat catchalls: `ContentPart::Other` and unsupported known
//! parts (e.g. `Document` when the canonical title/source can't be
//! mapped) drop with a tracing diagnostic; the caller sees a partial
//! body rather than a translation failure. Cache breakpoints survive as
//! sibling `{cachePoint}` entries.

use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use base64::Engine;
use serde_json::Value;

use routectl_core::{
    ContentPart, Error, KnownContentPart, Message, MessageContent, ReasoningDetail,
    ReasoningDetailKind, Result, Role,
};

use crate::anthropic_api::parts::strip_text_after_tool_use;

use super::types::{
    CachePoint, ConverseContentBlock, ConverseDocument, ConverseDocumentSource, ConverseImage,
    ConverseImageSource, ConverseMessage, ConverseRequestReasoningBlock,
    ConverseRequestReasoningText, ConverseToolResult, ConverseToolResultContent, ConverseToolUse,
};

/// Translate every message in `req.messages` into a `ConverseMessage`,
/// dropping Role::System (handled by the top-level `system` array) and
/// rejecting Role::Tool messages without a `tool_call_id` (AWS rejects
/// empty `toolUseId` with a 400). Messages whose translated content
/// vec is empty (canonical Null content, or every typed Part dropped
/// during translation) are skipped entirely -- AWS Converse rejects
/// `content: []` with "Member must have at least 1 element."
pub(super) fn build_messages(id: &str, messages: &[Message]) -> Result<Vec<ConverseMessage>> {
    let mut out: Vec<ConverseMessage> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role {
            Role::System => {
                // System lives in the top-level `system` array. Drop here
                // so we don't duplicate; direct callers without an
                // ingress have already had their System messages lifted
                // by `build_system` via `lift_legacy_system`.
            }
            Role::User => {
                let mut blocks = build_user_content_blocks(id, &msg.content)?;
                ensure_document_has_text_sibling(&mut blocks);
                if blocks.is_empty() {
                    tracing::debug!(
                        provider = id,
                        role = "user",
                        "skipping empty message after translation"
                    );
                    continue;
                }
                out.push(ConverseMessage {
                    role: "user".to_string(),
                    content: blocks,
                });
            }
            Role::Assistant => {
                let blocks = build_assistant_content_blocks(id, msg)?;
                if blocks.is_empty() {
                    tracing::debug!(
                        provider = id,
                        role = "assistant",
                        "skipping empty message after translation"
                    );
                    continue;
                }
                out.push(ConverseMessage {
                    role: "assistant".to_string(),
                    content: blocks,
                });
            }
            Role::Tool => out.push(build_tool_message(msg)?),
        }
    }
    Ok(out)
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
) -> Result<Vec<ConverseContentBlock>> {
    content_blocks_with_cache_control(id, content)
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
fn build_assistant_content_blocks(id: &str, msg: &Message) -> Result<Vec<ConverseContentBlock>> {
    if !msg.reasoning_details.is_empty() {
        let mut blocks = emit_reasoning_blocks_converse(id, &msg.reasoning_details)?;
        append_converse_content_blocks(id, &msg.content, &mut blocks)?;
        return Ok(blocks);
    }
    if let MessageContent::Parts(parts) = &msg.content {
        let cleaned = strip_text_after_tool_use(parts);
        return content_blocks_from_parts(id, &cleaned);
    }
    content_blocks_with_cache_control(id, &msg.content)
}

/// Translate `reasoning_details` into Bedrock Converse `ReasoningContent`
/// blocks for echo on a multi-turn assistant turn. Index-ordered so an
/// upstream that re-orders reasoning blocks doesn't surprise the
/// downstream signature check. Only `anthropic-claude-v1` format details
/// are emitted; others (e.g. OpenAI-format) are skipped -- they have no
/// Converse wire equivalent. Bedrock validates the signature on multi-turn
/// replay identical to direct Anthropic; a missing signature 400s with
/// "invalid reasoning content". Unsigned blocks are skipped and the count
/// is aggregated into a single WARN so the operator can correlate without
/// per-detail log spam.
fn emit_reasoning_blocks_converse(
    id: &str,
    details: &[ReasoningDetail],
) -> Result<Vec<ConverseContentBlock>> {
    let mut sorted = details.to_vec();
    sorted.sort_by_key(|d| d.index.unwrap_or(0));

    let mut blocks: Vec<ConverseContentBlock> = Vec::with_capacity(sorted.len());
    let mut skipped_unsigned: Vec<Option<u32>> = Vec::new();
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
                    skipped_unsigned.push(detail.index);
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
    if !skipped_unsigned.is_empty() {
        tracing::warn!(
            provider = id,
            skipped_count = skipped_unsigned.len(),
            skipped_indices = ?skipped_unsigned,
            "skipping Thinking blocks on Converse replay: signature missing or empty; \
             Bedrock Converse requires a signature on replayed reasoningContent blocks"
        );
    }
    Ok(blocks)
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
) -> Result<()> {
    match content {
        MessageContent::Text(t) if !t.is_empty() => {
            blocks.push(ConverseContentBlock::Text { text: t.clone() });
        }
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            let more = content_blocks_from_parts(id, &cleaned)?;
            blocks.extend(more);
        }
    }
    Ok(())
}

fn content_blocks_with_cache_control(
    id: &str,
    content: &MessageContent,
) -> Result<Vec<ConverseContentBlock>> {
    match content {
        MessageContent::Text(t) => Ok(vec![ConverseContentBlock::Text { text: t.clone() }]),
        MessageContent::Null => Ok(Vec::new()),
        MessageContent::Parts(parts) => content_blocks_from_parts(id, parts),
    }
}

/// Walk a slice of canonical `ContentPart` into Converse blocks. When a
/// part translates successfully AND carries a `cache_control` marker, a
/// sibling `{cachePoint}` block is emitted IMMEDIATELY AFTER the
/// translated block (avoids the orphan-cachePoint shape that AWS
/// rejects when a translation drops the underlying block).
fn content_blocks_from_parts(id: &str, parts: &[ContentPart]) -> Result<Vec<ConverseContentBlock>> {
    let mut out: Vec<ConverseContentBlock> = Vec::with_capacity(parts.len());
    for p in parts {
        if let Some(block) = translate_content_part(id, p)? {
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
fn translate_content_part(id: &str, p: &ContentPart) -> Result<Option<ConverseContentBlock>> {
    match p {
        ContentPart::Known(k) => translate_known_part(id, k),
        ContentPart::Other { type_tag, .. } => {
            tracing::warn!(
                provider = id,
                type_tag = %type_tag,
                "dropping unknown ContentPart::Other on Converse egress; \
                 forward-compat block types not yet modeled"
            );
            Ok(None)
        }
    }
}

fn translate_known_part(id: &str, k: &KnownContentPart) -> Result<Option<ConverseContentBlock>> {
    match k {
        KnownContentPart::Text { text, .. } => {
            Ok(Some(ConverseContentBlock::Text { text: text.clone() }))
        }
        KnownContentPart::Image { source, .. } => Ok(translate_image_source(id, source)),
        KnownContentPart::ImageUrl { image_url, .. } => Ok(translate_image_url(id, image_url)),
        KnownContentPart::Document { source, title, .. } => {
            Ok(translate_document(id, source, title.as_deref()))
        }
        KnownContentPart::ToolUse {
            id: tu_id,
            name,
            input,
            ..
        } => Ok(Some(ConverseContentBlock::ToolUse {
            tool_use: ConverseToolUse {
                tool_use_id: tu_id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
        })),
        KnownContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => Ok(Some(ConverseContentBlock::ToolResult {
            tool_result: ConverseToolResult {
                tool_use_id: tool_use_id.clone(),
                content: translate_tool_result_content(content),
                status: is_error.map(|e| if e { "error".into() } else { "success".into() }),
            },
        })),
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
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((mt, b64)) = rest.split_once(";base64,") {
            if let Some(format) = media_type_to_image_format(mt) {
                return Some(ConverseContentBlock::Image {
                    image: ConverseImage {
                        format,
                        source: ConverseImageSource {
                            bytes: b64.to_string(),
                        },
                    },
                });
            }
        }
    }
    tracing::warn!(
        provider = id,
        "dropping image_url on Converse egress; only base64 data URIs are supported"
    );
    None
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
    let bytes = match kind {
        "base64" => raw_data.to_string(),
        "text" => B64_STANDARD.encode(raw_data.as_bytes()),
        other => {
            tracing::warn!(
                provider = id,
                source_type = %other,
                "dropping unsupported document source type on Converse egress; \
                 AWS Converse JSON wire accepts only base64 or text sources"
            );
            return None;
        }
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
        },
    })
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

/// AWS validates `document.name` against `^[a-zA-Z0-9-()[\]_ ]{1,200}$`.
/// Map a canonical `title` to a safe name -- replace disallowed chars
/// with `_`, truncate at 200, default to `"document"` when title is
/// missing or fully scrubbed.
fn sanitize_document_name(title: Option<&str>) -> String {
    let raw = title.unwrap_or("").trim();
    if raw.is_empty() {
        return "document".to_string();
    }
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '(' | ')' | '[' | ']' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .take(200)
        .collect();
    if cleaned.trim().is_empty() {
        "document".to_string()
    } else {
        cleaned
    }
}

fn translate_tool_result_content(content: &Value) -> Vec<ConverseToolResultContent> {
    match content {
        Value::String(s) => vec![ConverseToolResultContent::Text { text: s.clone() }],
        Value::Array(arr) => arr
            .iter()
            .map(translate_tool_result_array_element)
            .collect(),
        Value::Null => Vec::new(),
        other => vec![ConverseToolResultContent::Json {
            json: other.clone(),
        }],
    }
}

/// Translate one element from an Anthropic-shape tool_result content
/// array. Anthropic clients send blocks like `{"type":"text","text":"..."}`
/// or `{"type":"image","source":{...}}`. The naive `Json` wrap loses
/// type discrimination and AWS rejects multimodal tool results on
/// Claude 3+. Dispatch on the `type` tag so each shape lands in the
/// correct AWS variant; bare strings stay as Text; unknown shapes fall
/// to Json.
fn translate_tool_result_array_element(v: &Value) -> ConverseToolResultContent {
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
            let title = obj.get("title").and_then(|t| t.as_str());
            ConverseToolResultContent::Document {
                document: serde_json::json!({
                    "format": format,
                    "name": sanitize_document_name(title),
                    "source": {"bytes": data},
                }),
            }
        }
        _ => ConverseToolResultContent::Json { json: v.clone() },
    }
}

/// Build a synthetic user-role message from a canonical `Role::Tool`
/// turn. Returns an error when `tool_call_id` is missing -- AWS rejects
/// `toolResult.toolUseId == ""` and the silent fallback that produced
/// an empty string upstream-failed with a vague 400.
fn build_tool_message(msg: &Message) -> Result<ConverseMessage> {
    let Some(tool_use_id) = msg.tool_call_id.as_ref().filter(|s| !s.is_empty()).cloned() else {
        return Err(routectl_core::Error::NormalizeRequest(
            "bedrock-converse".to_string(),
            "tool message missing tool_call_id (Role::Tool requires \
             non-empty toolUseId for AWS Converse)"
                .to_string(),
        ));
    };
    let content = match &msg.content {
        MessageContent::Text(t) => vec![ConverseToolResultContent::Text { text: t.clone() }],
        MessageContent::Parts(parts) => parts.iter().map(translate_part_for_tool_result).collect(),
        MessageContent::Null => Vec::new(),
    };
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
fn translate_part_for_tool_result(p: &ContentPart) -> ConverseToolResultContent {
    match p {
        ContentPart::Known(KnownContentPart::Text { text, .. }) => {
            ConverseToolResultContent::Text { text: text.clone() }
        }
        ContentPart::Known(KnownContentPart::Image { source, .. }) => {
            image_source_to_tool_result(source).unwrap_or_else(|| content_part_to_json_fallback(p))
        }
        ContentPart::Known(KnownContentPart::Document { source, title, .. }) => {
            document_to_tool_result(source, title.as_deref())
                .unwrap_or_else(|| content_part_to_json_fallback(p))
        }
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

/// Translate a canonical Document part (source + title) into the AWS
/// toolResult `Document` variant. Returns None for non-base64 sources
/// or unmappable media types.
fn document_to_tool_result(
    source: &Value,
    title: Option<&str>,
) -> Option<ConverseToolResultContent> {
    let obj = source.as_object()?;
    let kind = obj.get("type").and_then(|v| v.as_str())?;
    if kind != "base64" {
        return None;
    }
    let media_type = obj.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
    let data = obj.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let format = media_type_to_document_format(media_type)?;
    Some(ConverseToolResultContent::Document {
        document: serde_json::json!({
            "format": format,
            "name": sanitize_document_name(title),
            "source": {"bytes": data},
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ReasoningDetail, ReasoningDetailKind};
    use serde_json::json;

    fn user_msg() -> Message {
        Message {
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
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "content-path thinking".into(),
                        signature: Some("sig_content".into()),
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "result".into(),
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

    /// An image whose media_type doesn't map to an AWS image format is
    /// dropped (the caller-contract promises a tracing diagnostic on every
    /// drop). A sibling Text block confirms only the image was dropped.
    #[test]
    fn image_with_unmapped_media_type_is_dropped() {
        // Arrange
        use routectl_core::KnownContentPart;
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "look at this".into(),
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
}
