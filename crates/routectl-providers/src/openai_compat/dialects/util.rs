//! Helpers shared between dialect impls. Everything here is
//! `pub(super)` -- only siblings in `dialects/` should reach in.
//!
//! These helpers all mutate their inputs in place to fit the trait's
//! `&mut`-by-reference shape; a return is reserved for genuine errors.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use routectl_core::{ChatRequest, Error, Message, MessageContent, Result};

use crate::openai_compat::util::build_reasoning_detail;

/// Budget threshold (tokens) above which `reasoning.max_tokens`
/// is mapped to "high" effort; below is "medium". Shared by the
/// DeepSeek and vLLM dialects, which derive effort identically.
const BUDGET_HIGH_THRESHOLD: u32 = 8192;

/// Whether reasoning is "on for wire purposes" -- the single predicate that
/// gates BOTH the vLLM `chat_template_kwargs.enable_thinking` flag and the
/// `reasoning_effort` emission, so the two can never disagree.
///
/// Contract (exact):
///   `enabled == Some(false)`                                        -> false
///   `enabled == Some(true) || effort.is_some() || max_tokens.is_some()` -> true
///   otherwise (no reasoning config, or a config with no signal)      -> false
///
/// A present `effort`/`max_tokens` with `enabled` unset is a reasoning signal
/// (this is what the OpenAI ingress produces when promoting a top-level
/// `reasoning_effort`); explicit `Some(false)` always wins.
///
/// Shared by the DeepSeek and vLLM dialects; hoisted here so they cannot drift.
pub(super) fn reasoning_enabled_for_wire(req: &ChatRequest) -> bool {
    let Some(r) = req.reasoning.as_ref() else {
        return false;
    };
    if r.enabled == Some(false) {
        return false;
    }
    r.enabled == Some(true) || r.effort.is_some() || r.max_tokens.is_some()
}

/// Derive a `reasoning_effort` string from the canonical reasoning config.
/// Returns `Some(effort)` when the request carries a reasoning signal;
/// returns `None` when reasoning is absent or explicitly disabled.
///
/// Precedence: explicit `effort` > derived from `max_tokens` > None.
///
/// Shared by the DeepSeek and vLLM dialects (identical derivation). Gated by
/// [`reasoning_enabled_for_wire`] so the effort emission and vLLM's
/// `enable_thinking` flag are driven by one predicate.
pub(super) fn derive_reasoning_effort(req: &ChatRequest) -> Option<String> {
    // Disabled (or signal-less) reasoning must not leak an effort onto the
    // wire, even when the model's output_config set a default effort level.
    if !reasoning_enabled_for_wire(req) {
        return None;
    }
    let r = req.reasoning.as_ref()?;
    // Explicit effort wins over everything; passthrough verbatim.
    if let Some(effort) = r.effort.as_deref() {
        return Some(effort.to_string());
    }
    // Derive from max_tokens when effort is absent.
    if let Some(budget) = r.max_tokens {
        let effort = if budget >= BUDGET_HIGH_THRESHOLD {
            "high"
        } else {
            "medium"
        };
        return Some(effort.to_string());
    }
    None
}

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
///
/// `reasoning_index` is a per-stream, monotonically incrementing counter
/// owned by the streaming caller. Each emitted detail takes the current
/// value and then advances it, so successive streamed reasoning deltas
/// carry distinct `index` values (0, 1, 2, ...) rather than collapsing
/// onto index 0. Downstream consumers key on this index for ordering and
/// block identity.
pub(super) fn lift_delta_reasoning_content(
    id: &str,
    val: &mut Value,
    format_tag: &str,
    reasoning_index: &mut u32,
) -> Result<()> {
    // A usage-only terminal frame (stream_options.include_usage) carries
    // no `choices` key. Pass it through untouched rather than aborting the
    // whole stream with a missing-choices error.
    let Some(choices) = val.get_mut("choices").and_then(|v| v.as_array_mut()) else {
        return Ok(());
    };

    for choice in choices.iter_mut() {
        let delta = match choice.get_mut("delta").and_then(|v| v.as_object_mut()) {
            Some(d) => d,
            None => continue,
        };

        let rc = match delta.get("reasoning") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => continue,
        };

        let detail = build_reasoning_detail(&rc, format_tag, *reasoning_index);
        // saturating_add so a multi-day-running stream (4B+ reasoning
        // chunks) wraps to a no-op rather than panicking on overflow,
        // mirroring ThinkTagAccumulator's chunk_index contract.
        *reasoning_index = reasoning_index.saturating_add(1);
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
/// string field. DeepSeek v4+ and recent vLLM require this on
/// echo-back; the canonical schema uses `reasoning` for OpenRouter
/// compat, so we rename. Falls back to lowering `reasoning_details`
/// to a joined text string.
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
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant" {
            // DeepSeek 400s if reasoning fields appear on user/tool messages.
            m.remove("reasoning_content");
            m.remove("reasoning_details");
            m.remove("reasoning");
            continue;
        }
        if m.contains_key("reasoning_content") {
            m.remove("reasoning_details");
            m.remove("reasoning");
            continue;
        }
        // Treat `Value::Null` in `reasoning` as absent so NIM's dual-null
        // shape doesn't preempt the reasoning_details fallback.
        if let Some(reasoning) = m.get("reasoning") {
            if !reasoning.is_null() {
                if let Some(s) = reasoning.as_str()
                    && !s.is_empty()
                {
                    m.insert("reasoning_content".into(), Value::String(s.to_string()));
                }
                m.remove("reasoning");
                m.remove("reasoning_details");
                continue;
            }
            m.remove("reasoning");
        }
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
/// typed shape verbatim. If only the plaintext `reasoning` slot is
/// present, lift it into a single typed entry tagged with
/// `format_tag`.
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
            .and_then(|v| v.as_str().map(std::string::ToString::to_string))
            .or_else(|| {
                m.remove("reasoning_content")
                    .and_then(|v| v.as_str().map(std::string::ToString::to_string))
            });
        if let Some(text) = plaintext
            && !text.is_empty()
        {
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
    Ok(())
}

/// Lower a `reasoning_details` array (Anthropic-aligned typed shape)
/// to plaintext by joining each entry's `text` (or `payload.text`)
/// in order. Non-text entries warn + skip.
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
        // Anthropic-aligned schema nests text under `payload.text`.
        if let Some(text) = obj
            .get("payload")
            .and_then(|p| p.get("text"))
            .and_then(|v| v.as_str())
        {
            parts.push(text.to_string());
            continue;
        }
        let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        // Char-boundary-safe truncation: `&str[..n]` panics on non-ASCII
        // when n splits a multi-byte char. `kind` is upstream JSON and
        // may be Unicode.
        let kind_for_log = match kind.char_indices().nth(64) {
            Some((boundary, _)) => &kind[..boundary],
            None => kind,
        };
        tracing::warn!(
            provider = provider_id,
            detail_type = kind_for_log,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_with_messages(messages: Value) -> serde_json::Map<String, Value> {
        let mut obj = serde_json::Map::new();
        obj.insert("messages".into(), messages);
        obj
    }

    // ----- strip_history_reasoning -----

    #[test]
    fn strip_history_reasoning_removes_all_three_fields_from_assistant() {
        let mut obj = body_with_messages(json!([{
            "role": "assistant",
            "content": "ok",
            "reasoning_content": "hidden",
            "reasoning_details": [{"type":"reasoning.text","text":"d"}],
            "reasoning": "r",
        }]));
        strip_history_reasoning("test", &mut obj).unwrap();
        let m = obj["messages"][0].as_object().unwrap();
        assert!(m.get("reasoning_content").is_none());
        assert!(m.get("reasoning_details").is_none());
        assert!(m.get("reasoning").is_none());
        // Non-reasoning fields untouched.
        assert_eq!(m["content"], "ok");
    }

    #[test]
    fn strip_history_reasoning_no_op_when_no_reasoning_fields() {
        let mut obj = body_with_messages(json!([{"role":"user","content":"hi"}]));
        let before = obj["messages"][0].clone();
        strip_history_reasoning("test", &mut obj).unwrap();
        assert_eq!(obj["messages"][0], before);
    }

    // ----- preserve_history_reasoning_content -----

    #[test]
    fn preserve_content_renames_reasoning_to_reasoning_content_on_assistant() {
        // Canonical lands as `reasoning` on the wire (per the schema's
        // serde rename). DeepSeek v4 wants it as `reasoning_content`.
        let mut obj = body_with_messages(json!([{
            "role": "assistant",
            "content": "ok",
            "reasoning": "I considered the options",
        }]));
        preserve_history_reasoning_content("test", &mut obj).unwrap();
        let m = obj["messages"][0].as_object().unwrap();
        assert_eq!(
            m.get("reasoning_content").and_then(|v| v.as_str()),
            Some("I considered the options")
        );
        assert!(m.get("reasoning").is_none());
        assert!(m.get("reasoning_details").is_none());
    }

    #[test]
    fn preserve_content_keeps_existing_reasoning_content_intact() {
        let mut obj = body_with_messages(json!([{
            "role": "assistant",
            "content": "ok",
            "reasoning_content": "already there",
            "reasoning": "should be dropped",
            "reasoning_details": [{"type":"reasoning.text","text":"d"}],
        }]));
        preserve_history_reasoning_content("test", &mut obj).unwrap();
        let m = obj["messages"][0].as_object().unwrap();
        assert_eq!(m["reasoning_content"], "already there");
        assert!(m.get("reasoning").is_none());
        assert!(m.get("reasoning_details").is_none());
    }

    #[test]
    fn preserve_content_lowers_reasoning_details_array_to_string() {
        // When only the typed array is present, lower it to a single
        // joined string so DeepSeek can echo it as `reasoning_content`.
        let mut obj = body_with_messages(json!([{
            "role": "assistant",
            "content": "ok",
            "reasoning_details": [
                {"type":"reasoning.text","text":"first ","format":"deepseek-v1"},
                {"type":"reasoning.text","text":"second","format":"deepseek-v1"},
            ],
        }]));
        preserve_history_reasoning_content("test", &mut obj).unwrap();
        let m = obj["messages"][0].as_object().unwrap();
        assert_eq!(m["reasoning_content"], "first second");
        assert!(m.get("reasoning_details").is_none());
    }

    #[test]
    fn preserve_content_strips_reasoning_from_non_assistant_messages() {
        // DeepSeek 400s if reasoning fields appear on user/tool
        // messages too. Belt-and-braces: strip them from any role
        // that isn't assistant.
        let mut obj = body_with_messages(json!([
            {"role":"user","content":"hi","reasoning":"shouldnotbehere"},
            {"role":"tool","content":"r","reasoning_content":"alsono"},
        ]));
        preserve_history_reasoning_content("test", &mut obj).unwrap();
        let user = obj["messages"][0].as_object().unwrap();
        let tool = obj["messages"][1].as_object().unwrap();
        assert!(user.get("reasoning").is_none());
        assert!(tool.get("reasoning_content").is_none());
    }

    #[test]
    fn preserve_content_drops_empty_reasoning() {
        // Empty reasoning string is not worth round-tripping; drop.
        let mut obj = body_with_messages(json!([{
            "role": "assistant",
            "content": "ok",
            "reasoning": "",
        }]));
        preserve_history_reasoning_content("test", &mut obj).unwrap();
        let m = obj["messages"][0].as_object().unwrap();
        assert!(m.get("reasoning_content").is_none());
        assert!(m.get("reasoning").is_none());
    }

    // ----- preserve_history_reasoning_details -----

    #[test]
    fn preserve_details_keeps_existing_array_drops_legacy_slots() {
        let mut obj = body_with_messages(json!([{
            "role": "assistant",
            "content": "ok",
            "reasoning_details": [{"type":"reasoning.text","text":"already"}],
            "reasoning": "stale legacy",
            "reasoning_content": "stale legacy",
        }]));
        preserve_history_reasoning_details("test", &mut obj, "deepseek-v1").unwrap();
        let m = obj["messages"][0].as_object().unwrap();
        assert!(m.get("reasoning_details").is_some());
        assert_eq!(m["reasoning_details"][0]["text"], "already");
        assert!(m.get("reasoning").is_none());
        assert!(m.get("reasoning_content").is_none());
    }

    #[test]
    fn preserve_details_lifts_reasoning_string_into_typed_array() {
        let mut obj = body_with_messages(json!([{
            "role": "assistant",
            "content": "ok",
            "reasoning": "thought process",
        }]));
        preserve_history_reasoning_details("test", &mut obj, "openrouter-v1").unwrap();
        let m = obj["messages"][0].as_object().unwrap();
        let arr = m["reasoning_details"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "reasoning.text");
        assert_eq!(arr[0]["format"], "openrouter-v1");
        assert_eq!(arr[0]["text"], "thought process");
        assert_eq!(arr[0]["index"], 0);
    }

    /// derive_reasoning_effort suppresses effort when reasoning is
    /// explicitly disabled (`enabled == Some(false)`), even if an effort
    /// value is present. The hoisted single definition guards both
    /// DeepSeek and vLLM.
    #[test]
    fn derive_reasoning_effort_none_when_disabled() {
        use routectl_core::{ChatRequest, ReasoningConfig};
        let req = ChatRequest {
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: Some(16000),
                enabled: Some(false),
                exclude: None,
            }),
            ..Default::default()
        };
        assert!(
            derive_reasoning_effort(&req).is_none(),
            "disabled reasoning must derive no effort"
        );
    }

    /// The shared wire-enabled predicate: explicit Some(false) wins;
    /// otherwise a present enabled/effort/max_tokens signal turns it on;
    /// an empty or absent config is off.
    #[test]
    fn reasoning_enabled_for_wire_contract() {
        use routectl_core::{ChatRequest, ReasoningConfig};

        let with = |r: Option<ReasoningConfig>| ChatRequest {
            reasoning: r,
            ..Default::default()
        };
        let cfg = |enabled, effort: Option<&str>, max_tokens| ReasoningConfig {
            effort: effort.map(str::to_string),
            max_tokens,
            enabled,
            exclude: None,
        };

        // No reasoning config -> off.
        assert!(!reasoning_enabled_for_wire(&with(None)));
        // Explicit false wins even with effort + max_tokens present.
        assert!(!reasoning_enabled_for_wire(&with(Some(cfg(
            Some(false),
            Some("high"),
            Some(16000)
        )))));
        // enabled unset + effort -> on.
        assert!(reasoning_enabled_for_wire(&with(Some(cfg(
            None,
            Some("high"),
            None
        )))));
        // enabled unset + max_tokens -> on.
        assert!(reasoning_enabled_for_wire(&with(Some(cfg(
            None,
            None,
            Some(4096)
        )))));
        // enabled true, no other signal -> on.
        assert!(reasoning_enabled_for_wire(&with(Some(cfg(
            Some(true),
            None,
            None
        )))));
        // Config present but no signal -> off.
        assert!(!reasoning_enabled_for_wire(&with(Some(cfg(
            None, None, None
        )))));
    }

    /// A usage-only terminal frame (no `choices` key) must pass
    /// through `lift_delta_reasoning_content` as `Ok(())` rather than
    /// aborting the stream with a missing-choices error. The usage object
    /// is left untouched.
    #[test]
    fn lift_delta_reasoning_content_passes_usage_only_frame() {
        let mut val = json!({
            "id": "chunk-final",
            "model": "test",
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        });
        let mut idx: u32 = 0;
        let result = lift_delta_reasoning_content("test", &mut val, "deepseek-v1", &mut idx);
        assert!(result.is_ok(), "usage-only frame must not error");
        // Usage object preserved verbatim; no choices were synthesized.
        assert_eq!(val["usage"]["total_tokens"], 30);
        assert!(val.get("choices").is_none());
        assert_eq!(idx, 0, "no reasoning detail -> index unchanged");
    }

    #[test]
    fn lower_reasoning_details_with_unicode_type_does_not_panic() {
        // Regression: a non-ASCII `type` field (legitimate for
        // non-English locales or fuzzing) used to panic the log
        // truncation when the byte slice landed inside a multi-byte
        // char. Char-boundary-safe truncation must handle this.
        // 4-byte chars: a string of 100 emoji is 400 bytes, byte 64
        // lands inside char 16's third byte.
        let multi_byte_type = "\u{1F4A1}".repeat(100); // 100 light bulb emoji
        let details = json!([{
            "type": multi_byte_type,
            // No `text` field -> falls through to the warn path that
            // truncates `kind` for logging.
        }]);
        // Must not panic.
        let _ = lower_reasoning_details_to_text(&details, "test");
    }
}
