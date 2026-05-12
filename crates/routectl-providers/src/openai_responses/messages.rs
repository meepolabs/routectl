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
//! `FunctionCall` items emitted alongside the `Message` for that turn.
//!
//! Reasoning replay: codex re-injects reasoning blocks only when
//! `encrypted_content` is non-empty (see
//! `codex/codex-rs/core/src/arc_monitor.rs::325-336`). routectl always
//! emits the field; an empty string is the documented "no prior
//! signature" shape. Canonical Thinking blocks without a signature
//! (first-turn requests, or providers that didn't surface one) flow
//! through cleanly as empty-string `encrypted_content`.

use routectl_core::{
    ContentPart, Error, KnownContentPart, Message, MessageContent, Result, Role,
};

use super::types::{
    FunctionCallOutputBody, FunctionCallOutputContentItem, ReasoningSummaryItem, ResponseInputItem,
    ResponsesContentItem,
};

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

fn translate_user_message(
    id: &str,
    msg: &Message,
    out: &mut Vec<ResponseInputItem>,
) -> Result<()> {
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

/// Assistant turn translation. Walks each part, splitting into a
/// reasoning-item stream + a message-content stream + a tool-call
/// stream, then emits items in the order [reasoning?, message?,
/// tool_calls...] so multi-turn replay preserves the original ordering
/// the model sees.
fn translate_assistant_message(
    id: &str,
    msg: &Message,
    out: &mut Vec<ResponseInputItem>,
) -> Result<()> {
    let mut reasoning_items: Vec<ResponseInputItem> = Vec::new();
    let mut message_content: Vec<ResponsesContentItem> = Vec::new();
    let mut tool_calls: Vec<ResponseInputItem> = Vec::new();

    match &msg.content {
        MessageContent::Text(t) if !t.is_empty() => {
            message_content.push(ResponsesContentItem::OutputText { text: t.clone() });
        }
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            for p in parts {
                walk_assistant_part(
                    id,
                    p,
                    &mut reasoning_items,
                    &mut message_content,
                    &mut tool_calls,
                )?;
            }
        }
    }

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

fn translate_tool_message(
    id: &str,
    msg: &Message,
    out: &mut Vec<ResponseInputItem>,
) -> Result<()> {
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
                            out.push(ResponsesContentItem::InputText {
                                text: text.clone(),
                            });
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
/// `RedactedThinking` -> reasoning items (using the redacted data as
/// the encrypted_content carrier with an empty summary); `Text` ->
/// message content (output_text); `ToolUse` -> a separate
/// `FunctionCall` input item. Everything else drops with a WARN.
fn walk_assistant_part(
    id: &str,
    p: &ContentPart,
    reasoning: &mut Vec<ResponseInputItem>,
    message_content: &mut Vec<ResponsesContentItem>,
    tool_calls: &mut Vec<ResponseInputItem>,
) -> Result<()> {
    match p {
        ContentPart::Known(KnownContentPart::Text { text, .. }) => {
            if !text.is_empty() {
                message_content.push(ResponsesContentItem::OutputText {
                    text: text.clone(),
                });
            }
        }
        ContentPart::Known(KnownContentPart::Thinking {
            thinking,
            signature,
        }) => {
            reasoning.push(translate_thinking_part(thinking, signature.as_deref()));
        }
        ContentPart::Known(KnownContentPart::RedactedThinking { data }) => {
            // Redacted blocks have no plaintext summary; ride the
            // base64 bytes verbatim in encrypted_content so the
            // server-side replay gate has something to key on.
            reasoning.push(ResponseInputItem::Reasoning {
                summary: Vec::new(),
                encrypted_content: data.clone(),
            });
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
/// `Reasoning` input item. The signature is lifted verbatim into
/// `encrypted_content`; when None, an empty string is emitted -- codex
/// treats empty `encrypted_content` as a no-op for replay
/// (`arc_monitor.rs::325-336`), so the field is safe to send empty.
pub(super) fn translate_thinking_part(
    thinking: &str,
    signature: Option<&str>,
) -> ResponseInputItem {
    let summary = if thinking.is_empty() {
        Vec::new()
    } else {
        vec![ReasoningSummaryItem::SummaryText {
            text: thinking.to_string(),
        }]
    };
    ResponseInputItem::Reasoning {
        summary,
        encrypted_content: signature.unwrap_or("").to_string(),
    }
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
            let data = source
                .get("data")
                .and_then(|d| d.as_str())
                .unwrap_or("");
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
            ContentPart::Known(
                KnownContentPart::Image { .. } | KnownContentPart::ImageUrl { .. }
            )
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
