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
pub(super) fn strip_history_reasoning(
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
