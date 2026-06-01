//! Per-dialect response normalization.
//!
//! Converts a raw upstream JSON body into a routectl `ChatResponse`.
//! Each dialect has a single responsibility:
//!   - OpenAi/OpenRouter/Passthrough: direct deserialization; lift any
//!     stray `reasoning_content` for safety.
//!   - DeepSeek/Vllm: lift `message.reasoning_content` ->
//!     `reasoning_details[type=reasoning.text, format=<dialect-tag>]`.
//!   - RawThinkTag: regex-strip `<think>...</think>` blocks from content,
//!     push them as `reasoning_details`.

use serde_json::Value;

use routectl_core::{ChatResponse, Error, Message, MessageContent, Result, Usage};

use super::dialect::ReasoningDialect;

/// OpenAI-compat envelope fields that must NOT round-trip through the
/// canonical extras catchall to a non-OpenAI ingress. The forward-compat
/// catchall on `ChatResponse.extras` is designed for Anthropic-spec
/// additions (e.g. `context_management`); when the upstream is openai-
/// compat, the catchall would otherwise carry the OpenAI envelope's
/// `object`/`system_fingerprint` and the DeepSeek vendor `cost` field
/// through to an Anthropic egress unchanged. Strip them at the
/// normalize seam so canonical stays provider-agnostic.
///
/// Conservatively excluded from the strip list:
///   - `role`: not actually leaked here. The Anthropic ingress's
///     render emits `"role":"assistant"` from a hardcoded insert, not
///     via `extras`.
///   - `service_tier`: Anthropic-spec response field; must round-trip.
///
/// `pub` so cross-crate integration tests can pin against the same
/// allow-list rather than mirroring it as a literal -- a mirror that
/// silently goes stale the next time a key is added here.
pub const OPENAI_COMPAT_ENVELOPE_KEYS: &[&str] = &["object", "system_fingerprint", "cost"];

/// OpenAI/DeepSeek usage sub-bag keys that must NOT round-trip through
/// `Usage.extras`. Two of them carry information canonical models in
/// typed fields (`reasoning_tokens`, `cache_read_input_tokens`) and we
/// lift the value into those before stripping; the other two are
/// either redundant (`prompt_cache_miss_tokens` = `prompt_tokens -
/// cache_read`) or pure OpenAI-vendor detail that does not map to any
/// canonical concept.
///
/// `pub` for the same cross-crate-test reason as
/// `OPENAI_COMPAT_ENVELOPE_KEYS`.
pub const OPENAI_COMPAT_USAGE_SUBKEYS: &[&str] = &[
    "prompt_cache_hit_tokens",
    "prompt_cache_miss_tokens",
    "prompt_tokens_details",
    "completion_tokens_details",
];

pub fn normalize(id: &str, raw: Value, dialect: ReasoningDialect) -> Result<ChatResponse> {
    let preprocessed = coalesce_reasoning_content_in_response(raw);
    let mut resp: ChatResponse = serde_json::from_value(preprocessed)
        .map_err(|e| Error::normalize_response(id, e.to_string()))?;

    for choice in resp.choices.iter_mut() {
        apply_dialect_to_message(id, &mut choice.message, dialect)?;
    }

    let lifted_usage_subkeys = if let Some(usage) = resp.usage.as_mut() {
        lift_and_strip_usage_extras(usage)
    } else {
        Vec::new()
    };
    let dropped_envelope_keys = strip_envelope_extras(&mut resp.extras);

    // Surface a single DEBUG line per normalize call when at least one
    // key was actually dropped or lifted, so an operator auditing
    // foreign-shape sanitization can grep the openai-compat target
    // instead of inferring success from the absence of fields. No-op
    // calls (clean Anthropic-spec responses) stay silent.
    if !dropped_envelope_keys.is_empty() || !lifted_usage_subkeys.is_empty() {
        tracing::debug!(
            target: "routectl::openai_compat",
            id,
            dropped_envelope_keys = ?dropped_envelope_keys,
            lifted_usage_subkeys = ?lifted_usage_subkeys,
            "openai-compat normalize: stripped vendor keys"
        );
    }

    Ok(resp)
}

/// Lift OpenAI / DeepSeek usage sub-bag values that canonical models
/// in typed fields, then strip the now-redundant sub-bags from
/// `Usage.extras`. Idempotent: callers can run it more than once on
/// the same `Usage` without losing data.
///
/// Lifts (when the canonical field is still `None`):
///   - `completion_tokens_details.reasoning_tokens` -> `reasoning_tokens`.
///   - `prompt_cache_hit_tokens` (DeepSeek) OR
///     `prompt_tokens_details.cached_tokens` (OpenAI) ->
///     `cache_read_input_tokens`.
///
/// DeepSeek precedence: when both `prompt_cache_hit_tokens` and
/// `prompt_tokens_details.cached_tokens` are present (rare),
/// DeepSeek's wins because it is the dialect for the host most likely
/// to ship both. Either way the cumulative semantics align with
/// Anthropic's `cache_read_input_tokens`.
///
/// Returns the list of `OPENAI_COMPAT_USAGE_SUBKEYS` that were
/// actually present (and therefore stripped) on this call. The
/// caller (`normalize`) uses the list for an audit DEBUG line; an
/// empty Vec means no-op.
pub(crate) fn lift_and_strip_usage_extras(usage: &mut Usage) -> Vec<&'static str> {
    if usage.reasoning_tokens.is_none() {
        usage.reasoning_tokens = usage
            .extras
            .get("completion_tokens_details")
            .and_then(|v| v.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
    }
    if usage.cache_read_input_tokens.is_none() {
        usage.cache_read_input_tokens = usage
            .extras
            .get("prompt_cache_hit_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                usage
                    .extras
                    .get("prompt_tokens_details")
                    .and_then(|v| v.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
            })
            .map(|n| n as u32);
    }
    let mut stripped: Vec<&'static str> = Vec::new();
    for k in OPENAI_COMPAT_USAGE_SUBKEYS {
        if usage.extras.remove(*k).is_some() {
            stripped.push(*k);
        }
    }
    stripped
}

/// Strip OpenAI-compat envelope keys from `ChatResponse.extras`.
/// Anthropic-spec / Anthropic-beta fields (`service_tier`,
/// `context_management`, `container`, ...) and any unknown forward-
/// compat keys stay intact. Returns the list of keys actually
/// removed so `normalize` can audit-log the strip.
fn strip_envelope_extras(extras: &mut serde_json::Map<String, Value>) -> Vec<&'static str> {
    let mut dropped: Vec<&'static str> = Vec::new();
    for k in OPENAI_COMPAT_ENVELOPE_KEYS {
        if extras.remove(*k).is_some() {
            dropped.push(*k);
        }
    }
    dropped
}

/// OpenAI-compat upstreams (DeepSeek, vLLM, OpenRouter, NIM, llama.cpp,
/// ...) don't expose the matched stop sequence on the response wire --
/// the spec carries only `finish_reason`, and most hosts strip the
/// matched sequence from the response content. The Anthropic ingress
/// needs `Choice.matched_stop_sequence` populated to render
/// `stop_reason:"stop_sequence"` instead of the lossy `end_turn`,
/// which breaks claude-code structured-output flows whose
/// `is_error: true` envelope is keyed on the wire `stop_reason`.
///
/// Heuristic: when the request shipped at least one stop_sequence AND
/// a Choice finished with `finish_reason == "stop"` AND the egress
/// hasn't already lifted a native value, look for a suffix match
/// against the response content. If exactly one stop_sequence was
/// configured and no suffix match is found (typical: the host
/// stripped the matched marker from the content), fall back to that
/// sole sequence -- this is the common structured-output pattern
/// (single fence, finish_reason "stop") and the alternative is a
/// hard `end_turn` mis-render. Multiple ambiguous stops without a
/// suffix hit stay `None` so we don't over-claim.
/// Public for integration-test access; not a stability surface --
/// `#[doc(hidden)]` keeps it out of the rendered docs.
#[doc(hidden)]
pub fn apply_stop_sequence_heuristic(
    chat_resp: &mut ChatResponse,
    request_stop: Option<&[String]>,
) {
    let stops = match request_stop {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    for choice in chat_resp.choices.iter_mut() {
        if choice.matched_stop_sequence.is_some() {
            continue;
        }
        if choice.finish_reason.as_deref() != Some("stop") {
            continue;
        }
        let text = message_text_for_match(&choice.message);
        choice.matched_stop_sequence = detect_matched_stop_sequence(text.as_deref(), stops);
    }
}

/// Pick a single suffix-matching stop sequence from `stops` if the
/// content trails one (host left it in the body). Otherwise, when
/// exactly one stop_sequence was configured AND the response actually
/// carried content, fall back to that sequence as the best-guess for
/// the structured-output single-fence flow. Returns `None` for
/// multiple ambiguous stops without a suffix hit, or when there is
/// no content to evidence either way.
///
/// Public for integration-test access; not a stability surface --
/// `#[doc(hidden)]` keeps it out of the rendered docs.
#[doc(hidden)]
pub fn detect_matched_stop_sequence(text: Option<&str>, stops: &[String]) -> Option<String> {
    // Bail when there is no content to evidence the heuristic. Common
    // cases: tool-only responses (defense in depth -- `finish_reason`
    // on tool turns is "tool_calls", not "stop", and the caller
    // already gates on "stop", but be explicit), reasoning-only
    // responses, and `MessageContent::Null` from non-Anthropic hosts.
    // Without this gate the single-stop fallback below would over-claim
    // on responses that never actually emitted a stop_sequence.
    let t = text?;
    let trimmed = t.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    // Try the longest sequences first so an inner "</answer>" wins over a
    // shorter "<" when both are configured. Stable on equal length.
    let mut ordered: Vec<&String> = stops.iter().filter(|s| !s.is_empty()).collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in ordered.iter() {
        // ASCII-safe on UTF-8 because `str::ends_with(&str)` aligns
        // on char boundaries. Note we trim trailing whitespace from
        // content but NOT from `stops` -- mirror trimming the stop
        // would silently rewrite an operator's explicit choice and is
        // not worth the bytes saved.
        if trimmed.ends_with(s.as_str()) {
            return Some((**s).clone());
        }
    }
    // Single-stop fallback: structured-output flows configure one fence
    // and rely on stop_reason discrimination. Without this fallback the
    // common case stays broken because most hosts strip the matched
    // marker from the content. Only safe when content was present
    // (the `trimmed.is_empty()` guard above handles tool-only / null /
    // whitespace-only responses).
    //
    // Residual risk: a non-structured-output flow that configures a
    // single stop_sequence and naturally ends mid-thought without
    // emitting the sequence gets `stop_sequence` instead of
    // `end_turn`. Operators tracking this can disable the fallback by
    // adopting an Anthropic-upstream provider, which surfaces the
    // matched sequence natively.
    if ordered.len() == 1 {
        return Some(ordered[0].clone());
    }
    None
}

fn message_text_for_match(msg: &Message) -> Option<String> {
    match &msg.content {
        MessageContent::Text(t) => Some(t.clone()),
        MessageContent::Parts(parts) => {
            let mut buf = String::new();
            for p in parts {
                if let routectl_core::ContentPart::Known(routectl_core::KnownContentPart::Text {
                    text,
                    ..
                }) = p
                {
                    buf.push_str(text);
                }
            }
            if buf.is_empty() {
                None
            } else {
                Some(buf)
            }
        }
        MessageContent::Null => None,
    }
}

/// Coalesce `message.reasoning_content` -> `message.reasoning` across all
/// choices, so downstream serde deserialization sees a single canonical key.
///
/// Some providers emit BOTH (NIM's llama-3.3 returns both fields, often
/// null) which causes serde to refuse the alias mapping with "duplicate
/// field `reasoning`". By rewriting the JSON to a single field name
/// before deserializing, we avoid the collision and pick the non-null
/// value when only one is set.
pub(crate) fn coalesce_reasoning_content_in_response(mut raw: Value) -> Value {
    if let Some(choices) = raw.get_mut("choices").and_then(|v| v.as_array_mut()) {
        for choice in choices.iter_mut() {
            if let Some(msg) = choice.get_mut("message").and_then(|v| v.as_object_mut()) {
                merge_reasoning_keys(msg);
            }
        }
    }
    raw
}

/// Merge `reasoning_content` into `reasoning`, preferring whichever is a
/// non-null string. Always strips `reasoning_content` after.
pub(crate) fn merge_reasoning_keys(obj: &mut serde_json::Map<String, Value>) {
    let rc = obj.remove("reasoning_content");
    let r_is_null = obj.get("reasoning").map_or(true, |v| v.is_null());
    if r_is_null {
        // Either no `reasoning` key, or it's null. Promote rc if non-null.
        match rc {
            Some(v) if !v.is_null() => {
                obj.insert("reasoning".into(), v);
            }
            _ => {
                obj.remove("reasoning");
            }
        }
    }
}

fn apply_dialect_to_message(id: &str, msg: &mut Message, dialect: ReasoningDialect) -> Result<()> {
    dialect.as_dyn().apply_response(id, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{MessageContent, ReasoningDetailKind};
    use serde_json::json;

    fn fake_response(content: &str) -> Value {
        json!({
            "id": "chatcmpl-test",
            "model": "test-model",
            "created": 1_700_000_000_i64,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            }
        })
    }

    fn fake_response_with_reasoning(content: &str, reasoning: &str) -> Value {
        json!({
            "id": "chatcmpl-test",
            "model": "test-model",
            "created": 1_700_000_000_i64,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                    "reasoning": reasoning
                },
                "finish_reason": "stop"
            }]
        })
    }

    #[test]
    fn openai_passthrough() {
        let raw = fake_response("hello");
        let resp = normalize("test", raw, ReasoningDialect::OpenAi).unwrap();
        assert_eq!(resp.choices[0].message.reasoning_details.len(), 0);
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn deepseek_lifts_reasoning_content() {
        let raw = fake_response_with_reasoning("The answer is 42", "Let me think step by step");
        let resp = normalize("test", raw, ReasoningDialect::DeepSeek).unwrap();
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("deepseek-v1"));
        assert!(matches!(details[0].kind, ReasoningDetailKind::Text));
        assert_eq!(details[0].payload["text"], "Let me think step by step");
    }

    #[test]
    fn vllm_lifts_reasoning_content() {
        let raw = fake_response_with_reasoning("result", "vllm reasoning trace");
        let resp = normalize("test", raw, ReasoningDialect::Vllm).unwrap();
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("vllm-reasoning-v1"));
        assert!(matches!(details[0].kind, ReasoningDetailKind::Text));
    }

    #[test]
    fn raw_think_tag_strips_and_lifts() {
        let raw = fake_response("<think>inner thought</think>The answer is 42");
        let resp = normalize("test", raw, ReasoningDialect::RawThinkTag).unwrap();
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("raw-think-tag-v1"));
        assert_eq!(details[0].payload["text"], "inner thought");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "The answer is 42"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn raw_think_tag_multiline() {
        let raw = fake_response("<think>\nline1\nline2\n</think>After thought");
        let resp = normalize("test", raw, ReasoningDialect::RawThinkTag).unwrap();
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details[0].payload["text"], "\nline1\nline2\n");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t.trim(), "After thought"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn passthrough_no_mutation() {
        let raw = fake_response("keep me");
        let resp = normalize("test", raw, ReasoningDialect::Passthrough).unwrap();
        assert_eq!(resp.choices[0].message.reasoning_details.len(), 0);
    }

    #[test]
    fn openrouter_no_mutation() {
        let raw = fake_response("openrouter content");
        let resp = normalize("test", raw, ReasoningDialect::OpenRouter).unwrap();
        assert_eq!(resp.choices[0].message.reasoning_details.len(), 0);
    }

    /// The forward-compat catchall on `ChatResponse.extras` exists for
    /// Anthropic-spec additions; openai-compat envelope fields
    /// (`object`, `system_fingerprint`, `cost`) must NOT round-trip
    /// through it to a non-OpenAI egress. Pin the strip.
    #[test]
    fn envelope_keys_stripped_from_extras() {
        let raw = json!({
            "id": "chatcmpl-test",
            "model": "test-model",
            "object": "chat.completion",
            "system_fingerprint": "fp_test_v1",
            "cost": "0",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }]
        });
        let resp = normalize("test", raw, ReasoningDialect::OpenAi).unwrap();
        assert!(
            !resp.extras.contains_key("object"),
            "object must be stripped, got extras={:?}",
            resp.extras
        );
        assert!(!resp.extras.contains_key("system_fingerprint"));
        assert!(!resp.extras.contains_key("cost"));
    }

    /// `service_tier` is an Anthropic-spec response field shipped on
    /// every recent Anthropic response; the openai-compat normalize
    /// path must not strip it (the strip list is openai-compat-only).
    #[test]
    fn service_tier_preserved_through_normalize() {
        let raw = json!({
            "id": "test",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "service_tier": "standard"
        });
        let resp = normalize("test", raw, ReasoningDialect::OpenAi).unwrap();
        assert_eq!(resp.extras.get("service_tier"), Some(&json!("standard")));
    }

    /// `completion_tokens_details.reasoning_tokens` lifts into
    /// canonical `Usage.reasoning_tokens` and the sub-bag is stripped.
    #[test]
    fn usage_lifts_reasoning_tokens_from_completion_details() {
        let raw = json!({
            "id": "test", "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30,
                "completion_tokens_details": {"reasoning_tokens": 7}
            }
        });
        let resp = normalize("test", raw, ReasoningDialect::OpenAi).unwrap();
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.reasoning_tokens, Some(7));
        assert!(!usage.extras.contains_key("completion_tokens_details"));
    }

    /// DeepSeek's `prompt_cache_hit_tokens` lifts into canonical
    /// `Usage.cache_read_input_tokens`. The vendor sibling
    /// `prompt_cache_miss_tokens` is stripped without a lift (no
    /// canonical slot; equivalent to `prompt_tokens - cache_read`).
    #[test]
    fn usage_lifts_cache_read_from_deepseek_prompt_cache_hit() {
        let raw = json!({
            "id": "test", "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
                "prompt_cache_hit_tokens": 80,
                "prompt_cache_miss_tokens": 20
            }
        });
        let resp = normalize("test", raw, ReasoningDialect::DeepSeek).unwrap();
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.cache_read_input_tokens, Some(80));
        assert!(!usage.extras.contains_key("prompt_cache_hit_tokens"));
        assert!(!usage.extras.contains_key("prompt_cache_miss_tokens"));
    }

    /// OpenAI's `prompt_tokens_details.cached_tokens` lifts into
    /// canonical `Usage.cache_read_input_tokens` when the DeepSeek
    /// `prompt_cache_hit_tokens` sibling is absent.
    #[test]
    fn usage_lifts_cache_read_from_openai_prompt_tokens_details() {
        let raw = json!({
            "id": "test", "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
                "prompt_tokens_details": {"cached_tokens": 64}
            }
        });
        let resp = normalize("test", raw, ReasoningDialect::OpenAi).unwrap();
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.cache_read_input_tokens, Some(64));
        assert!(!usage.extras.contains_key("prompt_tokens_details"));
    }

    /// When both DeepSeek-style and OpenAI-style cache-hit fields are
    /// present, DeepSeek wins. Documents the precedence so a future
    /// reader sees the intent.
    #[test]
    fn usage_deepseek_cache_hit_wins_over_openai_cached() {
        let raw = json!({
            "id": "test", "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
                "prompt_cache_hit_tokens": 80,
                "prompt_tokens_details": {"cached_tokens": 64}
            }
        });
        let resp = normalize("test", raw, ReasoningDialect::DeepSeek).unwrap();
        assert_eq!(resp.usage.unwrap().cache_read_input_tokens, Some(80));
    }

    /// Idempotency: when canonical `reasoning_tokens` is already set
    /// (e.g. the upstream put it at the top of `usage` directly), the
    /// sub-bag lift does NOT overwrite it. Pins the `or_insert`-style
    /// semantics intended by the helper.
    #[test]
    fn usage_lift_does_not_clobber_already_set_canonical_field() {
        let raw = json!({
            "id": "test", "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30,
                "reasoning_tokens": 11,
                "completion_tokens_details": {"reasoning_tokens": 99}
            }
        });
        let resp = normalize("test", raw, ReasoningDialect::OpenAi).unwrap();
        assert_eq!(resp.usage.unwrap().reasoning_tokens, Some(11));
    }

    /// Audit trail for foreign-shape sanitization: when the helpers
    /// actually drop or lift a key, they return the affected names.
    /// `normalize` then DEBUG-logs the lists so an operator triaging a
    /// vendor upstream can grep `routectl::openai_compat` instead of
    /// inferring success from the absence of fields. The logging
    /// itself runs on a tracing subscriber and isn't asserted here;
    /// pinning the helper return values gives the same coverage at
    /// zero subscriber cost.
    #[test]
    fn strip_helpers_return_actually_removed_keys() {
        // Arrange
        let mut extras = serde_json::Map::new();
        extras.insert("object".into(), json!("chat.completion"));
        extras.insert("system_fingerprint".into(), json!("fp"));
        extras.insert("service_tier".into(), json!("standard"));
        let mut usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            extras: {
                let mut m = serde_json::Map::new();
                m.insert("prompt_cache_hit_tokens".into(), json!(80));
                m.insert("prompt_tokens_details".into(), json!({"cached_tokens": 64}));
                m
            },
            ..Default::default()
        };

        // Act
        let dropped = strip_envelope_extras(&mut extras);
        let lifted = lift_and_strip_usage_extras(&mut usage);

        // Assert: only keys actually present are reported.
        assert!(dropped.contains(&"object"), "got {dropped:?}");
        assert!(dropped.contains(&"system_fingerprint"), "got {dropped:?}");
        assert!(
            !dropped.contains(&"cost"),
            "cost was not in extras; must not be reported as dropped, got {dropped:?}"
        );
        assert!(
            lifted.contains(&"prompt_cache_hit_tokens"),
            "got {lifted:?}"
        );
        assert!(lifted.contains(&"prompt_tokens_details"), "got {lifted:?}");
        // Anthropic-spec field stays.
        assert!(extras.contains_key("service_tier"));
    }

    /// Counterpart: clean-upstream calls return empty Vecs so
    /// `normalize` can no-op-skip the audit DEBUG line.
    #[test]
    fn strip_helpers_return_empty_when_nothing_to_strip() {
        // Arrange: no envelope keys, no usage sub-bags.
        let mut extras = serde_json::Map::new();
        extras.insert("service_tier".into(), json!("priority"));
        let mut usage = Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        };

        // Act
        let dropped = strip_envelope_extras(&mut extras);
        let lifted = lift_and_strip_usage_extras(&mut usage);

        // Assert
        assert!(
            dropped.is_empty(),
            "no envelope keys present; got {dropped:?}"
        );
        assert!(
            lifted.is_empty(),
            "no usage sub-bags present; got {lifted:?}"
        );
    }
}
