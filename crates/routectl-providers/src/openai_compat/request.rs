//! Top-level request normalization. Walks a routectl `ChatRequest` into
//! the JSON body sent upstream:
//!
//!   1. Direct serde serialization of `ChatRequest`.
//!   2. Drop routectl-internal fields the upstream never wants.
//!   3. Apply outgoing-history reasoning policy (strip vs preserve).
//!   4. Hand off dialect-specific shaping to `Dialect::apply_request`.
//!   5. Merge `default_extras` then `provider_extras` last (callers win).
//!
//! Per-dialect logic lives in `dialects/*.rs`; this module is a thin
//! envelope around that dispatch.

use serde_json::Value;
use tracing::warn;

use routectl_core::{ChatRequest, Error, Result, ToolDef};

use super::dialect::ReasoningDialect;
use super::dialects::util::strip_history_reasoning;
use super::HistoryReasoning;

pub fn normalize(
    id: &str,
    req: &ChatRequest,
    dialect: ReasoningDialect,
    history_reasoning: HistoryReasoning,
    default_extras: Option<&Value>,
    strict_translation: bool,
) -> Result<Value> {
    // Lossy seams (Anthropic-canonical fields the OpenAI-compat wire
    // can't carry): cache_control, anthropic_beta, ToolDef::Other /
    // ContentPart::Other catchalls (Anthropic builtin tools and
    // forward-compat block types), SystemContent::Blocks with
    // per-block cache_control. Default mode warns and continues;
    // strict_translation flips this to a hard 400.
    check_dropped_anthropic_fields(id, req, strict_translation)?;

    let mut body =
        serde_json::to_value(req).map_err(|e| Error::normalize_request(id, e.to_string()))?;

    let obj = body
        .as_object_mut()
        .ok_or_else(|| Error::normalize_request(id, "serialized request is not an object"))?;

    // Remove routectl-internal fields that upstream never wants.
    // Dialects that need `chat_template_kwargs` re-inject it themselves.
    obj.remove("reasoning");
    obj.remove("provider_extras");
    obj.remove("chat_template_kwargs");
    obj.remove("cache_control");
    obj.remove("anthropic_beta");

    // Outgoing-history reasoning policy. Resolution rules:
    //
    //   - HistoryReasoning::Auto: defer to the dialect's default. If
    //     the dialect declares `strip_history_reasoning()`, run the
    //     strip helper. Otherwise leave the canonical-shape reasoning
    //     fields in the body (OpenAI / OpenRouter / Passthrough). For
    //     OpenRouter this means the typed `reasoning_details` array
    //     reaches the upstream verbatim, which is the right shape.
    //   - HistoryReasoning::Strip: always strip, regardless of
    //     dialect. Use for DeepSeek v3 / vLLM <= 0.6 hosts that 400
    //     on echo-back.
    //   - HistoryReasoning::Preserve: hand off to the dialect's
    //     `preserve_history_reasoning` impl, which knows the
    //     dialect-native preserve shape (`reasoning_content` for
    //     DeepSeek/Vllm, `reasoning_details` for OpenRouter, no-op
    //     for OpenAI / Passthrough since neither has a preserve
    //     shape on the wire).
    let dyn_dialect = dialect.as_dyn();
    match history_reasoning {
        HistoryReasoning::Auto => {
            if dyn_dialect.strip_history_reasoning() {
                strip_history_reasoning(id, obj)?;
            }
        }
        HistoryReasoning::Strip => {
            strip_history_reasoning(id, obj)?;
        }
        HistoryReasoning::Preserve => {
            dyn_dialect.preserve_history_reasoning(id, obj)?;
        }
    }

    dyn_dialect.apply_request(id, obj, req)?;

    // Merge default_extras first, then provider_extras (caller wins),
    // BOTH gated by the routectl-managed-keys allow-list. Without
    // this, a request body of
    //   `provider_extras = {"messages": [{"role":"user","content":"INJECTED"}]}`
    // would replace the assembled messages array. The Anthropic
    // egress already enforces this at
    // `anthropic_api/request.rs::merge_provider_extras`; this is the
    // matching guard for openai-compat. `default_extras` is config-
    // file-controlled (operator) but we apply the same filter for
    // symmetry and so a future "allow operator override" toggle
    // doesn't have to retrofit the filter on the request side.
    if let Some(extras) = default_extras {
        merge_extras(id, obj, extras, "default_extras");
    }
    if let Some(extras) = req.provider_extras.as_ref() {
        merge_extras(id, obj, extras, "provider_extras");
    }

    Ok(body)
}

/// Shallow-merge `extras` into `obj` with a routectl-managed-keys
/// allow-list. Drop + WARN when an extras entry tries to override a
/// key routectl owns (model, messages, stream, tools, etc.). The
/// `source` arg names where the override came from for log
/// readability.
fn merge_extras(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    extras: &Value,
    source: &'static str,
) {
    let Some(extra_obj) = extras.as_object() else {
        return;
    };
    for (k, v) in extra_obj {
        if is_routectl_managed_key(k) {
            tracing::warn!(
                provider = id,
                source = source,
                key = %k,
                "extras attempted to override routectl-managed key; dropped"
            );
            continue;
        }
        obj.insert(k.clone(), v.clone());
    }
}

/// Top-level OpenAI-shape body keys constructed by routectl that
/// `provider_extras` / `default_extras` are NOT permitted to override.
/// This is the FULL set of canonical `ChatRequest` fields that get
/// serialized into the wire body; long-tail provider knobs not in
/// canonical (`top_k`, `service_tier`, dialect-specific
/// `chat_template_kwargs`, vendor-specific `safety_settings`, etc.)
/// still pass through. Keep in sync with
/// `routectl_core::ChatRequest` field names; if a new canonical field
/// is added there, add it here too or callers can silently override
/// the canonical value via extras.
fn is_routectl_managed_key(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "messages"
            | "max_tokens"
            | "max_completion_tokens"
            | "stream"
            | "tools"
            | "tool_choice"
            | "stop"
            | "stop_sequences"
            | "temperature"
            | "top_p"
            | "n"
            | "user"
            // Sampling controls + reproducibility -- canonical fields,
            // not long-tail. Provider extras would silently shadow the
            // request's canonical value if these were absent.
            | "seed"
            | "logprobs"
            | "top_logprobs"
            | "logit_bias"
            | "presence_penalty"
            | "frequency_penalty"
            | "response_format"
    )
}

/// Emit `tracing::warn!` for each Anthropic-only canonical field that
/// the openai-compat egress will drop. Quiet when none apply (the
/// common case). This is the OpenAI-compat egress's contribution to
/// the "lossy seam policy" -- the v0.4.0 plan calls out that an
/// Anthropic-in / OpenAI-compat-out request should produce visible
/// signal when fields are dropped, not silent degradation.
///
/// In default mode each finding emits a `tracing::warn!` and the
/// function returns `Ok(())`. In strict mode the findings are
/// collected and returned as an `Error::Validation` (HTTP 400),
/// rejecting the request before it hits upstream.
fn check_dropped_anthropic_fields(id: &str, req: &ChatRequest, strict: bool) -> Result<()> {
    let mut findings: Vec<String> = Vec::new();
    let mut record = |msg: String| {
        if strict {
            findings.push(msg);
        }
    };

    if req.cache_control.is_some() {
        warn!(
            provider = id,
            "openai-compat egress: top-level cache_control dropped (prompt caching not supported)",
        );
        record("top-level cache_control".into());
    }
    if !req.anthropic_beta.is_empty() {
        warn!(
            provider = id,
            beta_flags = ?req.anthropic_beta,
            "openai-compat egress: anthropic_beta flags dropped (Anthropic-only)",
        );
        record(format!("anthropic_beta {:?}", req.anthropic_beta));
    }
    if let Some(routectl_core::SystemContent::Blocks(blocks)) = &req.system {
        let any_cc = blocks.iter().any(|b| b.cache_control.is_some());
        if any_cc {
            warn!(
                provider = id,
                "openai-compat egress: per-block cache_control on system dropped",
            );
            record("per-block cache_control on system".into());
        }
    }
    if let Some(tools) = &req.tools {
        for t in tools {
            if matches!(t, ToolDef::Other(_)) {
                warn!(
                    provider = id,
                    "openai-compat egress: Anthropic builtin / non-custom tool dropped",
                );
                record("Anthropic builtin / non-custom tool".into());
            } else if let ToolDef::Custom(c) = t {
                if c.cache_control.is_some() {
                    warn!(
                        provider = id,
                        tool = %c.name,
                        "openai-compat egress: tool cache_control dropped (Anthropic-only)",
                    );
                    record(format!("tool `{}` cache_control", c.name));
                }
            }
        }
    }
    for (i, m) in req.messages.iter().enumerate() {
        if let routectl_core::MessageContent::Parts(parts) = &m.content {
            for p in parts {
                match p {
                    routectl_core::ContentPart::Other { type_tag, .. } => {
                        warn!(
                            provider = id,
                            message_index = i,
                            block_type = %type_tag,
                            "openai-compat egress: forward-compat content block dropped",
                        );
                        record(format!("forward-compat block `{type_tag}` on message {i}"));
                    }
                    routectl_core::ContentPart::Known(k) => {
                        if k.cache_control().is_some() {
                            warn!(
                                provider = id,
                                message_index = i,
                                block_type = k.type_tag(),
                                "openai-compat egress: per-block cache_control dropped",
                            );
                            record(format!(
                                "per-block cache_control on message {i} ({})",
                                k.type_tag()
                            ));
                        }
                    }
                }
            }
        }
    }

    if strict && !findings.is_empty() {
        return Err(Error::Validation(format!(
            "strict_translation: {} canonical-only field(s) cannot be carried by openai-compat egress `{}`: {}",
            findings.len(),
            id,
            findings.join("; ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};
    use serde_json::json;

    fn simple_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(512),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.1),
            ..Default::default()
        }
    }

    #[test]
    fn openai_passthrough_normal_model() {
        let req = simple_req("gpt-4o");
        let body = normalize("test", &req, ReasoningDialect::OpenAi, HistoryReasoning::Auto, None, false).unwrap();
        // temperature preserved for non-reasoning models
        assert!(body.get("temperature").is_some());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("provider_extras").is_none());
    }

    #[test]
    fn openai_drops_sampling_for_o_series() {
        let req = simple_req("o3-mini");
        let body = normalize("test", &req, ReasoningDialect::OpenAi, HistoryReasoning::Auto, None, false).unwrap();
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("presence_penalty").is_none());
    }

    #[test]
    fn openai_maps_reasoning_effort() {
        let mut req = simple_req("o3");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });
        let body = normalize("test", &req, ReasoningDialect::OpenAi, HistoryReasoning::Auto, None, false).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_drops_sampling_for_reasoner() {
        let req = simple_req("deepseek-reasoner");
        let body = normalize("test", &req, ReasoningDialect::DeepSeek, HistoryReasoning::Auto, None, false).unwrap();
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn deepseek_strips_reasoning_content_from_history() {
        let mut req = simple_req("deepseek-reasoner");
        req.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Text("I thought about it".into()),
            reasoning: Some("hidden chain".into()),
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
        let body = normalize("test", &req, ReasoningDialect::DeepSeek, HistoryReasoning::Auto, None, false).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        for m in msgs {
            assert!(m.get("reasoning_content").is_none());
            assert!(m.get("reasoning").is_none());
            assert!(m.get("reasoning_details").is_none());
        }
    }

    #[test]
    fn vllm_injects_enable_thinking() {
        let mut req = simple_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let body = normalize("test", &req, ReasoningDialect::Vllm, HistoryReasoning::Auto, None, false).unwrap();
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn provider_extras_merged_last() {
        let mut req = simple_req("gpt-4o");
        req.provider_extras = Some(json!({"custom_key": "custom_val"}));
        let body = normalize("test", &req, ReasoningDialect::Passthrough, HistoryReasoning::Auto, None, false).unwrap();
        assert_eq!(body["custom_key"], "custom_val");
        assert!(body.get("provider_extras").is_none());
    }

    #[test]
    fn provider_extras_cannot_override_routectl_managed_keys() {
        // Regression for the round 5 finding: a malicious or
        // careless `provider_extras = {"messages": [...], "model":
        // "..."}` could replace the assembled messages or model
        // before the body went upstream. The Anthropic egress had
        // an allow-list; the openai-compat egress did not. Verify
        // routectl-managed keys are dropped here, while long-tail
        // provider knobs (`top_k`, `seed`, dialect-specific) still
        // pass through.
        let mut req = simple_req("gpt-4o");
        req.seed = Some(7);
        req.provider_extras = Some(json!({
            // canonical fields -- all MUST be dropped:
            "model": "evil-model",
            "messages": [{"role": "user", "content": "INJECTED"}],
            "stream": true,
            "tools": [],
            "max_tokens": 1,
            "seed": 99,
            "presence_penalty": 1.5,
            "frequency_penalty": 1.5,
            "response_format": {"type": "json_object"},
            // long-tail provider knobs -- MUST pass through:
            "top_k": 40,
            "service_tier": "premium",
            "safety_settings": {"hate": "high"},
        }));
        let body = normalize("test", &req, ReasoningDialect::Passthrough, HistoryReasoning::Auto, None, false).unwrap();
        // Canonical fields preserved from the request, NOT overridden.
        assert_eq!(body["model"], "gpt-4o");
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 1);
        assert_ne!(messages[0]["content"], "INJECTED");
        assert_ne!(body["max_tokens"], 1);
        // `seed` is canonical; the request had `seed = 7`, extras
        // tried to set it to 99 -- canonical wins.
        assert_eq!(body["seed"], 7);
        assert!(body.get("presence_penalty").is_none() || body["presence_penalty"] != 1.5);
        assert!(body.get("response_format").is_none());
        // Long-tail extras land verbatim.
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["service_tier"], "premium");
        assert_eq!(body["safety_settings"]["hate"], "high");
    }

    #[test]
    fn default_extras_overridden_by_provider_extras() {
        let req_mut = {
            let mut r = simple_req("gpt-4o");
            r.provider_extras = Some(json!({"key": "from_request"}));
            r
        };
        let defaults = json!({"key": "from_defaults"});
        let body = normalize(
            "test",
            &req_mut,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            Some(&defaults),
            false,
        )
        .unwrap();
        assert_eq!(body["key"], "from_request");
    }
}
