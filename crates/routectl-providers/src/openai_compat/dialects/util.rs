//! Helpers shared between dialect impls. Everything here is
//! `pub(super)` -- only siblings in `dialects/` should reach in.
//!
//! These helpers all mutate their inputs in place to fit the trait's
//! `&mut`-by-reference shape; a return is reserved for genuine errors.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use routectl_core::{Error, Message, MessageContent, Result};

use crate::openai_compat::util::build_reasoning_detail;

/// Move `msg.reasoning` (a plain string) into a typed
/// `reasoning_details` block tagged with `format_tag`.
///
/// `msg.reasoning` is left empty after the lift -- the typed array is
/// the canonical representation. DeepSeek and vLLM both emit
/// `reasoning_content` upstream which the openai_compat response
/// preprocessor coalesces into `msg.reasoning` before this runs.
pub(super) fn lift_reasoning_content_field(msg: &mut Message, format_tag: &str) {
    let text = match msg.reasoning.take() {
        Some(t) if !t.is_empty() => t,
        _ => return,
    };
    let detail = build_reasoning_detail(&text, format_tag, msg.reasoning_details.len() as u32);
    msg.reasoning_details.push(detail);
}

/// Strip `<think>...</think>` blocks from `msg.content`, lifting their
/// contents into a typed `reasoning_details` entry tagged `format_tag`.
/// No-op when content has no think tags or is non-text.
pub(super) fn lift_think_tags(_id: &str, msg: &mut Message, format_tag: &str) -> Result<()> {
    let content = match &msg.content {
        MessageContent::Text(t) => t.clone(),
        // Parts/Null: not expected from reasoning-model endpoints.
        MessageContent::Parts(_) | MessageContent::Null => return Ok(()),
    };

    let re = think_tag_regex();
    let mut reasoning_text = String::new();
    let stripped = re.replace_all(&content, |caps: &regex::Captures| {
        reasoning_text.push_str(caps.get(1).map_or("", |m| m.as_str()));
        ""
    });

    if reasoning_text.is_empty() {
        return Ok(());
    }

    let detail = build_reasoning_detail(
        &reasoning_text,
        format_tag,
        msg.reasoning_details.len() as u32,
    );
    msg.reasoning_details.push(detail);
    msg.content = MessageContent::Text(stripped.trim_start().to_string());
    Ok(())
}

fn think_tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // DOTALL via `(?s)` so `.` matches newlines inside think blocks.
        Regex::new(r"(?s)<think>(.*?)</think>").expect("static regex is valid")
    })
}

/// Wrap `delta.reasoning` (a coalesced string) into a typed
/// `reasoning_details` array entry on the chunk's first choice's delta.
/// Leaves `delta.reasoning` intact for legacy-compat clients.
pub(super) fn lift_delta_reasoning_content(
    id: &str,
    val: &mut Value,
    format_tag: &str,
) -> Result<()> {
    let choices = val
        .get_mut("choices")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| Error::Streaming(format!("provider `{id}`: chunk missing choices")))?;

    for choice in choices.iter_mut() {
        let delta = match choice.get_mut("delta").and_then(|v| v.as_object_mut()) {
            Some(d) => d,
            None => continue,
        };

        let rc = match delta.get("reasoning") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => continue,
        };

        let detail = build_reasoning_detail(&rc, format_tag, 0);
        let detail_val = serde_json::to_value(detail)
            .map_err(|e| Error::Streaming(format!("provider `{id}`: detail serialize: {e}")))?;

        delta
            .entry("reasoning_details")
            .or_insert_with(|| Value::Array(vec![]))
            .as_array_mut()
            .expect("just inserted array")
            .push(detail_val);
    }
    Ok(())
}

/// Strip `reasoning_content`, `reasoning_details`, and `reasoning` from
/// every message in the outgoing request body. DeepSeek 400s when an
/// echoed assistant message carries any of these.
///
/// Visibility: `pub(in crate::openai_compat)` so the egress runtime in
/// `super::super::request::normalize` can call it directly. The
/// strip-or-preserve choice is owned by the runtime, not the dialect
/// trait, because it depends on the per-provider `history_reasoning`
/// TOML knob (which dialects don't see).
pub(in crate::openai_compat) fn strip_history_reasoning(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
) -> Result<()> {
    let messages = obj
        .get_mut("messages")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| Error::normalize_request(id, "messages is not an array"))?;

    for msg in messages.iter_mut() {
        if let Some(m) = msg.as_object_mut() {
            m.remove("reasoning_content");
            m.remove("reasoning_details");
            m.remove("reasoning");
        }
    }
    Ok(())
}

/// Preserve outgoing assistant reasoning as a `reasoning_content`
/// string field. DeepSeek v4+ and recent vLLM hosts require this on
/// echo-back to the API.
///
/// For each assistant message the body carries:
///   - If `reasoning_content` is already present, leave it.
///   - Else if `reasoning` is present (canonical's plaintext slot),
///     RENAME it to `reasoning_content`. The canonical schema uses
///     `reasoning` for OpenRouter compat; DeepSeek wants the field
///     under its own name.
///   - Else if `reasoning_details` is present, lower the typed array
///     to a string by joining each detail's `text` payload in order
///     (Anthropic-aligned details get flattened into a single string;
///     non-text details are dropped with a tracing::warn).
///   - Drop `reasoning` and `reasoning_details` after the rewrite so
///     DeepSeek doesn't 400 on the echoed Anthropic-shape array.
pub(super) fn preserve_history_reasoning_content(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
) -> Result<()> {
    let messages = obj
        .get_mut("messages")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| Error::normalize_request(id, "messages is not an array"))?;

    for msg in messages.iter_mut() {
        let Some(m) = msg.as_object_mut() else {
            continue;
        };
        // Only assistant messages carry reasoning.
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant" {
            // Strip reasoning fields from non-assistant messages
            // anyway -- DeepSeek 400s if they sneak onto user / tool
            // messages too.
            m.remove("reasoning_content");
            m.remove("reasoning_details");
            m.remove("reasoning");
            continue;
        }
        // Already has reasoning_content -- leave it. Belt-and-braces
        // remove the legacy slots so the wire body has exactly one
        // surface.
        if m.contains_key("reasoning_content") {
            m.remove("reasoning_details");
            m.remove("reasoning");
            continue;
        }
        // Try the plaintext slot.
        if let Some(reasoning) = m.remove("reasoning") {
            if let Some(s) = reasoning.as_str() {
                if !s.is_empty() {
                    m.insert("reasoning_content".into(), Value::String(s.to_string()));
                }
            }
            m.remove("reasoning_details");
            continue;
        }
        // Fall back to lowering reasoning_details to a string.
        if let Some(details) = m.remove("reasoning_details") {
            let lowered = lower_reasoning_details_to_text(&details, id);
            if !lowered.is_empty() {
                m.insert("reasoning_content".into(), Value::String(lowered));
            }
        }
    }
    Ok(())
}

/// Preserve outgoing assistant reasoning as a structured
/// `reasoning_details` array. OpenRouter accepts the Anthropic-aligned
/// typed shape verbatim.
///
/// For each assistant message:
///   - If `reasoning_details` is already present, leave it and drop
///     the legacy `reasoning` / `reasoning_content` slots.
///   - Else if `reasoning` is present, lift it to a single
///     `reasoning_details` entry with format = `format_tag` so the
///     downstream consumer can round-trip continuation correctly.
///   - Else if `reasoning_content` is present, same lift.
pub(super) fn preserve_history_reasoning_details(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    format_tag: &str,
) -> Result<()> {
    let messages = obj
        .get_mut("messages")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| Error::normalize_request(id, "messages is not an array"))?;

    for msg in messages.iter_mut() {
        let Some(m) = msg.as_object_mut() else {
            continue;
        };
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant" {
            m.remove("reasoning_content");
            m.remove("reasoning_details");
            m.remove("reasoning");
            continue;
        }
        if m.contains_key("reasoning_details") {
            m.remove("reasoning");
            m.remove("reasoning_content");
            continue;
        }
        let plaintext = m
            .remove("reasoning")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| {
                m.remove("reasoning_content")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
            });
        if let Some(text) = plaintext {
            if !text.is_empty() {
                m.insert(
                    "reasoning_details".into(),
                    serde_json::json!([{
                        "type": "reasoning.text",
                        "format": format_tag,
                        "index": 0,
                        "text": text,
                    }]),
                );
            }
        }
    }
    Ok(())
}

/// Lower a `reasoning_details` array (Anthropic-aligned typed shape)
/// to a plaintext string by joining each entry's `text` payload in
/// order. Non-text details are skipped with a `tracing::warn!` so
/// operators see information loss when an upstream sends typed
/// reasoning that doesn't survive the lowering.
fn lower_reasoning_details_to_text(details: &Value, provider_id: &str) -> String {
    let Some(arr) = details.as_array() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for d in arr {
        let Some(obj) = d.as_object() else {
            continue;
        };
        if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
            parts.push(text.to_string());
            continue;
        }
        // Anthropic-aligned schema sometimes nests text under
        // `payload.text`. Honor both shapes.
        if let Some(text) = obj
            .get("payload")
            .and_then(|p| p.get("text"))
            .and_then(|v| v.as_str())
        {
            parts.push(text.to_string());
            continue;
        }
        let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        tracing::warn!(
            provider = provider_id,
            detail_type = kind,
            "preserve_history_reasoning_content: non-text reasoning detail dropped during lowering",
        );
    }
    parts.join("")
}

/// Set of OpenAI/DeepSeek-style request fields that reasoning-only
/// models reject. Applied via [`drop_sampling_params`].
pub(super) const SAMPLING_DROP: &[&str] = &[
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "logprobs",
    "top_logprobs",
];

pub(super) fn drop_sampling_params(obj: &mut serde_json::Map<String, Value>) {
    for key in SAMPLING_DROP {
        obj.remove(*key);
    }
}
