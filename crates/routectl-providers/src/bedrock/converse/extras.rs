//! `additionalModelRequestFields` bag assembly for AWS Converse.
//!
//! AWS Converse forwards this bag verbatim to the underlying model. For
//! Claude on Converse it carries the same fields routectl puts on a
//! direct Anthropic-API body: `thinking`, `anthropic_beta`, top-level
//! `cache_control`, `output_config`, plus operator-supplied extras.
//!
//! Routectl-managed keys are shielded from operator overrides via
//! `is_converse_managed_key` -- a misconfigured TOML cannot silently
//! replace the `thinking` block we computed.

use serde_json::{Map, Value};

use routectl_core::{is_canonical_request_key, ChatRequest};

use crate::anthropic_api::request::build_thinking;
use crate::anthropic_api::types::ThinkingConfig;
use crate::effort::clamp_effort_to_supported;

use super::super::betas::filter_bedrock_betas;
use super::super::BedrockConfig;
use super::types::ConverseToolChoice;

/// Build the `additionalModelRequestFields` bag. Returns None when no
/// fields land in the bag (avoids emitting `additionalModelRequestFields:
/// {}` upstream).
///
/// `anthropic_beta` is filtered against the same Bedrock allowlist as
/// the Invoke adapter (see `super::super::betas`). AWS validates the
/// flag set independently per-request whether the body shape is
/// Invoke (Anthropic-shape body) or Converse (`additionalModelRequestFields`).
/// The Invoke gotcha applies on both paths: a single unsupported flag
/// 400s the entire request.
///
/// The post-translation `tool_choice` reference is consumed solely by
/// `strip_thinking_when_tool_choice_forces_use` -- when toolChoice
/// resolves to `{any:{}}` or `{tool:{name}}`, Anthropic's extended-
/// thinking docs forbid pairing thinking with that tool_choice and
/// the Converse upstream 400s. The strip removes thinking from the
/// final bag while leaving toolChoice intact (the caller's intent to
/// force a tool is preserved).
pub(super) fn build_additional_fields(
    cfg: &BedrockConfig,
    req: &ChatRequest,
    tool_choice: Option<&ConverseToolChoice>,
) -> Option<Value> {
    let mut bag: Map<String, Value> = Map::new();

    insert_thinking(cfg, req, &mut bag);
    insert_anthropic_beta(cfg, req, &mut bag);
    insert_top_level_cache_control(req, &mut bag);
    insert_provider_extras(cfg, req, &mut bag);
    insert_operator_extras(cfg, &mut bag);

    // Filter anthropic_beta against the operator-supplied
    // `[bedrock] allowed_betas` list (routectl ships no const default).
    // Operator-supplied flags from cfg.anthropic_beta pass through
    // unconditionally; flags lifted from the inbound `anthropic-beta`
    // HTTP header that are not on the operator's accepted list drop
    // at DEBUG. The override hooks (`[bedrock] allowed_betas` global,
    // `[providers.X] anthropic_beta` per-provider floor) apply
    // identically to both Invoke and Converse paths. See
    // `super::super::betas` for the full contract.
    filter_bedrock_betas(&cfg.id, &mut bag, &cfg.anthropic_beta, &cfg.allowed_betas);

    // Warn when the operator's allowed_body_fields list would drop a
    // routectl-managed key that carries thinking or effort semantics.
    // The downstream filter logs at DEBUG for all drops; upgrading to
    // WARN here (before the filter runs) ensures operators can see the
    // loss without digging through debug logs.
    if !cfg.allowed_body_fields.is_empty() {
        for key in ["thinking", "output_config"] {
            if bag.contains_key(key) && !cfg.allowed_body_fields.iter().any(|k| k == key) {
                tracing::warn!(
                    provider = %cfg.id,
                    field = %key,
                    surface = "converse_additional_fields",
                    "allowed_body_fields omits routectl-managed field; it will be \
                     dropped and thinking/effort semantics will be lost. Add this \
                     field to [bedrock] allowed_body_fields to preserve Converse behavior."
                );
            }
        }
    }

    // Filter the bag itself against `[bedrock] allowed_body_fields`.
    // Anthropic-on-Bedrock rejects unknown body fields with HTTP 400
    // ("Extra inputs are not permitted"); for Converse those fields
    // ride in `additionalModelRequestFields` and AWS forwards them
    // verbatim to Anthropic which performs the schema check. Without
    // this filter, an Anthropic-ingress forward-compat sweep entry
    // like `mcp_servers` or `diagnostics` lands in the bag and 400s
    // every claude-code request to Converse.
    super::super::body_fields::filter_bedrock_body_fields(
        &cfg.id,
        &mut bag,
        &cfg.allowed_body_fields,
        super::super::body_fields::FilterContext::ConverseAdditionalFields,
    );

    // Final pass: Anthropic's extended-thinking docs forbid `thinking`
    // alongside a `tool_choice` that forces tool use. Strip thinking
    // from the bag when toolChoice has resolved to `{any:{}}` or
    // `{tool:{name}}`. Runs last so the check operates on the fully
    // composed bag (insert_thinking + provider_extras + operator_extras
    // + filtered for managed keys + body-field allowlist), matching
    // the wire body the request will actually carry.
    strip_thinking_when_tool_choice_forces_use(cfg, &mut bag, tool_choice);

    if bag.is_empty() {
        None
    } else {
        Some(Value::Object(bag))
    }
}

/// Layer canonical `req.provider_extras` (the Anthropic ingress's
/// forward-compat sweep destination) into the bag. Without this, top-
/// level Anthropic body fields routectl doesn't model
/// (`context_management`, `mcp_servers`, `container`, the legacy-
/// merged `output_config.format`, ...) silently disappear at the
/// Converse egress because they're stored in `provider_extras`, not
/// on canonical's typed surface.
///
/// Source: this helper only ever sees `req.provider_extras` -- the
/// Anthropic ingress's forward-compat sweep. Drops here are by design
/// (the swept key conflicts with a key routectl builds itself, e.g.
/// `thinking`) and were flooding `routectl-warn.log` on every
/// claude-code request. The drop log fires at DEBUG with neutral
/// phrasing. Operator-config extras flow through
/// `insert_operator_extras` below, which keeps the WARN-level
/// adversarial phrasing because that path IS an operator misconfig.
fn insert_provider_extras(cfg: &BedrockConfig, req: &ChatRequest, bag: &mut Map<String, Value>) {
    let Some(extras) = req.provider_extras.as_ref().and_then(|v| v.as_object()) else {
        return;
    };
    for (k, v) in extras {
        if is_converse_managed_key(k) {
            tracing::debug!(
                provider = %cfg.id,
                key = %k,
                "forward-compat extra would override routectl-managed key; \
                 dropped (Converse)"
            );
            continue;
        }
        // Operator extras (insert_operator_extras) run AFTER this and
        // use `entry().or_insert_with()`, so a provider-extra key
        // wins over an operator-extra at the same name -- which
        // matches the Anthropic egress precedence.
        bag.insert(k.clone(), v.clone());
    }
}

/// Reuse build_thinking from the Anthropic egress so the legacy vs
/// adaptive shape decision matches there. Adaptive thinking pairs with
/// `output_config.effort`; Converse exposes it via the same bag.
fn insert_thinking(cfg: &BedrockConfig, req: &ChatRequest, bag: &mut Map<String, Value>) {
    let Some(thinking) = build_thinking(req, cfg.adaptive_thinking.unwrap_or(false)) else {
        return;
    };
    let is_adaptive = matches!(thinking, ThinkingConfig::Adaptive);
    if let Ok(v) = serde_json::to_value(&thinking) {
        bag.insert("thinking".to_string(), v);
    }
    if is_adaptive {
        // Clamp effort against the operator-declared effort_levels cap
        // before inserting into the bag. Empty effort_levels = pass-through
        // (current Bedrock Converse default). Mirrors the Anthropic-API
        // egress behavior in derive_effort.
        let raw_effort = req
            .reasoning
            .as_ref()
            .and_then(|r| r.effort.clone())
            .unwrap_or_else(|| "medium".to_string());
        let effort = clamp_effort_to_supported(&raw_effort, &req.routectl_internal.effort_levels)
            .into_owned();
        bag.insert(
            "output_config".to_string(),
            serde_json::json!({"effort": effort}),
        );
    }
}

/// Merge anthropic_beta from canonical (header-lifted by the Anthropic
/// ingress) with any provider-config flags. Dedup; preserve first-seen
/// order so config-asserted flags win on dup.
fn insert_anthropic_beta(cfg: &BedrockConfig, req: &ChatRequest, bag: &mut Map<String, Value>) {
    let mut betas: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for s in cfg.anthropic_beta.iter().chain(req.anthropic_beta.iter()) {
        if seen.insert(s.clone()) {
            betas.push(Value::String(s.clone()));
        }
    }
    if !betas.is_empty() {
        bag.insert("anthropic_beta".to_string(), Value::Array(betas));
    }
}

fn insert_top_level_cache_control(req: &ChatRequest, bag: &mut Map<String, Value>) {
    if let Some(cc) = req.cache_control.as_ref() {
        if let Ok(v) = serde_json::to_value(cc) {
            bag.insert("cache_control".to_string(), v);
        }
    }
}

/// Layer operator-supplied extras (long-tail Anthropic knobs like
/// top_k, metadata, service_tier). Last in so they fill in keys
/// routectl didn't set, but lose to canonical-derived fields above when
/// keys clash -- avoids a misconfigured config silently overriding
/// anthropic_beta the caller intended to send. Routectl-managed keys
/// are dropped with a WARN to match the Invoke egress's
/// `is_bedrock_invoke_managed_key` policy.
fn insert_operator_extras(cfg: &BedrockConfig, bag: &mut Map<String, Value>) {
    let Some(extras) = cfg
        .additional_model_request_fields
        .as_ref()
        .and_then(|v| v.as_object())
    else {
        return;
    };
    for (k, v) in extras {
        if is_converse_managed_key(k) {
            tracing::warn!(
                provider = %cfg.id,
                key = %k,
                "additional_model_request_fields attempted to override \
                 routectl-managed key; dropped (Converse)"
            );
            continue;
        }
        bag.entry(k.clone()).or_insert_with(|| v.clone());
    }
}

/// Keys in `additionalModelRequestFields` that routectl manages. This
/// guards the bag level, not the top-level Converse request body. The
/// function delegates to the shared canonical list first (catching any
/// attempt to smuggle a ChatRequest-level key name into the bag, e.g.
/// `provider_extras = {"messages": [...]}` which after bag assembly
/// would forward a second `messages` value downstream) and then adds
/// the Converse-bag-specific keys that routectl writes from canonical
/// fields:
///
///   - `thinking`      -- built by `insert_thinking` from `req.reasoning`.
///   - `output_config` -- written by the adaptive-thinking path.
///
/// Note: `anthropic_beta` and `cache_control` are already covered by
/// `is_canonical_request_key` (they are `ChatRequest` wire fields) and
/// do not need to be listed here.
///
/// Converse top-level body fields (`inferenceConfig`, `toolConfig`,
/// `additionalModelResponseFieldPaths`) also appear here because an
/// operator TOML that sets `additional_model_request_fields.messages`
/// would produce a malformed Converse body if forwarded.
fn is_converse_managed_key(key: &str) -> bool {
    is_canonical_request_key(key)
        || matches!(
            key,
            // Converse-bag-level keys routectl writes from canonical fields.
            "thinking"
                | "output_config"
                // Converse top-level body fields -- should never appear in
                // the bag; if they do, drop them to avoid confusing AWS.
                | "inferenceConfig"
                | "toolConfig"
                | "additionalModelResponseFieldPaths"
        )
}

/// Anthropic's extended-thinking docs explicitly forbid pairing
/// `thinking` with a `tool_choice` value that forces tool use. Anthropic
/// on Bedrock honors the same constraint -- whether the thinking shape
/// rides in an Anthropic Messages body (Invoke) or in a Converse
/// `additionalModelRequestFields` bag, AWS forwards the bag verbatim to
/// Anthropic which 400s with "Thinking may not be enabled when
/// tool_choice forces tool use."
///
/// Strip `thinking` from the bag (NOT `toolChoice`; the caller's intent
/// to force a tool is preserved) when the post-translation Converse
/// `toolChoice` resolves to `Any` or `Tool`. `Auto` and absent
/// `toolChoice` do not trigger the strip.
fn strip_thinking_when_tool_choice_forces_use(
    cfg: &BedrockConfig,
    bag: &mut Map<String, Value>,
    tool_choice: Option<&ConverseToolChoice>,
) {
    let forces_use = matches!(
        tool_choice,
        Some(ConverseToolChoice::Any { .. }) | Some(ConverseToolChoice::Tool { .. })
    );
    if !forces_use {
        return;
    }
    if bag.remove("thinking").is_some() {
        let variant = match tool_choice {
            Some(ConverseToolChoice::Any { .. }) => "any",
            Some(ConverseToolChoice::Tool { .. }) => "tool",
            _ => unreachable!(
                "forces_use guarantees Any or Tool; update this match \
                 when adding a new forcing ConverseToolChoice variant"
            ),
        };
        tracing::debug!(
            provider = %cfg.id,
            tool_choice_type = %variant,
            "stripped thinking from Converse additionalModelRequestFields: \
             toolChoice forces tool use; Anthropic forbids the combo"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{ConverseSpecificTool, ConverseToolChoice, EmptyObject};
    use super::{build_additional_fields, is_converse_managed_key};
    use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};

    #[test]
    fn output_config_is_managed_key() {
        // Regression: adaptive-thinking writes output_config into the
        // bag; an operator-supplied value must not silently override.
        assert!(is_converse_managed_key("output_config"));
    }

    #[test]
    fn standard_managed_keys_are_recognized() {
        for k in [
            "messages",
            "system",
            "inferenceConfig",
            "toolConfig",
            "additionalModelResponseFieldPaths",
            "anthropic_beta",
            "thinking",
            "cache_control",
        ] {
            assert!(is_converse_managed_key(k), "expected {k:?} managed");
        }
    }

    #[test]
    fn non_managed_keys_pass_through() {
        for k in ["top_k", "metadata", "service_tier", "container"] {
            assert!(!is_converse_managed_key(k), "expected {k:?} NOT managed");
        }
    }

    // -----------------------------------------------------------------
    // toolChoice + thinking conflict resolution (Converse parallel)
    //
    // Anthropic-on-Converse honors the same constraint as Anthropic
    // direct: a `thinking` block in `additionalModelRequestFields`
    // alongside `toolChoice` set to `{any:{}}` or `{tool:{name}}` causes
    // AWS to forward to Anthropic which 400s. The strip removes thinking
    // from the bag (NOT toolChoice; the caller's intent to force a tool
    // is preserved).
    // -----------------------------------------------------------------

    /// Test config with `max_tokens > 1024` so legacy thinking is
    /// composed onto the bag, and a permissive `allowed_body_fields`
    /// list so the body-field filter doesn't drop `thinking` on its own.
    fn fake_cfg() -> BedrockConfig {
        BedrockConfig {
            id: "bedrock:test-converse".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-sonnet-4-5".into(),
            api_shape: BedrockApiShape::Converse,
            creds: BedrockCreds::BearerKey { key: "test".into() },
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            allowed_betas: Vec::new(),
            allowed_body_fields: Vec::new(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
        }
    }

    /// Helper: build a ChatRequest with reasoning enabled (-> thinking
    /// composition) at a `max_tokens` that fits the legacy floor.
    fn req_with_thinking() -> ChatRequest {
        ChatRequest {
            model: "anthropic.claude-sonnet-4-5".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: Some("medium".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        }
    }

    /// True when `thinking` is absent from the wire bag. A `None` bag
    /// (no fields at all) and a `Some(obj)` without a `thinking` key
    /// are both wire-equivalent: nothing thinking-related reaches AWS.
    fn bag_thinking_absent(bag: &Option<serde_json::Value>) -> bool {
        match bag {
            None => true,
            Some(v) => v
                .as_object()
                .map(|o| o.get("thinking").is_none())
                .unwrap_or(true),
        }
    }

    #[test]
    fn tool_choice_any_with_thinking_strips_thinking() {
        // Arrange
        let cfg = fake_cfg();
        let req = req_with_thinking();
        let tc = ConverseToolChoice::Any {
            any: EmptyObject {},
        };

        // Act
        let bag = build_additional_fields(&cfg, &req, Some(&tc));

        // Assert: thinking dropped. Because thinking was the only field
        // in the bag, the now-empty bag collapses to None -- either way
        // thinking is gone from the wire.
        assert!(
            bag_thinking_absent(&bag),
            "thinking must be stripped when toolChoice is Any, got: {bag:?}"
        );
    }

    #[test]
    fn tool_choice_tool_with_thinking_strips_thinking() {
        // Arrange: the Claude Code WebSearch shape that motivated the fix.
        let cfg = fake_cfg();
        let req = req_with_thinking();
        let tc = ConverseToolChoice::Tool {
            tool: ConverseSpecificTool {
                name: "web_search".into(),
            },
        };

        // Act
        let bag = build_additional_fields(&cfg, &req, Some(&tc));

        // Assert: thinking dropped (bag collapses to None when empty).
        assert!(
            bag_thinking_absent(&bag),
            "thinking must be stripped when toolChoice is Tool, got: {bag:?}"
        );
    }

    #[test]
    fn tool_choice_auto_with_thinking_keeps_thinking() {
        // Regression guard: Auto does not force tool use, so thinking
        // must survive in the bag.
        let cfg = fake_cfg();
        let req = req_with_thinking();
        let tc = ConverseToolChoice::Auto {
            auto: EmptyObject {},
        };

        let bag = build_additional_fields(&cfg, &req, Some(&tc)).expect("bag should be present");
        let bag = bag.as_object().expect("bag is an object");

        assert_eq!(
            bag.get("thinking").and_then(|v| v.get("type")),
            Some(&serde_json::Value::String("enabled".into())),
            "thinking must survive on toolChoice Auto, got: {bag:?}"
        );
    }

    #[test]
    fn no_tool_choice_with_thinking_keeps_thinking() {
        // Regression guard: absent toolChoice never triggers the strip.
        let cfg = fake_cfg();
        let req = req_with_thinking();

        let bag = build_additional_fields(&cfg, &req, None).expect("bag should be present");
        let bag = bag.as_object().expect("bag is an object");

        assert_eq!(
            bag.get("thinking").and_then(|v| v.get("type")),
            Some(&serde_json::Value::String("enabled".into())),
            "thinking must survive when toolChoice is absent, got: {bag:?}"
        );
    }

    #[test]
    fn tool_choice_any_without_thinking_no_op() {
        // Regression guard: when reasoning is absent (no thinking
        // composed), the strip is harmless and the bag is either None
        // or thinking-absent.
        let cfg = fake_cfg();
        let req = ChatRequest {
            model: "anthropic.claude-sonnet-4-5".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(2048),
            ..Default::default()
        };
        let tc = ConverseToolChoice::Any {
            any: EmptyObject {},
        };

        let bag = build_additional_fields(&cfg, &req, Some(&tc));

        assert!(
            bag_thinking_absent(&bag),
            "thinking must be absent when no reasoning was set, got: {bag:?}"
        );
    }
}
