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

use routectl_core::{ChatRequest, Error, Result, ToolDef, is_canonical_request_key};

use super::HistoryReasoning;
use super::dialect::ReasoningDialect;
use super::dialects::util::strip_history_reasoning;

/// `source` value passed to [`merge_extras`] for operator-config-supplied
/// extras (`[providers.X] payload_extras = {...}` -- renamed from the
/// pre-v0.6.0 `default_extras`). Drop here is adversarial -- the
/// operator asked routectl to send a managed key.
const DEFAULT_EXTRAS_SOURCE: &str = "payload_extras";

/// `source` value passed to [`merge_extras`] for canonical-swept
/// `req.provider_extras` (the Anthropic ingress's forward-compat sweep
/// destination). Drop here is by design -- the swept field has no
/// openai-compat equivalent and routectl already documented it gets
/// dropped; an Anthropic-only beta field showing up in extras is NOT
/// an operator misconfiguration.
const PROVIDER_EXTRAS_SOURCE: &str = "provider_extras";

pub fn normalize(
    id: &str,
    req: &ChatRequest,
    dialect: ReasoningDialect,
    history_reasoning: HistoryReasoning,
    payload_extras: Option<&Value>,
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
        // Drop the Claude Code billing/attribution block before flatten:
        // openai-compat is a third-party upstream and must not receive the
        // client fingerprint the block carries.
        let mut billing_dropped = false;
        let filtered = crate::system_filter::strip_billing_attribution(sys, &mut billing_dropped);
        if billing_dropped {
            warn!(
                provider = id,
                "openai-compat egress: Claude Code billing/attribution system block dropped",
            );
        }
        let text = filtered.as_ref().map(|s| s.flatten()).unwrap_or_default();
        if !text.is_empty() {
            let messages = obj
                .entry("messages")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| {
                    Error::normalize_request(id, "serialized messages is not an array")
                })?;
            // Direct callers (no ingress) may send both req.system AND
            // Role::System messages. Drop the existing role:system
            // entries so the lowered system prompt isn't duplicated.
            // The OpenAI ingress already does this lift at parse time;
            // doing it here protects library callers too.
            messages.retain(|m| {
                m.as_object()
                    .and_then(|o| o.get("role"))
                    .and_then(|r| r.as_str())
                    != Some("system")
            });
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

    dyn_dialect.apply_request(id, obj, req)?;

    // Lift Anthropic-shape request fields to OpenAI-wire shape. Runs
    // after dialect apply so dialect shaping is visible, and before
    // extras merge so operator-supplied provider_extras cannot clobber
    // the lift (managed-key allow-list already blocks "tools" et al,
    // but belt-and-suspenders ordering matters for clarity).
    //
    // history_reasoning processing MUST run AFTER wire_lift, not
    // before. The `thinking` step of wire_lift extracts Anthropic
    // `thinking` / `redacted_thinking` content blocks from assistant
    // messages into the message-envelope `reasoning_details` array.
    // The strip path needs to see those extracted entries to remove
    // them; the preserve path needs them to lower into
    // `reasoning_content` (deepseek/vllm) or keep as typed details
    // (openrouter). Running history_reasoning before wire_lift would
    // make both modes blind to anything cc echoed as content blocks.
    super::wire_lift::lift_all(id, obj, req, strict_translation)?;

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

    // payload_extras (provider-level) then req.provider_extras
    // (canonical, post-dispatch merge of provider + model). Both
    // gated by the managed-key allow-list -- without this, a request
    // body of `provider_extras = {"messages":[...]}` could replace
    // the assembled messages.
    if let Some(extras) = payload_extras {
        merge_extras(id, obj, extras, DEFAULT_EXTRAS_SOURCE);
    }
    if let Some(extras) = req.provider_extras.as_ref() {
        merge_extras(id, obj, extras, PROVIDER_EXTRAS_SOURCE);
    }

    Ok(body)
}

/// Shallow-merge `extras` into `obj` with a routectl-managed-keys
/// allow-list. Drop when an extras entry tries to override a key
/// routectl owns (model, messages, stream, tools, etc.). The
/// `source` arg names where the override came from and selects the
/// log level + wording:
///
///   - `DEFAULT_EXTRAS_SOURCE` -> WARN with the existing adversarial
///     phrasing. Operator asked routectl to send a managed key, so
///     the drop deserves their attention.
///   - `PROVIDER_EXTRAS_SOURCE` -> DEBUG with neutral phrasing. The
///     Anthropic ingress's forward-compat sweep is the source for
///     this path; a swept Anthropic-only beta field with no
///     openai-compat equivalent is BY DESIGN and was flooding
///     `routectl-warn.log` on every Anthropic-ingress request.
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
            if k == "metadata" {
                // Anthropic ingress stashes the full inbound `metadata`
                // object into provider_extras. Strict openai-compat hosts
                // (NIM, vLLM-strict, DeepSeek-direct) 400 with
                // `Unsupported parameter(s): metadata`. Warn so an
                // operator sees the drop; log the key only, never the
                // object's contents (may carry PII like user_id).
                tracing::warn!(
                    provider = id,
                    source = source,
                    "openai-compat egress: Anthropic `metadata` object dropped (not valid on OpenAI wire)"
                );
            } else if source == PROVIDER_EXTRAS_SOURCE {
                tracing::debug!(
                    provider = id,
                    source = source,
                    key = %k,
                    "forward-compat extra has no openai-compat equivalent; dropped"
                );
            } else {
                tracing::warn!(
                    provider = id,
                    source = source,
                    key = %k,
                    "extras attempted to override routectl-managed key; dropped"
                );
            }
            continue;
        }
        obj.insert(k.clone(), v.clone());
    }
}

/// OpenAI-compat wire keys that routectl owns. Delegates to the shared
/// canonical list (`routectl_core::is_canonical_request_key`) and adds
/// keys that are openai-compat-specific wire artefacts not on `ChatRequest`
/// but still managed by this egress:
///
///   - `output_config` -- Anthropic-shape nested structured-output config.
///     The openai-compat egress uses `response_format` for JSON mode; an
///     `output_config` key in `provider_extras` would land on OpenAI-shape
///     hosts that don't understand it, so it is blocked here. (The
///     Anthropic-API egress lets `output_config` through from provider_extras
///     because that is the intended forwarding path for Anthropic upstreams.)
///   - Anthropic-only beta fields swept into `provider_extras` by the
///     Anthropic ingress's forward-compat sweep
///     (`crates/routectl-cli/src/ingress/anthropic.rs::translate_request`).
///     These must NOT reach openai-compat upstreams: lenient hosts
///     (OpenRouter) silently ignore them; strict hosts (NIM, DeepSeek
///     API direct, vLLM with strict schema) 400 with `Unsupported
///     parameter(s): <field>`. Bug I (2026-05-18, NIM 400 on cc's
///     `context_management`) is the canonical case. Add new entries
///     here when a new Anthropic-only top-level beta field ships.
///     (Anthropic-API egress correctly forwards these via its own
///     `merge_provider_extras` path, untouched by this block-list.)
fn is_routectl_managed_key(key: &str) -> bool {
    is_canonical_request_key(key)
        || matches!(
            key,
            // Anthropic-only nested output config; not valid on OpenAI wire.
            "output_config"
            // Anthropic ingress sweeps the inbound `metadata` object into
            // provider_extras. Strict openai-compat hosts 400 on it; drop.
            | "metadata"
            // Anthropic-only quarterly-cadence beta fields. Forwarding
            // these to strict openai-compat upstreams 400s the request.
            | "context_management"
            | "context_hint"
            | "mcp_servers"
            | "container"
            | "inference_geo"
            | "speed"
            | "diagnostics"
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
                // wire_lift::tools::lift is the canonical write point for
                // the Anthropic-builtin warn; no second warn here.
            } else if let ToolDef::Custom(c) = t
                && c.cache_control.is_some()
            {
                warn!(
                    provider = id,
                    tool = %c.name,
                    "openai-compat egress: tool cache_control dropped (Anthropic-only)",
                );
                record(format!("tool `{}` cache_control", c.name));
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
///
/// Checks three shapes:
///   1. Flat `m.reasoning` string (DeepSeek / vLLM echo-back slot).
///   2. Typed `m.reasoning_details` array (lifted details).
///   3. `ContentPart::Known(KnownContentPart::Thinking)` content block --
///      the common Anthropic-ingress shape where the thinking trace rides
///      inside the parts array instead of the dedicated reasoning slots.
fn request_carries_reasoning(req: &ChatRequest) -> bool {
    use routectl_core::{ContentPart, KnownContentPart, MessageContent};
    req.messages.iter().any(|m| {
        matches!(m.role, routectl_core::Role::Assistant)
            && (m.reasoning.as_deref().is_some_and(|s| !s.is_empty())
                || !m.reasoning_details.is_empty()
                || matches!(&m.content, MessageContent::Parts(parts)
                    if parts.iter().any(|p| matches!(
                        p,
                        ContentPart::Known(KnownContentPart::Thinking { .. })
                    ))
                ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{
        ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, ReasoningConfig, Role,
    };
    use serde_json::json;

    fn simple_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message {
                refusal: None,
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

    /// v0.8: the openai-compat egress MUST NOT inject `max_tokens` when
    /// the caller omitted it. The good-translator principle: only
    /// inject where the upstream demands it (Anthropic-shape egresses).
    /// Pins the resolution-policy guarantee for #35.
    #[test]
    fn openai_compat_does_not_inject_max_tokens_when_caller_omitted() {
        let mut req = simple_req("gpt-4o");
        req.max_tokens = None;
        // Also set the router carrier to a non-zero value; the egress
        // must STILL leave it out of the wire body. routectl_internal
        // is Anthropic-shape territory only.
        req.routectl_internal.max_output_tokens = 8000;
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("max_tokens").is_none()
                || body.get("max_tokens") == Some(&serde_json::Value::Null),
            "openai-compat must leave max_tokens absent on omitted-by-caller requests; got: {body}"
        );
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

    /// Reasoning models (drops_sampling_params=true) reject `max_tokens`
    /// on real OpenAI; the OpenAI ingress renamed the inbound
    /// `max_completion_tokens` to canonical `max_tokens`, so the egress
    /// must rename it back for these models.
    #[test]
    fn openai_renames_max_tokens_to_max_completion_tokens_for_reasoning_model() {
        let req = simple_req("o3-mini"); // simple_req sets max_tokens = 512
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert!(
            body.get("max_tokens").is_none(),
            "reasoning model must not carry max_tokens; got: {body}"
        );
        assert_eq!(
            body["max_completion_tokens"], 512,
            "reasoning model must carry max_completion_tokens"
        );
    }

    /// Non-reasoning models keep `max_tokens` unchanged and must not gain
    /// a `max_completion_tokens` key.
    #[test]
    fn openai_keeps_max_tokens_for_non_reasoning_model() {
        let req = simple_req("gpt-4o"); // max_tokens = 512, not a reasoning model
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 512);
        assert!(
            body.get("max_completion_tokens").is_none(),
            "non-reasoning model must not gain max_completion_tokens; got: {body}"
        );
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
            refusal: None,
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
            refusal: None,
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
            refusal: None,
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
            refusal: None,
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
        // Stray routectl-managed keys in
        // `provider_extras = {"messages": [...], "model": "..."}`
        // could replace the assembled messages or model before the
        // body went upstream. The Anthropic egress had an allow-list;
        // the openai-compat egress did not. Verify routectl-managed
        // keys are dropped here, while long-tail
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

    /// Drop-semantics parity: a managed-key collision from
    /// `default_extras` (operator-config source) is dropped EXACTLY
    /// the way the provider_extras (forward-compat sweep source)
    /// drop is. The source-branching change in `merge_extras` only
    /// toggles the log level/wording; the dropped-key set is
    /// identical. This pins that the two source branches converge on
    /// the same body shape (the WARN vs DEBUG choice is reviewable in
    /// source; the drop is asserted here).
    #[test]
    fn default_extras_cannot_override_routectl_managed_keys() {
        let mut req = simple_req("gpt-4o");
        req.seed = Some(7);
        let defaults = json!({
            // canonical fields -- MUST be dropped:
            "model": "evil-model",
            "messages": [{"role": "user", "content": "INJECTED"}],
            "stream": true,
            "tools": [],
            "max_tokens": 1,
            "seed": 99,
            // long-tail provider knobs -- MUST pass through:
            "top_k": 40,
            "service_tier": "premium",
        });
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            Some(&defaults),
            false,
        )
        .unwrap();
        // Canonical fields preserved from the request, NOT overridden.
        assert_eq!(body["model"], "gpt-4o");
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 1);
        assert_ne!(messages[0]["content"], "INJECTED");
        assert_ne!(body["max_tokens"], 1);
        assert_eq!(body["seed"], 7);
        // Long-tail extras land verbatim.
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["service_tier"], "premium");
    }

    #[test]
    fn anthropic_metadata_is_dropped_from_openai_compat_body() {
        // The Anthropic ingress sweeps the inbound `metadata` object into
        // provider_extras. Strict openai-compat hosts (NIM, vLLM-strict,
        // DeepSeek-direct) 400 with `Unsupported parameter(s): metadata`,
        // so the egress must strip it from the upstream body.
        let mut req = simple_req("gpt-4o");
        req.provider_extras = Some(json!({
            "metadata": {"user_id": "abc123"},
            "top_k": 40,
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
        assert!(
            body.get("metadata").is_none(),
            "Anthropic metadata must not reach the openai-compat wire, got: {body}"
        );
        // A non-metadata provider_extras key still forwards.
        assert_eq!(body["top_k"], 40);
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

    #[test]
    fn direct_caller_with_both_req_system_and_role_system_dedupes() {
        // Direct callers (no ingress) might send both `req.system` AND
        // a Role::System message. The egress must drop the existing
        // role:system entries when injecting from req.system, so the
        // wire body doesn't carry two competing system prompts.
        use routectl_core::SystemContent;
        let mut req = simple_req("test-model");
        req.system = Some(SystemContent::Text("the real system prompt".into()));
        req.messages.insert(
            0,
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Text("legacy duplicate".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        let system_count = messages.iter().filter(|m| m["role"] == "system").count();
        assert_eq!(
            system_count, 1,
            "expected exactly one role:system message, got: {body}"
        );
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "the real system prompt");
        assert_ne!(
            messages[0]["content"], "legacy duplicate",
            "the lowered req.system must win, not the legacy Role::System message"
        );
    }

    /// `request_carries_reasoning` must return `true` when an assistant
    /// message has a `Thinking` content part (the Anthropic-ingress shape
    /// where the thinking trace rides in the parts array). Before this fix
    /// the check only looked at `m.reasoning` and `m.reasoning_details`,
    /// so the strip WARN was never emitted for this shape even though the
    /// downstream strip would remove the block.
    #[test]
    fn request_carries_reasoning_detects_thinking_content_part() {
        let mut req = simple_req("any-model");
        req.messages.push(Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Thinking {
                    thinking: "my trace".into(),
                    signature: None,
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "answer".into(),
                    cache_control: None,
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
        assert!(
            request_carries_reasoning(&req),
            "Thinking content part must be detected as carrying reasoning"
        );
    }

    /// Counterpart: a request with NO reasoning in any form must return `false`.
    #[test]
    fn request_carries_reasoning_false_when_no_reasoning() {
        let req = simple_req("any-model");
        assert!(!request_carries_reasoning(&req));
    }

    /// The Claude Code billing/attribution block (a system block whose
    /// text starts with `x-anthropic-billing-header:`) must be dropped
    /// before the openai-compat egress lowers `system` to a role:system
    /// message; a normal sibling block must survive and reach the wire.
    #[test]
    fn openai_compat_drops_billing_block_keeps_normal_block() {
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("gpt-4o");
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "x-anthropic-billing-header: v=1; fp=secret".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "you are helpful".into(),
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
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(
            messages[0]["content"], "you are helpful",
            "billing block must be dropped; only the normal block survives, got: {body}"
        );
    }

    /// A block whose text contains the billing prefix MID-string (not at
    /// the start) is a normal prompt and must be preserved.
    #[test]
    fn openai_compat_preserves_mid_string_billing_prefix() {
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("gpt-4o");
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "intro x-anthropic-billing-header: not at start".into(),
            cache_control: None,
            citations: None,
        }]));
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Passthrough,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(
            messages[0]["content"], "intro x-anthropic-billing-header: not at start",
            "a mid-string occurrence must NOT be treated as the billing block"
        );
    }

    /// Leading whitespace before the billing prefix still matches and the
    /// block is dropped (mirrors the reference trim-then-prefix check).
    #[test]
    fn openai_compat_drops_billing_block_with_leading_whitespace() {
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req("gpt-4o");
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "  \n\tx-anthropic-billing-header: v=1".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "real prompt".into(),
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
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[0]["content"], "real prompt",
            "leading-whitespace billing block must still be dropped, got: {body}"
        );
    }
}
