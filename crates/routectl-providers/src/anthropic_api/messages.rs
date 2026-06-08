//! Canonical `messages[]` -> Anthropic wire `messages[]` translation.
//!
//! Per-role dispatch (`translate_messages`): User content rides through
//! `translate_simple_content`; Assistant turns route through
//! `build_assistant_content` (which threads `reasoning_details` back as
//! Thinking / RedactedThinking blocks and re-emits OpenAI-shape
//! `tool_calls` as ToolUse blocks for multi-turn replay); Role::System
//! is dropped here (already lifted into `req.system` by `system.rs`);
//! Role::Tool becomes a synthesized user-role message carrying a
//! tool_result block.
//!
//! `normalize_replay_invariants` applies two outgoing invariants before
//! translation: a hard reject for tool_result messages missing a
//! tool_call_id, and (gated on `history_reasoning`) a strip of unsigned
//! Thinking blocks that real Anthropic would 400 on replay. Forward-
//! compat: `ContentPart::Other` passes through verbatim via
//! `ContentBlock::Other`.

use std::borrow::Cow;

use serde_json::{json, Value};

use routectl_core::{
    ChatRequest, ContentPart, CoreHistoryReasoning, Error, KnownContentPart, Message,
    MessageContent, ReasoningDetail, ReasoningDetailKind, Result, Role,
};

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
/// - STRIP any `Thinking` content block whose `signature` is missing or
///   empty from each message's `Parts` content -- UNLESS
///   `history_reasoning` is `Preserve`. Cross-provider fallback (a prior
///   turn handled by deepseek which signs with its own uuid format, then
///   the next turn falls back to Anthropic) and SDKs that fail to
///   round-trip the signature field would otherwise 400 real Anthropic
///   with a confusing upstream error. Strip drops just the offending
///   block; signed thinking blocks pass through unchanged and so does
///   every other block type.
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
/// carrying the provider id, the count of dropped blocks, and the
/// affected message indices. Block content is never logged (could be
/// reasoning over sensitive data). Preserve strips nothing, so the WARN
/// does not fire under Preserve.
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
    let mut affected_messages: Vec<usize> = Vec::new();
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
            affected_messages.push(i);
        }
        let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
        let has_reasoning = !msg.reasoning_details.is_empty();
        if kept.is_empty() && !has_tool_calls && !has_reasoning {
            // Stripping emptied this message and there's no other
            // content source. Anthropic's wire spec rejects
            // content: [] for both user and assistant roles; emit
            // nothing rather than trade one 400 for another.
            continue;
        }
        out.push(Message {
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
        affected_messages = ?affected_messages,
        "stripping unsigned thinking blocks from outgoing request: \
         Anthropic requires a signature on replayed Thinking blocks. \
         Cross-provider fallback or SDKs that fail to round-trip the \
         signature field would otherwise 400 the request. Routectl \
         drops just the unsigned blocks; signed thinking blocks and \
         other content pass through unchanged."
    );

    Ok(Cow::Owned(out))
}

/// True iff `p` is a `Thinking` block whose `signature` is missing
/// or empty. Pulled out so the pre-scan and the rebuild walk share a
/// single predicate.
fn is_unsigned_thinking_part(p: &ContentPart) -> bool {
    matches!(
        p,
        ContentPart::Known(KnownContentPart::Thinking { signature, .. })
            if signature.as_deref().unwrap_or("").is_empty()
    )
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

fn translate_content_part(p: &ContentPart) -> ContentBlock {
    match p {
        ContentPart::Known(k) => translate_known_part(k),
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

fn translate_known_part(k: &KnownContentPart) -> ContentBlock {
    match k {
        KnownContentPart::Text {
            text,
            cache_control,
        } => ContentBlock::Text {
            text: text.clone(),
            cache_control: cache_control.clone(),
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
        } => match parse_file_document_source(file) {
            Some((source, title)) => ContentBlock::Document {
                source,
                title,
                citations: None,
                cache_control: cache_control.clone(),
            },
            None => {
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
        },
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
            data: data.clone(),
            cache_control: None,
        },
    }
}

/// Reconstruct an Anthropic content array for an assistant message that
/// carries reasoning_details (tool-use continuity). thinking blocks with
/// signatures must be passed back verbatim.
fn build_assistant_content(id: &str, msg: &Message) -> Result<AnthropicContent> {
    let has_tool_calls = msg
        .tool_calls
        .as_ref()
        .map(|tc| !tc.is_empty())
        .unwrap_or(false);
    if msg.reasoning_details.is_empty() && !has_tool_calls {
        // No multi-turn reasoning to thread back AND no OpenAI-shape
        // tool_calls field to re-emit; fall through to the generic
        // content translation (Text or Parts), but strip trailing
        // text-after-tool_use first (see helper docstring).
        return Ok(translate_assistant_simple_content(&msg.content));
    }

    let mut blocks = emit_reasoning_blocks(id, &msg.reasoning_details)?;
    append_assistant_message_blocks(&mut blocks, &msg.content);
    if let Some(tool_calls) = msg.tool_calls.as_ref() {
        emit_tool_use_blocks_from_calls(id, tool_calls, &mut blocks)?;
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
fn emit_tool_use_blocks_from_calls(
    id: &str,
    tool_calls: &[Value],
    blocks: &mut Vec<ContentBlock>,
) -> Result<()> {
    for call in tool_calls {
        let tool_id = call
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
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
                    tool_id = %tool_id,
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
fn emit_reasoning_blocks(id: &str, details: &[ReasoningDetail]) -> Result<Vec<ContentBlock>> {
    let mut sorted = details.to_vec();
    sorted.sort_by_key(|d| d.index.unwrap_or(0));

    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(sorted.len());
    let mut skipped_unsigned: Vec<Option<u32>> = Vec::new();
    // Track reasoning details dropped because their format is not
    // `anthropic-claude-v1`. These cannot be replayed as Anthropic blocks
    // regardless of signature presence; a separate WARN aggregates them so
    // operators can distinguish format-mismatch drops from unsigned drops.
    let mut skipped_format_count: usize = 0;
    let mut skipped_format_values: Vec<String> = Vec::new();
    for detail in &sorted {
        match detail.kind {
            ReasoningDetailKind::Text => {
                if detail.format.as_deref() != Some(super::ANTHROPIC_FORMAT) {
                    skipped_format_count = skipped_format_count.saturating_add(1);
                    skipped_format_values
                        .push(detail.format.as_deref().unwrap_or("<none>").to_string());
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
                    // Anthropic 400s on a Thinking block without a
                    // signature; skipping is better than a hard fail.
                    // Aggregate the WARN per-call (Claude 4.5 multi-
                    // block thinking turns can pile up several skipped
                    // entries and per-detail WARN would flood the log).
                    skipped_unsigned.push(detail.index);
                    continue;
                }
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature: signature.to_string(),
                    cache_control: None,
                });
            }
            ReasoningDetailKind::Encrypted => {
                if detail.format.as_deref() != Some(super::ANTHROPIC_FORMAT) {
                    skipped_format_count = skipped_format_count.saturating_add(1);
                    skipped_format_values
                        .push(detail.format.as_deref().unwrap_or("<none>").to_string());
                    continue;
                }
                let data = detail
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                blocks.push(ContentBlock::RedactedThinking {
                    data,
                    cache_control: None,
                });
            }
            ReasoningDetailKind::Summary => {
                // Not an Anthropic block; skip.
            }
        }
    }
    if !skipped_unsigned.is_empty() {
        tracing::warn!(
            provider = id,
            skipped_count = skipped_unsigned.len(),
            skipped_indices = ?skipped_unsigned,
            "skipping Thinking blocks on replay: signature missing or empty \
             (multi-block thinking history is now partially echoed; \
             see CLAUDE.md \"Anthropic streaming reasoning replay\" residual)"
        );
    }
    if skipped_format_count > 0 {
        // Deduplicate format strings for a compact log field; order is
        // not meaningful so sort-then-dedup is fine.
        skipped_format_values.sort_unstable();
        skipped_format_values.dedup();
        tracing::warn!(
            provider = id,
            skipped_count = skipped_format_count,
            skipped_formats = ?skipped_format_values,
            "skipping reasoning blocks on replay: format is not anthropic-claude-v1 \
             (non-Anthropic format details cannot be echoed as Anthropic Thinking blocks)"
        );
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
fn append_assistant_message_blocks(blocks: &mut Vec<ContentBlock>, content: &MessageContent) {
    match content {
        MessageContent::Text(t) if !t.is_empty() => blocks.push(ContentBlock::Text {
            text: t.clone(),
            cache_control: None,
        }),
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            for p in cleaned.iter() {
                blocks.push(translate_content_part(p));
            }
        }
    }
}

/// Assistant-message variant of `translate_simple_content` that strips
/// trailing text-after-tool_use before per-part translation. Called
/// only from `build_assistant_content`. Text/Null arms delegate to
/// `translate_simple_content` so the two stay in lockstep -- only the
/// `Parts` arm needs the strip.
fn translate_assistant_simple_content(c: &MessageContent) -> AnthropicContent {
    match c {
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            AnthropicContent::Blocks(cleaned.iter().map(translate_content_part).collect())
        }
        // Text/Null arms are identical to `translate_simple_content`;
        // delegate to keep them in one place.
        _ => translate_simple_content(c),
    }
}

/// Translate plain message content (no multi-turn reasoning context).
/// Text -> AnthropicContent::Text (cheaper wire form). Parts ->
/// AnthropicContent::Blocks via per-part translation.
fn translate_simple_content(c: &MessageContent) -> AnthropicContent {
    match c {
        MessageContent::Text(t) => AnthropicContent::Text(t.clone()),
        MessageContent::Null => AnthropicContent::Text(String::new()),
        MessageContent::Parts(parts) => {
            AnthropicContent::Blocks(parts.iter().map(translate_content_part).collect())
        }
    }
}

// ---------------------------------------------------------------------------
// Tool-role messages
// ---------------------------------------------------------------------------

fn build_tool_message(msg: &Message) -> AnthropicMessage {
    let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
    // Anthropic tool_result.content accepts either a string or an array
    // of content blocks. We honor whichever shape the canonical message
    // carries.
    let content_val = match &msg.content {
        MessageContent::Text(t) => Value::String(t.clone()),
        MessageContent::Parts(parts) => Value::Array(
            parts
                .iter()
                .map(|p| serde_json::to_value(translate_content_part(p)).unwrap_or(Value::Null))
                .collect(),
        ),
        MessageContent::Null => Value::Null,
    };
    AnthropicMessage {
        role: AnthropicRole::User,
        content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id,
            content: content_val,
            cache_control: None,
            is_error: None,
        }]),
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
pub(super) fn translate_messages(id: &str, messages: &[Message]) -> Result<Vec<AnthropicMessage>> {
    let mut out: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role {
            Role::System => {
                // Already handled via req.system / lift_legacy_system.
                // Drop here (do not duplicate in the messages array).
            }
            Role::User => out.push(AnthropicMessage {
                role: AnthropicRole::User,
                content: translate_simple_content(&msg.content),
            }),
            Role::Assistant => out.push(AnthropicMessage {
                role: AnthropicRole::Assistant,
                content: build_assistant_content(id, msg)?,
            }),
            Role::Tool => out.push(build_tool_message(msg)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod translate_file_part_tests {
    use super::translate_content_part;
    use super::ContentBlock;
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
        match translate_content_part(&part) {
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
        match translate_content_part(&part) {
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
        match translate_content_part(&part) {
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
        match translate_content_part(&part) {
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
        match translate_content_part(&part) {
            ContentBlock::Document { cache_control, .. } => {
                assert!(cache_control.is_some());
            }
            other => panic!("expected Document, got {other:?}"),
        }
    }
}
