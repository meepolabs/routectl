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
    // Lossy seams: Anthropic-canonical fields the OpenAI-compat wire
    // can't carry. Default mode warns + continues; strict mode 400s.
    check_dropped_anthropic_fields(id, req, strict_translation)?;

    let mut body =
        serde_json::to_value(req).map_err(|e| Error::normalize_request(id, e.to_string()))?;

    let obj = body
        .as_object_mut()
        .ok_or_else(|| Error::normalize_request(id, "serialized request is not an object"))?;

    // Lower canonical `system` (Anthropic-shape top-level field) into a
    // synthetic `role: "system"` message. Strict OpenAI-compat hosts
    // (NIM) reject the top-level field with `400 Validation:
    // Unsupported parameter(s): system`.
    if let Some(sys) = req.system.as_ref() {
        let text = sys.flatten();
        if !text.is_empty() {
            let messages = obj
                .entry("messages")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| {
                    Error::normalize_request(id, "serialized messages is not an array")
                })?;
            messages.insert(0, serde_json::json!({"role": "system", "content": text}));
        }
    }

    // Dialects that need `chat_template_kwargs` re-inject it themselves.
    obj.remove("system");
    obj.remove("reasoning");
    obj.remove("provider_extras");
    obj.remove("chat_template_kwargs");
    obj.remove("cache_control");
    obj.remove("anthropic_beta");

    let dyn_dialect = dialect.as_dyn();
    let dialect_strips = dyn_dialect.strip_history_reasoning();
    let resolved_strip = match history_reasoning {
        HistoryReasoning::Auto => dialect_strips,
        HistoryReasoning::Strip => true,
        HistoryReasoning::Preserve => false,
    };
    if resolved_strip && request_carries_reasoning(req) {
        warn!(
            provider = id,
            mode = ?history_reasoning,
            "openai-compat egress: assistant reasoning_content stripped from outgoing history. \
             Set `history_reasoning = \"preserve\"` on the provider if your upstream requires \
             echo-back (DeepSeek v4+, recent vLLM)."
        );
    }
    match history_reasoning {
        HistoryReasoning::Auto => {
            if dialect_strips {
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

    // default_extras then provider_extras (caller wins). Both gated
    // by the managed-key allow-list -- without this, a request body
    // of `provider_extras = {"messages":[...]}` could replace the
    // assembled messages.
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

/// Canonical `ChatRequest` field names that routectl owns on the wire.
/// `provider_extras` / `default_extras` cannot override these; long-tail
/// provider knobs (`top_k`, `service_tier`, dialect-specific
/// `chat_template_kwargs`, vendor-specific `safety_settings`) still
/// pass through. Keep in sync with `routectl_core::ChatRequest`.
fn is_routectl_managed_key(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "messages"
            | "system"
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
            | "seed"
            | "logprobs"
            | "top_logprobs"
            | "logit_bias"
            | "presence_penalty"
            | "frequency_penalty"
            | "response_format"
    )
}

/// Emit `tracing::warn!` for each Anthropic-only canonical field
/// dropped on the openai-compat wire. Default mode warns + returns
/// `Ok(())`; strict mode collects the findings and returns an
/// `Error::Validation` (HTTP 400) so the operator sees the loss
/// before the upstream does.
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
            if let ToolDef::Other(v) = t {
                // OpenAI function-shape tools (`{type:"function",
                // function:{...}}`) land here today (the OpenAI ingress
                // does not lift them, so canonical preserves the wire
                // shape). They serialize verbatim through the
                // openai-compat egress and reach the upstream
                // unchanged -- no warn needed. Only Anthropic builtin
                // / unknown shapes are genuinely dropped at the
                // upstream's deserialization step.
                if routectl_core::CustomTool::from_openai_function(v).is_some() {
                    continue;
                }
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

/// True when the canonical request carries assistant reasoning that
/// the strip path would silently drop. Drives the operator-visibility
/// warn so a DeepSeek-v4 / vLLM operator can see why their upstream
/// 400s without enabling debug logs.
fn request_carries_reasoning(req: &ChatRequest) -> bool {
    req.messages.iter().any(|m| {
        matches!(m.role, routectl_core::Role::Assistant)
            && (m.reasoning.as_deref().is_some_and(|s| !s.is_empty())
                || !m.reasoning_details.is_empty())
    })
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
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        // temperature preserved for non-reasoning models
        assert!(body.get("temperature").is_some());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("provider_extras").is_none());
    }

    #[test]
    fn openai_drops_sampling_for_o_series() {
        let req = simple_req("o3-mini");
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
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
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_drops_sampling_for_reasoner() {
        let req = simple_req("deepseek-reasoner");
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::DeepSeek,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
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
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::DeepSeek,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        for m in msgs {
            assert!(m.get("reasoning_content").is_none());
            assert!(m.get("reasoning").is_none());
            assert!(m.get("reasoning_details").is_none());
        }
    }

    /// DeepSeek v4 + history_reasoning="preserve" must echo the
    /// canonical `reasoning` string back to the wire as
    /// `reasoning_content`. Without this, multi-turn 400s with
    /// "reasoning_content in the thinking mode must be passed back
    /// to the API". This is the bug that motivated the whole branch.
    #[test]
    fn deepseek_preserve_renames_reasoning_to_reasoning_content() {
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
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::DeepSeek,
            HistoryReasoning::Preserve,
            None,
            false,
        )
        .unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert_eq!(
            assistant["reasoning_content"], "hidden chain",
            "preserve mode must rename reasoning -> reasoning_content"
        );
        assert!(
            assistant.get("reasoning").is_none(),
            "preserve mode must drop the legacy `reasoning` slot",
        );
    }

    /// Counterpart: explicit `Strip` must drop reasoning even on a
    /// dialect whose default would preserve. Future-proofing in case
    /// a dialect default flips.
    #[test]
    fn explicit_strip_overrides_dialect_default() {
        let mut req = simple_req("anything");
        req.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Text("a".into()),
            reasoning: Some("zap me".into()),
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenRouter, // default = passthrough/no-op
            HistoryReasoning::Strip,
            None,
            false,
        )
        .unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert!(assistant.get("reasoning").is_none());
        assert!(assistant.get("reasoning_content").is_none());
    }

    /// OpenRouter + preserve emits the typed `reasoning_details`
    /// array (Anthropic-aligned shape).
    #[test]
    fn openrouter_preserve_lifts_reasoning_to_typed_details_array() {
        let mut req = simple_req("anthropic/claude-haiku-4-5");
        req.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Text("ok".into()),
            reasoning: Some("trace".into()),
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenRouter,
            HistoryReasoning::Preserve,
            None,
            false,
        )
        .unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        let details = assistant["reasoning_details"].as_array().unwrap();
        assert_eq!(details[0]["text"], "trace");
        assert_eq!(details[0]["type"], "reasoning.text");
        assert!(assistant.get("reasoning").is_none());
    }

    #[test]
    fn vllm_injects_enable_thinking() {
        let mut req = simple_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn provider_extras_merged_last() {
        let mut req = simple_req("gpt-4o");
        req.provider_extras = Some(json!({"custom_key": "custom_val"}));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
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
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
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
    fn extras_cannot_re_inject_top_level_system() {
        // Regression: the `system` lowering above strips the top-level
        // `system` key. If extras (default or provider) could
        // reintroduce it, strict hosts (NIM) would 400 again. The
        // managed-key allow-list must include `system`.
        use routectl_core::SystemContent;
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Text("real system prompt".into()));
        req.provider_extras = Some(json!({
            "system": "INJECTED via provider_extras",
        }));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            Some(&json!({"system": "INJECTED via default_extras"})),
            false,
        )
        .unwrap();
        assert!(
            body.get("system").is_none(),
            "top-level `system` must remain stripped, got: {body}"
        );
        // The lowered system message is at messages[0].
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "real system prompt");
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

    #[test]
    fn system_text_lowered_to_role_system_message() {
        // Canonical `system: "you are a helpful bot"` MUST be lowered
        // into a synthetic `role: "system"` message at the start of
        // the messages array, and the top-level `system` field MUST
        // be removed. NIM strict-rejects unknown top-level fields with
        // `400 Validation: Unsupported parameter(s): system`.
        use routectl_core::SystemContent;
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Text("you are a helpful bot".into()));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("system").is_none(),
            "top-level `system` must be stripped, body: {body}"
        );
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "you are a helpful bot");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn system_blocks_flattened_to_role_system_message() {
        // SystemContent::Blocks (Anthropic-shape per-block array)
        // collapses to a single newline-joined string for the
        // synthetic role:"system" message, since OpenAI-compat has no
        // wire shape for typed system blocks.
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "first block".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "second block".into(),
                cache_control: None,
                citations: None,
            },
        ]));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(body.get("system").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "first block\nsecond block");
    }

    #[test]
    fn empty_system_does_not_inject_message() {
        // SystemContent::Text("") flattens to "" -- skip injection,
        // don't waste a wire slot on a noop system prompt.
        use routectl_core::SystemContent;
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Text(String::new()));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(body.get("system").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user", "no system message injected");
    }
}
