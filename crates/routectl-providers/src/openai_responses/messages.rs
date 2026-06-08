//! Canonical `messages[]` -> Responses `input[]` translation.
//!
//! The Responses API has no role-tagged message envelope: each
//! `input[]` item is a top-level tagged union (`message` / `reasoning`
//! / `function_call` / `function_call_output`). User and assistant
//! turns become `Message` items with `role: "user" | "assistant"` and
//! content blocks; assistant `thinking` parts become `Reasoning`
//! items (with the canonical signature lifted into
//! `encrypted_content`); tool messages become `FunctionCallOutput`
//! items. ToolUse content parts on an assistant turn become standalone
//! `FunctionCall` items emitted alongside the `Message` for that turn;
//! OpenAI-shape `Message.tool_calls` (populated by the OpenAI ingress
//! instead of ToolUse content parts) are re-emitted the same way so a
//! following `function_call_output` is never dangling.
//!
//! Reasoning replay: codex re-injects reasoning blocks only when
//! `encrypted_content` is non-empty (see
//! `codex/codex-rs/core/src/arc_monitor.rs::325-336`). routectl always
//! emits the field; an empty string is the documented "no prior
//! signature" shape. Canonical Thinking blocks without a signature
//! (first-turn requests, or providers that didn't surface one) flow
//! through cleanly as empty-string `encrypted_content`.

use serde_json::Value;

use routectl_core::{ContentPart, Error, KnownContentPart, Message, MessageContent, Result, Role};

use super::types::{
    FunctionCallOutputBody, FunctionCallOutputContentItem, ReasoningContentItem,
    ReasoningSummaryItem, ResponseInputItem, ResponsesContentItem,
};
use super::OPENAI_RESPONSES_FORMAT;
use routectl_core::{ReasoningDetail, ReasoningDetailKind};

/// Walk the canonical `messages[]` and produce a flat `input[]` array
/// in Responses-shape. System messages are dropped here (lifted into
/// `instructions` by `system.rs`); each non-system turn may produce
/// 1+ input items (e.g. an assistant turn with both thinking and text
/// emits `Reasoning` + `Message`).
pub(super) fn build_input(id: &str, messages: &[Message]) -> Result<Vec<ResponseInputItem>> {
    let mut out: Vec<ResponseInputItem> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role {
            Role::System => {
                // Lifted into top-level `instructions` by system.rs;
                // intentionally dropped here.
            }
            Role::User => translate_user_message(id, msg, &mut out)?,
            Role::Assistant => translate_assistant_message(id, msg, &mut out)?,
            Role::Tool => translate_tool_message(id, msg, &mut out)?,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Per-role translation
// ---------------------------------------------------------------------------

fn translate_user_message(id: &str, msg: &Message, out: &mut Vec<ResponseInputItem>) -> Result<()> {
    // Lift any `tool_result` parts into FunctionCallOutput input items.
    // The Anthropic ingress emits tool outputs as
    // `ContentPart::ToolResult` on a user-role message (the Anthropic
    // wire shape); the Responses API needs them as separate
    // `function_call_output` input items keyed by call_id. Without
    // this lift, the upstream 400s with "No tool output found for
    // function call <id>".
    extract_tool_results(id, &msg.content, out);

    let content = build_user_content(id, &msg.content)?;
    if content.is_empty() {
        tracing::debug!(
            provider = id,
            role = "user",
            "skipping empty user message after Responses translation"
        );
        return Ok(());
    }
    out.push(ResponseInputItem::Message {
        role: "user".into(),
        content,
    });
    Ok(())
}

/// Walk a user-message content and emit one FunctionCallOutput input
/// item per `tool_result` part. The Anthropic Messages wire shape ships
/// tool outputs as user-turn content blocks; the Responses API wants
/// them as sibling input items, so we lift them out before
/// `build_user_content` walks the remaining parts.
fn extract_tool_results(id: &str, content: &MessageContent, out: &mut Vec<ResponseInputItem>) {
    let MessageContent::Parts(parts) = content else {
        return;
    };
    for p in parts {
        let ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id,
            content,
            ..
        }) = p
        else {
            continue;
        };
        if tool_use_id.is_empty() {
            tracing::warn!(
                provider = id,
                "dropping tool_result with empty tool_use_id on Responses egress"
            );
            continue;
        }
        let output = tool_result_to_output_body(id, content);
        out.push(ResponseInputItem::FunctionCallOutput {
            call_id: tool_use_id.clone(),
            output,
        });
    }
}

/// Translate the Anthropic-shape `tool_result.content` value into a
/// FunctionCallOutputBody. Anthropic's content slot is permissive: a
/// flat string, an array of blocks, or any JSON value. Codex parity
/// prefers a flat string when possible.
fn tool_result_to_output_body(id: &str, content: &Value) -> FunctionCallOutputBody {
    if let Some(s) = content.as_str() {
        return FunctionCallOutputBody::Text(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        // Walk the array as if it were canonical parts. If every entry
        // is a `{type: "text", text: "..."}` block, collapse to a
        // flat string. Otherwise fall back to a JSON-encoded text body
        // so the upstream still sees the structured payload.
        let mut buf = String::new();
        let mut all_text = true;
        for v in arr {
            if let (Some("text"), Some(text)) = (
                v.get("type").and_then(Value::as_str),
                v.get("text").and_then(Value::as_str),
            ) {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(text);
            } else {
                all_text = false;
                break;
            }
        }
        if all_text {
            return FunctionCallOutputBody::Text(buf);
        }
    }
    // Anything else: serialize the value so the model gets the raw
    // structured output. Better than dropping the payload.
    let serialized = serde_json::to_string(content).unwrap_or_else(|e| {
        tracing::warn!(
            provider = id,
            error = %e,
            "tool_result content failed to serialize; emitting empty output"
        );
        String::new()
    });
    FunctionCallOutputBody::Text(serialized)
}

/// Assistant turn translation. Walks each part, splitting into a
/// reasoning-item stream + a message-content stream + a tool-call
/// stream, then emits items in the order [reasoning?, message?,
/// tool_calls...] so multi-turn replay preserves the original ordering
/// the model sees.
///
/// Reasoning replay (critical correctness path): the Responses-side
/// canonical channel for prior-turn reasoning is
/// `msg.reasoning_details` (the response translator stamps every
/// reasoning block there with `format = "openai-responses-v1"` and
/// preserves the upstream `encrypted_content` signature). Routing
/// reasoning solely through content-parts would lose the signature
/// because `ContentPart::Thinking` has no slot for the JWT payload.
///
/// Mutual-exclusion rule: `reasoning_details` (the response-side
/// channel) and `ContentPart::Thinking` (the request-side channel)
/// MUST NOT both populate on the same assistant turn. When they do,
/// prefer `reasoning_details` and skip Thinking parts to avoid
/// duplicate Reasoning items on the wire. Log at debug so the
/// duplicate is visible during triage.
fn translate_assistant_message(
    id: &str,
    msg: &Message,
    out: &mut Vec<ResponseInputItem>,
) -> Result<()> {
    let mut reasoning_items: Vec<ResponseInputItem> = Vec::new();
    let mut message_content: Vec<ResponsesContentItem> = Vec::new();
    let mut tool_calls: Vec<ResponseInputItem> = Vec::new();

    // Phase 1: lift reasoning_details into Reasoning input items.
    // Only entries tagged with the Responses format participate; other
    // formats (e.g. Anthropic) ride a different replay shape that the
    // canonical hub doesn't translate here.
    lift_reasoning_details(&msg.reasoning_details, &mut reasoning_items);
    let suppress_thinking_parts = !reasoning_items.is_empty();

    let mut content_has_tool_use = false;
    match &msg.content {
        MessageContent::Text(t) if !t.is_empty() => {
            message_content.push(ResponsesContentItem::OutputText { text: t.clone() });
        }
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            content_has_tool_use = parts
                .iter()
                .any(|p| matches!(p, ContentPart::Known(KnownContentPart::ToolUse { .. })));
            for p in parts {
                walk_assistant_part(
                    id,
                    p,
                    suppress_thinking_parts,
                    &mut reasoning_items,
                    &mut message_content,
                    &mut tool_calls,
                )?;
            }
        }
    }

    // Re-emit OpenAI-shape `Message.tool_calls` as `function_call` input
    // items. The OpenAI ingress populates `tool_calls` rather than
    // emitting `KnownContentPart::ToolUse` content parts; without this a
    // turn whose calls live only on `tool_calls` produces no
    // `function_call`, and the following `function_call_output` is
    // dangling ("No tool output found for function call <id>"). The guard
    // skips re-emission when content already carried ToolUse parts (the
    // walk above already pushed those) so the call isn't doubled. The
    // Responses wire wants `arguments` as a JSON STRING, so the parsed
    // value is re-serialized -- consistent with the ToolUse-part path,
    // which also `serde_json::to_string`s its input.
    append_function_calls_from_tool_calls(id, msg, content_has_tool_use, &mut tool_calls)?;

    out.extend(reasoning_items);
    if !message_content.is_empty() {
        out.push(ResponseInputItem::Message {
            role: "assistant".into(),
            content: message_content,
        });
    }
    out.extend(tool_calls);
    Ok(())
}

/// Re-emit OpenAI-shape `Message.tool_calls` as Responses `function_call`
/// input items. See `translate_assistant_message` for the orphaned-output
/// failure this prevents. No-op when `tool_calls` is empty or when the
/// content already carried `ToolUse` parts (avoids double-emission).
fn append_function_calls_from_tool_calls(
    id: &str,
    msg: &Message,
    content_has_tool_use: bool,
    tool_calls: &mut Vec<ResponseInputItem>,
) -> Result<()> {
    let Some(raw_calls) = msg.tool_calls.as_ref().filter(|tc| !tc.is_empty()) else {
        return Ok(());
    };
    if content_has_tool_use {
        return Ok(());
    }
    for call in crate::tool_calls::normalize_tool_calls(id, raw_calls) {
        tool_calls.push(ResponseInputItem::FunctionCall {
            call_id: call.id,
            name: call.name,
            arguments: serde_json::to_string(&call.arguments)
                .map_err(|e| Error::normalize_request(id, e.to_string()))?,
        });
    }
    Ok(())
}

/// Walk `reasoning_details` and emit one Reasoning input item per
/// distinct upstream item id (or one fall-through item when no id is
/// preserved). Multiple details sharing the same `id` collapse to a
/// single Reasoning item carrying the union of summary, content, and
/// encrypted_content surfaces. Format-tagged with anything other than
/// `openai-responses-v1` is skipped: those entries come from a
/// different upstream and replaying them here would corrupt the wire.
fn lift_reasoning_details(details: &[ReasoningDetail], out: &mut Vec<ResponseInputItem>) {
    if details.is_empty() {
        return;
    }
    // Bucket by id (None-id details ride a single unnamed bucket).
    // Preserve arrival order via the `order` vector so output is
    // deterministic.
    let mut order: Vec<Option<String>> = Vec::new();
    let mut groups: std::collections::HashMap<Option<String>, ReasoningGroup> =
        std::collections::HashMap::new();
    let mut skipped_count: u32 = 0;
    let mut skipped_formats: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for d in details {
        if d.format.as_deref() != Some(OPENAI_RESPONSES_FORMAT) {
            skipped_count += 1;
            skipped_formats.insert(d.format.as_deref().unwrap_or("<none>").to_string());
            continue;
        }
        let key = d.id.clone();
        if !groups.contains_key(&key) {
            order.push(key.clone());
            groups.insert(key.clone(), ReasoningGroup::default());
        }
        let group = groups.get_mut(&key).expect("inserted above");
        match d.kind {
            ReasoningDetailKind::Summary => {
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str()) {
                    group.summary.push(ReasoningSummaryItem::SummaryText {
                        text: text.to_string(),
                    });
                }
            }
            ReasoningDetailKind::Text => {
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str()) {
                    group.content.push(ReasoningContentItem::ReasoningText {
                        text: text.to_string(),
                    });
                }
            }
            ReasoningDetailKind::Encrypted => {
                if let Some(sig) = d.payload.get("encrypted_content").and_then(|v| v.as_str()) {
                    if group.encrypted_content.is_none() {
                        group.encrypted_content = Some(sig.to_string());
                    } else {
                        // Multiple Encrypted details on the same id:
                        // surface as an inner reasoning_encrypted
                        // content block so no signature is lost.
                        group
                            .content
                            .push(ReasoningContentItem::ReasoningEncrypted {
                                encrypted_content: sig.to_string(),
                            });
                    }
                }
            }
        }
    }

    for key in order {
        let group = groups.remove(&key).expect("recorded in order");
        let encrypted_content = group.encrypted_content.unwrap_or_default();
        // A reasoning item with empty encrypted_content cannot be
        // validly replayed: re-injecting it by its upstream id is a
        // no-op (chatgpt-oauth) or a hard 404 "Item not found"
        // (api.openai.com). Skip it rather than ship a dangling id.
        if encrypted_content.is_empty() {
            tracing::debug!(
                ?key,
                "openai-responses: skipping reasoning replay item with empty encrypted_content"
            );
            continue;
        }
        out.push(ResponseInputItem::Reasoning {
            id: key,
            summary: group.summary,
            content: group.content,
            encrypted_content,
        });
    }

    if skipped_count > 0 {
        let formats: Vec<&str> = skipped_formats.iter().map(String::as_str).collect();
        tracing::debug!(
            skipped = skipped_count,
            formats = ?formats,
            "openai-responses: skipped reasoning_details entries with non-openai-responses-v1 format"
        );
    }
}

#[derive(Default)]
struct ReasoningGroup {
    summary: Vec<ReasoningSummaryItem>,
    content: Vec<ReasoningContentItem>,
    encrypted_content: Option<String>,
}

fn translate_tool_message(id: &str, msg: &Message, out: &mut Vec<ResponseInputItem>) -> Result<()> {
    let Some(call_id) = msg.tool_call_id.as_ref().filter(|s| !s.is_empty()).cloned() else {
        return Err(Error::normalize_request(
            id,
            "tool message missing tool_call_id (Role::Tool requires a \
             non-empty tool_call_id for the Responses API function_call_output item)",
        ));
    };
    let output = match &msg.content {
        MessageContent::Text(t) => FunctionCallOutputBody::Text(t.clone()),
        MessageContent::Null => FunctionCallOutputBody::Text(String::new()),
        MessageContent::Parts(parts) => build_tool_output_body(id, parts),
    };
    out.push(ResponseInputItem::FunctionCallOutput { call_id, output });
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-part translation
// ---------------------------------------------------------------------------

fn build_user_content(id: &str, content: &MessageContent) -> Result<Vec<ResponsesContentItem>> {
    match content {
        MessageContent::Text(t) if t.is_empty() => Ok(Vec::new()),
        MessageContent::Text(t) => Ok(vec![ResponsesContentItem::InputText { text: t.clone() }]),
        MessageContent::Null => Ok(Vec::new()),
        MessageContent::Parts(parts) => {
            let mut out: Vec<ResponsesContentItem> = Vec::with_capacity(parts.len());
            for p in parts {
                match p {
                    ContentPart::Known(KnownContentPart::Text { text, .. }) => {
                        if !text.is_empty() {
                            out.push(ResponsesContentItem::InputText { text: text.clone() });
                        }
                    }
                    ContentPart::Known(KnownContentPart::Image { source, .. }) => {
                        if let Some(item) = translate_image_source(id, source) {
                            out.push(item);
                        }
                    }
                    ContentPart::Known(KnownContentPart::ImageUrl { image_url, .. }) => {
                        // OpenAI-shape image_url block: extract the url
                        // field and emit an InputImage. detail, if present
                        // on the nested object, is forwarded.
                        if let Some(url) = image_url.get("url").and_then(|u| u.as_str()) {
                            let detail = image_url
                                .get("detail")
                                .and_then(|d| d.as_str())
                                .map(str::to_string);
                            out.push(ResponsesContentItem::InputImage {
                                image_url: url.to_string(),
                                detail,
                            });
                        } else {
                            tracing::warn!(
                                provider = id,
                                role = "user",
                                "dropping image_url part with missing url field on Responses egress"
                            );
                        }
                    }
                    ContentPart::Known(KnownContentPart::ToolResult { .. }) => {
                        // Lifted to FunctionCallOutput in
                        // `extract_tool_results`; skip silently here.
                    }
                    ContentPart::Known(KnownContentPart::File { file, .. }) => {
                        if let Some(item) = translate_file_part(id, file) {
                            out.push(item);
                        }
                    }
                    ContentPart::Known(other) => {
                        tracing::warn!(
                            provider = id,
                            part_type = other.type_tag(),
                            role = "user",
                            "dropping unsupported user content part on Responses egress"
                        );
                    }
                    ContentPart::Other { type_tag, .. } => {
                        tracing::warn!(
                            provider = id,
                            part_type = %type_tag,
                            role = "user",
                            "dropping forward-compat user content part on Responses egress"
                        );
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Per-part walker for an assistant turn. Routes each part into the
/// appropriate output bucket: `Thinking` -> reasoning items;
/// `RedactedThinking` -> reasoning items with EMPTY encrypted_content
/// (the opaque Anthropic blob is not a valid OpenAI token and is not
/// forwarded); `Text` -> message content (output_text); `ToolUse` -> a
/// separate `FunctionCall` input item. Everything else drops with a WARN.
///
/// `suppress_thinking_parts` is true when `reasoning_details` already
/// produced Reasoning items: in that case Thinking + RedactedThinking
/// content parts are skipped (with a debug log) to avoid duplicate
/// Reasoning items on the wire. The canonical schema invariant is
/// that the two surfaces are mutually exclusive; we prefer the
/// response-side `reasoning_details` because it carries the JWT
/// signature in a slot that ContentPart::Thinking lacks.
fn walk_assistant_part(
    id: &str,
    p: &ContentPart,
    suppress_thinking_parts: bool,
    reasoning: &mut Vec<ResponseInputItem>,
    message_content: &mut Vec<ResponsesContentItem>,
    tool_calls: &mut Vec<ResponseInputItem>,
) -> Result<()> {
    match p {
        ContentPart::Known(KnownContentPart::Text { text, .. }) => {
            if !text.is_empty() {
                message_content.push(ResponsesContentItem::OutputText { text: text.clone() });
            }
        }
        ContentPart::Known(KnownContentPart::Thinking {
            thinking,
            signature,
        }) => {
            if suppress_thinking_parts {
                tracing::debug!(
                    provider = id,
                    role = "assistant",
                    "skipping Thinking content-part because reasoning_details already emitted Reasoning items"
                );
            } else {
                reasoning.push(translate_thinking_part(
                    thinking,
                    signature.as_deref(),
                    None,
                ));
            }
        }
        ContentPart::Known(KnownContentPart::RedactedThinking { data }) => {
            if suppress_thinking_parts {
                tracing::debug!(
                    provider = id,
                    role = "assistant",
                    "skipping RedactedThinking content-part because reasoning_details already emitted Reasoning items"
                );
            } else {
                // A RedactedThinking content-part carries an opaque
                // Anthropic blob with no format tag, so it is gated the
                // same way as Thinking: the blob is NOT a valid OpenAI
                // encrypted_content token and must not be forwarded into
                // that slot. `data` is treated as the (absent) signature
                // for an unknown-format part, yielding empty
                // encrypted_content.
                reasoning.push(translate_thinking_part("", Some(data), None));
            }
        }
        ContentPart::Known(KnownContentPart::ToolUse {
            id: tu_id,
            name,
            input,
            ..
        }) => {
            tool_calls.push(ResponseInputItem::FunctionCall {
                call_id: tu_id.clone(),
                name: name.clone(),
                arguments: serde_json::to_string(input)
                    .map_err(|e| Error::normalize_request(id, e.to_string()))?,
            });
        }
        ContentPart::Known(other) => {
            tracing::warn!(
                provider = id,
                part_type = other.type_tag(),
                role = "assistant",
                "dropping unsupported assistant content part on Responses egress"
            );
        }
        ContentPart::Other { type_tag, .. } => {
            tracing::warn!(
                provider = id,
                part_type = %type_tag,
                role = "assistant",
                "dropping forward-compat assistant content part on Responses egress"
            );
        }
    }
    Ok(())
}

/// Translate a canonical Thinking block to a Responses-shape
/// `Reasoning` input item. The signature is forwarded as
/// `encrypted_content` ONLY when `format` is `openai-responses-v1` --
/// the one case where we know the signature is a valid OpenAI
/// encrypted_content token. For Anthropic-format thinking parts the
/// field is emitted as an empty string; codex treats empty
/// `encrypted_content` as a no-op for replay (`arc_monitor.rs::325-336`).
pub(super) fn translate_thinking_part(
    thinking: &str,
    signature: Option<&str>,
    format: Option<&str>,
) -> ResponseInputItem {
    let summary = if thinking.is_empty() {
        Vec::new()
    } else {
        vec![ReasoningSummaryItem::SummaryText {
            text: thinking.to_string(),
        }]
    };
    // Only forward the signature as encrypted_content when the source
    // is openai-responses-v1. Anthropic signatures are not valid OpenAI
    // encrypted_content tokens; forwarding them would corrupt the replay
    // gate on the upstream server.
    let encrypted_content = if format == Some(OPENAI_RESPONSES_FORMAT) {
        signature.unwrap_or("").to_string()
    } else {
        String::new()
    };
    ResponseInputItem::Reasoning {
        id: None,
        summary,
        content: Vec::new(),
        encrypted_content,
    }
}

/// Translate an OpenAI-shape `File` part's nested `file` object into a
/// `ResponsesContentItem::InputFile`. The nested object carries either
/// `file_data` (a `data:<mime>;base64,<...>` URI for an inline upload)
/// or `file_id` (a reference to a previously-uploaded file), plus an
/// optional `filename`. Returns `None` and WARNs when neither carrier is
/// present (an empty file part has nothing the upstream can act on).
fn translate_file_part(id: &str, file: &serde_json::Value) -> Option<ResponsesContentItem> {
    let file_data = file
        .get("file_data")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let file_id = file
        .get("file_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let filename = file
        .get("filename")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if file_data.is_none() && file_id.is_none() {
        tracing::warn!(
            provider = id,
            role = "user",
            "dropping file part with no file_data or file_id on Responses egress"
        );
        return None;
    }

    Some(ResponsesContentItem::InputFile {
        file_data,
        file_id,
        filename,
    })
}

/// Translate a canonical `Image` source block to a
/// `ResponsesContentItem::InputImage`. Returns `None` and emits a WARN
/// when the source shape is unrecognized (forward-compat unknown kind).
fn translate_image_source(id: &str, source: &serde_json::Value) -> Option<ResponsesContentItem> {
    let kind = source.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "base64" => {
            let media_type = source
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("application/octet-stream");
            let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if data.is_empty() {
                tracing::warn!(
                    provider = id,
                    role = "user",
                    "dropping image part with empty base64 data on Responses egress"
                );
                return None;
            }
            Some(ResponsesContentItem::InputImage {
                image_url: format!("data:{};base64,{}", media_type, data),
                detail: None,
            })
        }
        "url" => {
            let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if url.is_empty() {
                tracing::warn!(
                    provider = id,
                    role = "user",
                    "dropping image part with empty url on Responses egress"
                );
                return None;
            }
            Some(ResponsesContentItem::InputImage {
                image_url: url.to_string(),
                detail: None,
            })
        }
        other => {
            tracing::warn!(
                provider = id,
                source_kind = other,
                role = "user",
                "dropping image part with unknown source kind on Responses egress"
            );
            None
        }
    }
}

/// Build a `FunctionCallOutputBody` from a parts slice. When all parts
/// are plain text the result collapses to a flat string (codex parity,
/// most-common path). When any part is non-text (e.g. an image returned
/// by a visual tool) the result is an Items array. Unknown parts are
/// WARN-dropped; the remaining known parts are still forwarded.
fn build_tool_output_body(id: &str, parts: &[ContentPart]) -> FunctionCallOutputBody {
    let has_non_text = parts.iter().any(|p| {
        matches!(
            p,
            ContentPart::Known(KnownContentPart::Image { .. } | KnownContentPart::ImageUrl { .. })
        )
    });

    if !has_non_text {
        // Fast path: all text. Concatenate.
        let mut buf = String::new();
        for p in parts {
            if let ContentPart::Known(KnownContentPart::Text { text, .. }) = p {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(text);
            } else {
                tracing::warn!(
                    provider = id,
                    part_type = p.type_tag(),
                    role = "tool",
                    "dropping unsupported tool result part on Responses egress"
                );
            }
        }
        return FunctionCallOutputBody::Text(buf);
    }

    // Mixed path: build typed items array.
    let mut items: Vec<FunctionCallOutputContentItem> = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            ContentPart::Known(KnownContentPart::Text { text, .. }) => {
                items.push(FunctionCallOutputContentItem::InputText { text: text.clone() });
            }
            ContentPart::Known(KnownContentPart::Image { source, .. }) => {
                if let Some(item) = translate_tool_image_source(id, source) {
                    items.push(item);
                }
            }
            ContentPart::Known(KnownContentPart::ImageUrl { image_url, .. }) => {
                if let Some(url) = image_url.get("url").and_then(|u| u.as_str()) {
                    let detail = image_url
                        .get("detail")
                        .and_then(|d| d.as_str())
                        .map(str::to_string);
                    items.push(FunctionCallOutputContentItem::InputImage {
                        image_url: url.to_string(),
                        detail,
                    });
                } else {
                    tracing::warn!(
                        provider = id,
                        role = "tool",
                        "dropping image_url part with missing url in tool result on Responses egress"
                    );
                }
            }
            other => {
                tracing::warn!(
                    provider = id,
                    part_type = other.type_tag(),
                    role = "tool",
                    "dropping unsupported tool result part on Responses egress"
                );
            }
        }
    }
    FunctionCallOutputBody::Items(items)
}

/// Translate an Anthropic-shape image source inside a tool result to a
/// `FunctionCallOutputContentItem::InputImage`. Returns `None` on
/// unrecognized source kinds.
fn translate_tool_image_source(
    id: &str,
    source: &serde_json::Value,
) -> Option<FunctionCallOutputContentItem> {
    let kind = source.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "base64" => {
            let media_type = source
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("application/octet-stream");
            let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if data.is_empty() {
                tracing::warn!(
                    provider = id,
                    role = "tool",
                    "dropping image part with empty base64 data in tool result on Responses egress"
                );
                return None;
            }
            Some(FunctionCallOutputContentItem::InputImage {
                image_url: format!("data:{};base64,{}", media_type, data),
                detail: None,
            })
        }
        "url" => {
            let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if url.is_empty() {
                tracing::warn!(
                    provider = id,
                    role = "tool",
                    "dropping image part with empty url in tool result on Responses egress"
                );
                return None;
            }
            Some(FunctionCallOutputContentItem::InputImage {
                image_url: url.to_string(),
                detail: None,
            })
        }
        other => {
            tracing::warn!(
                provider = id,
                source_kind = other,
                role = "tool",
                "dropping image part with unknown source kind in tool result on Responses egress"
            );
            None
        }
    }
}

#[cfg(test)]
mod messages_tests {
    use serde_json::json;

    use routectl_core::{ReasoningDetail, ReasoningDetailKind};

    use super::super::types::ResponseInputItem;
    use super::super::OPENAI_RESPONSES_FORMAT;
    use super::{lift_reasoning_details, translate_thinking_part};

    fn make_detail(
        format: Option<&str>,
        kind: ReasoningDetailKind,
        payload: serde_json::Value,
    ) -> ReasoningDetail {
        ReasoningDetail {
            kind,
            id: None,
            format: format.map(str::to_string),
            index: None,
            payload,
        }
    }

    // -------------------------------------------------------------------
    // Finding 5: lift_reasoning_details skips non-openai-responses-v1
    // entries and aggregates the dropped formats for debug logging.
    // -------------------------------------------------------------------

    #[test]
    fn lift_skips_anthropic_format_details() {
        // Arrange: one detail with anthropic-claude-v1 format.
        let details = vec![make_detail(
            Some("anthropic-claude-v1"),
            ReasoningDetailKind::Text,
            json!({"text": "some reasoning"}),
        )];

        // Act
        let mut out = Vec::new();
        lift_reasoning_details(&details, &mut out);

        // Assert: no items emitted for anthropic-format details.
        assert!(
            out.is_empty(),
            "expected no Reasoning items from anthropic-claude-v1 details"
        );
    }

    #[test]
    fn lift_skips_format_less_details() {
        // Arrange: detail with no format tag.
        let details = vec![make_detail(
            None,
            ReasoningDetailKind::Text,
            json!({"text": "some reasoning"}),
        )];

        // Act
        let mut out = Vec::new();
        lift_reasoning_details(&details, &mut out);

        // Assert: no items emitted for format-less details.
        assert!(
            out.is_empty(),
            "expected no Reasoning items from format-less details"
        );
    }

    #[test]
    fn lift_includes_openai_responses_v1_details() {
        // Arrange: a v1 detail carrying an encrypted_content signature
        // (the only shape that can be validly replayed).
        let details = vec![make_detail(
            Some(OPENAI_RESPONSES_FORMAT),
            ReasoningDetailKind::Encrypted,
            json!({"encrypted_content": "SIG"}),
        )];

        // Act
        let mut out = Vec::new();
        lift_reasoning_details(&details, &mut out);

        // Assert: openai-responses-v1 details produce a Reasoning item.
        assert_eq!(
            out.len(),
            1,
            "expected one Reasoning item from openai-responses-v1 detail"
        );
    }

    #[test]
    fn lift_skips_v1_detail_with_empty_encrypted_content() {
        // Arrange: a v1 detail with no encrypted_content (text only).
        // It cannot be validly replayed, so it must be dropped to avoid
        // a dangling reasoning id on the wire.
        let details = vec![make_detail(
            Some(OPENAI_RESPONSES_FORMAT),
            ReasoningDetailKind::Text,
            json!({"text": "the reasoning text"}),
        )];

        // Act
        let mut out = Vec::new();
        lift_reasoning_details(&details, &mut out);

        // Assert
        assert!(
            out.is_empty(),
            "expected no Reasoning items for a v1 detail with empty encrypted_content"
        );
    }

    #[test]
    fn lift_replays_v1_detail_with_non_empty_encrypted_content() {
        // Arrange: a v1 detail that carries both text and a signature.
        let details = vec![
            make_detail(
                Some(OPENAI_RESPONSES_FORMAT),
                ReasoningDetailKind::Text,
                json!({"text": "the reasoning text"}),
            ),
            make_detail(
                Some(OPENAI_RESPONSES_FORMAT),
                ReasoningDetailKind::Encrypted,
                json!({"encrypted_content": "SIG"}),
            ),
        ];

        // Act
        let mut out = Vec::new();
        lift_reasoning_details(&details, &mut out);

        // Assert: a single Reasoning item carrying the signature.
        assert_eq!(out.len(), 1, "expected one replayed Reasoning item");
        let ResponseInputItem::Reasoning {
            encrypted_content, ..
        } = &out[0]
        else {
            panic!("expected ResponseInputItem::Reasoning");
        };
        assert_eq!(encrypted_content, "SIG");
    }

    #[test]
    fn lift_mixed_formats_only_includes_v1() {
        // Arrange: mix of openai-responses-v1 and anthropic-claude-v1.
        // The v1 entries carry a signature so the v1 item is replayable.
        let details = vec![
            make_detail(
                Some(OPENAI_RESPONSES_FORMAT),
                ReasoningDetailKind::Text,
                json!({"text": "openai reasoning"}),
            ),
            make_detail(
                Some(OPENAI_RESPONSES_FORMAT),
                ReasoningDetailKind::Encrypted,
                json!({"encrypted_content": "SIG"}),
            ),
            make_detail(
                Some("anthropic-claude-v1"),
                ReasoningDetailKind::Text,
                json!({"text": "anthropic reasoning"}),
            ),
        ];

        // Act
        let mut out = Vec::new();
        lift_reasoning_details(&details, &mut out);

        // Assert: only the v1 item is included.
        assert_eq!(
            out.len(),
            1,
            "expected exactly one item (the openai-responses-v1 detail)"
        );
    }

    // -------------------------------------------------------------------
    // Finding 6: translate_thinking_part gates signature on format.
    // -------------------------------------------------------------------

    #[test]
    fn anthropic_format_thinking_does_not_leak_signature_into_encrypted_content() {
        // Arrange: ContentPart::Thinking path passes format = None
        // (KnownContentPart::Thinking carries no format field).
        let thinking = "I reasoned carefully about this.";
        let signature = Some("anthropic_sig_MUST_NOT_APPEAR");

        // Act
        let item = translate_thinking_part(thinking, signature, None);

        // Assert: Anthropic signature MUST NOT appear in encrypted_content.
        let ResponseInputItem::Reasoning {
            encrypted_content, ..
        } = item
        else {
            panic!("expected ResponseInputItem::Reasoning");
        };
        assert!(
            encrypted_content.is_empty(),
            "Anthropic signature must not leak into encrypted_content, got: {encrypted_content}"
        );
    }

    #[test]
    fn openai_responses_format_forwards_signature_to_encrypted_content() {
        // Arrange: openai-responses-v1 path (came from reasoning_details).
        let thinking = "Some intermediate reasoning";
        let signature = Some("test-openai-sig-not-real");

        // Act
        let item = translate_thinking_part(thinking, signature, Some(OPENAI_RESPONSES_FORMAT));

        // Assert: the signature IS forwarded.
        let ResponseInputItem::Reasoning {
            encrypted_content, ..
        } = item
        else {
            panic!("expected ResponseInputItem::Reasoning");
        };
        assert_eq!(
            encrypted_content, "test-openai-sig-not-real",
            "openai-responses-v1 signature must be forwarded as encrypted_content"
        );
    }

    #[test]
    fn openai_responses_format_no_signature_emits_empty_encrypted_content() {
        // Arrange: openai-responses-v1 format, no signature available.
        let thinking = "Some reasoning";

        // Act
        let item = translate_thinking_part(thinking, None, Some(OPENAI_RESPONSES_FORMAT));

        // Assert: no signature -> empty encrypted_content (the documented
        // "no prior signature" shape; codex treats it as a no-op).
        let ResponseInputItem::Reasoning {
            encrypted_content, ..
        } = item
        else {
            panic!("expected ResponseInputItem::Reasoning");
        };
        assert!(
            encrypted_content.is_empty(),
            "None signature should yield empty encrypted_content, got: {encrypted_content}"
        );
    }
}

#[cfg(test)]
mod tool_calls_field_tests {
    use serde_json::json;

    use routectl_core::{ContentPart, KnownContentPart, Message, MessageContent, Role};

    use super::super::types::ResponseInputItem;
    use super::build_input;

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// An assistant turn whose tool call rides ONLY on the OpenAI-shape
    /// `tool_calls` field (content null/empty, no ToolUse content part)
    /// must emit a `function_call` input item carrying the call_id, name,
    /// and forwarded arguments string -- so the following
    /// `function_call_output` is not dangling.
    #[test]
    fn assistant_openai_tool_calls_field_emits_function_call_item() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
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
        let out = build_input("test", &messages).unwrap();

        // Assert: a function_call item carries the call data.
        let fc_idx = out
            .iter()
            .position(|i| {
                matches!(
                    i,
                    ResponseInputItem::FunctionCall { call_id, name, arguments }
                        if call_id == "call_1"
                            && name == "get_weather"
                            && arguments == "{\"city\":\"SF\"}"
                )
            })
            .expect("tool_calls field must produce a matching function_call item");

        // The function_call_output references the same id and follows
        // the function_call (not orphaned).
        let fco_idx = out
            .iter()
            .position(|i| {
                matches!(i, ResponseInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1")
            })
            .expect("function_call_output must be present");
        assert!(
            fc_idx < fco_idx,
            "function_call must precede its function_call_output"
        );
    }

    /// A tool call with a missing id is synthesized to a non-empty
    /// call_id so the Responses upstream does not reject an empty id.
    #[test]
    fn assistant_tool_call_missing_id_is_synthesized_on_responses() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
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
        let out = build_input("test", &messages).unwrap();

        // Assert
        let call_id = out
            .iter()
            .find_map(|i| match i {
                ResponseInputItem::FunctionCall { call_id, .. } => Some(call_id),
                _ => None,
            })
            .expect("missing-id tool call must still produce a function_call item");
        assert!(
            !call_id.is_empty(),
            "missing id must be synthesized non-empty, got empty"
        );
    }

    /// When the assistant turn ALREADY carries a ToolUse content part,
    /// setting `tool_calls` as well must NOT double-emit the function_call
    /// item (the content-part walk already emitted it).
    #[test]
    fn assistant_tool_use_content_part_not_doubled_by_tool_calls_field() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
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
        let out = build_input("test", &messages).unwrap();

        // Assert: exactly one function_call item, not two.
        let count = out
            .iter()
            .filter(|i| matches!(i, ResponseInputItem::FunctionCall { .. }))
            .count();
        assert_eq!(
            count, 1,
            "function_call must not be doubled when both content part and tool_calls are set"
        );
    }

    /// A single-turn assistant text message with no tool_calls produces a
    /// single assistant Message item and no function_call items.
    #[test]
    fn assistant_plain_text_turn_unchanged_without_tool_calls() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
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
        let out = build_input("test", &messages).unwrap();

        // Assert: no function_call items; the assistant Message survives.
        assert!(
            out.iter()
                .all(|i| !matches!(i, ResponseInputItem::FunctionCall { .. })),
            "no function_call items expected on a plain text turn"
        );
        let assistant_msg = out
            .iter()
            .find(|i| matches!(i, ResponseInputItem::Message { role, .. } if role == "assistant"));
        assert!(assistant_msg.is_some(), "assistant Message must survive");
    }
}
