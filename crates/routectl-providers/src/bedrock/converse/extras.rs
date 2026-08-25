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

use routectl_core::{ChatRequest, is_canonical_request_key, sanitize_for_log};

use crate::anthropic_api::request::DroppedFormatKeys;
use crate::anthropic_api::request::build_thinking;
use crate::anthropic_api::types::ThinkingConfig;
use crate::effort::clamp_effort_to_supported;

use super::super::BedrockConfig;
use super::super::betas::filter_bedrock_betas;
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

    // Bag-insertion ordering invariant (security-relevant): the client
    // path (`insert_provider_extras`) MUST run before the operator path
    // (`insert_operator_extras`), and `insert_operator_extras` uses
    // `entry().or_insert_with()` (first-writer-wins). Together these give
    // the intentional client-wins-over-operator precedence -- a client
    // `metadata` is skipped, and operator config fills only keys nothing
    // earlier set. INVARIANT: no insertion step added before
    // `insert_operator_extras` may write an operator-configurable key
    // (e.g. `metadata`, `top_k`), or the operator's config value would be
    // silently shadowed. Do not reorder these calls or relax the
    // `or_insert_with` semantics.
    insert_thinking(cfg, req, &mut bag);
    let dropped_format_keys = insert_response_format(req, &mut bag);
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
                    field = %sanitize_for_log(key),
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

    // Scrub the `output_config.format` keys Anthropic cannot represent from
    // the fully composed bag, so every path that can write the field is
    // covered rather than just the shared converter. One WARN for the request,
    // from whichever path supplied the keys.
    dropped_format_keys
        .merged(crate::anthropic_api::request::drop_unrepresentable_output_format_keys(&mut bag))
        .warn(&cfg.id);

    if bag.is_empty() {
        return None;
    }

    // Feature-triggered structured-outputs beta union. When the bag carries
    // `output_config.format`, AWS forwards it to Anthropic which gates it
    // behind `STRUCTURED_OUTPUTS_BETA`; a Converse request whose `[bedrock]
    // allowed_betas` omits that flag would otherwise ship the gated field
    // ungated and 400. The flag is a routectl-derived server requirement
    // implied by the shipped bag, not a client-opted beta, so it bypasses the
    // allowlist -- run AFTER `filter_bedrock_betas` and
    // `filter_bedrock_body_fields` (which could drop `output_config`
    // entirely). Reuses the same helper as the Bedrock-Invoke lane: the bag's
    // `output_config.format` + `anthropic_beta` shape matches the Anthropic
    // body it reads. Feature-triggered and idempotent -- no format means no
    // flag, an already-present flag is neither duplicated nor reordered.
    let mut bag = Value::Object(bag);
    crate::anthropic_api::request::apply_structured_outputs_beta_to_body(&mut bag);
    Some(bag)
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
                key = %sanitize_for_log(k),
                "forward-compat extra would override routectl-managed key; \
                 dropped (Converse)"
            );
            continue;
        }
        // Skip the Anthropic `metadata` block on the CLIENT path. It
        // carries the client fingerprint (`user_id`, `account_uuid`)
        // and Bedrock is always a third-party upstream. Operator-set
        // metadata flows through `insert_operator_extras` (not gated
        // here) -- that is the operator's deliberate choice. Shared key
        // with the Invoke seam via
        // `crate::bedrock::CLIENT_FINGERPRINT_METADATA_KEY`.
        if k == crate::bedrock::CLIENT_FINGERPRINT_METADATA_KEY {
            tracing::debug!(
                provider = %cfg.id,
                "stripped client metadata fingerprint from Converse \
                 additionalModelRequestFields (third-party upstream)"
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

/// Honor the canonical structured-output directive on the Converse bag by
/// mapping `req.response_format` (OpenAI-shape) onto Anthropic's
/// `output_config.format`, the shape AWS forwards verbatim to Claude. Uses
/// the same shared converter as the Anthropic-API egress so both Claude
/// seams emit the identical wire field. Merges into any `output_config`
/// `insert_thinking` already wrote (adaptive effort), preserving that
/// sibling; a caller-supplied `output_config.format` is left untouched.
///
/// Non-Claude Converse models do not honor `output_config.format`; the
/// admission-time capability gate (an operator `unsupported_features`
/// declaration) is what routes those away -- forwarding the inert bag key
/// here is harmless (AWS ignores unknown bag fields for such models).
fn insert_response_format(req: &ChatRequest, bag: &mut Map<String, Value>) -> DroppedFormatKeys {
    let Some(rf) = req.response_format.as_ref() else {
        return DroppedFormatKeys::default();
    };
    let Some((format, dropped)) =
        crate::anthropic_api::request::response_format_to_anthropic_format(rf)
    else {
        return DroppedFormatKeys::default();
    };
    crate::anthropic_api::request::set_output_config_format(bag, format);
    dropped
}

/// Reuse build_thinking from the Anthropic egress so the legacy vs
/// adaptive shape decision matches there. Adaptive thinking pairs with
/// `output_config.effort`; Converse exposes it via the same bag.
fn insert_thinking(cfg: &BedrockConfig, req: &ChatRequest, bag: &mut Map<String, Value>) {
    let Some(thinking) = build_thinking(req, cfg.adaptive_thinking.unwrap_or(false)) else {
        return;
    };
    let is_adaptive = matches!(thinking, ThinkingConfig::Adaptive { .. });
    if let Ok(mut v) = serde_json::to_value(&thinking) {
        // Bedrock Converse acceptance of `thinking.display` is
        // UNMEASURED -- no live probe has confirmed the field passes the
        // additionalModelRequestFields validator. Strip it rather than
        // risk a 400 on every thinking request that happens to carry an
        // explicit display. The strip is deliberately positioned on the
        // serialized value (not the enum) so nothing else in the bag can
        // pick the key up later.
        if let Some(stripped) = v.as_object_mut().and_then(|o| o.remove("display")) {
            tracing::warn!(
                stripped_display = ?stripped,
                "bedrock-converse: dropping thinking.display; the field's \
                 acceptance on Converse is unverified. Reasoning text will \
                 be returned per the model default."
            );
        }
        bag.insert("thinking".to_string(), v);
    }
    if is_adaptive {
        // Clamp effort against the operator-declared effort_levels cap
        // before inserting into the bag. Empty effort_levels = pass-through
        // (current Bedrock Converse default). Mirrors the Anthropic-API
        // egress behavior in derive_effort. A `None` clamp is reasoning-OFF
        // (`effort: "none"`); `build_thinking` already returns `Disabled`
        // for that (never Adaptive), so this branch is not reached with
        // "none" -- but the guard keeps the wire free of an orphaned
        // output_config.effort even if that invariant ever shifts.
        let raw_effort = req
            .reasoning
            .as_ref()
            .and_then(|r| r.effort.clone())
            .unwrap_or_else(|| "medium".to_string());
        if let Some(effort) =
            clamp_effort_to_supported(&raw_effort, &req.routectl_internal.effort_levels)
        {
            bag.insert(
                "output_config".to_string(),
                serde_json::json!({"effort": effort.into_owned()}),
            );
        }
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
        // AWS Converse only caches via per-block `cachePoint` blocks;
        // a top-level marker in additionalModelRequestFields is ignored
        // by the service. Forward it inert (so the bag mirrors the
        // Anthropic shape) but warn so a caller who set it knows the
        // caching they asked for will not happen on this path.
        tracing::warn!(
            "top-level cache_control on Converse path does not produce \
             caching (only per-block cachePoint does); forwarding inert \
             in additionalModelRequestFields"
        );
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
                key = %sanitize_for_log(k),
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
        Some(ConverseToolChoice::Any { .. } | ConverseToolChoice::Tool { .. })
    );
    if !forces_use {
        return;
    }
    if bag.remove("thinking").is_some() {
        // On the adaptive path, `output_config.effort` is only valid
        // alongside `thinking:{type:adaptive}`. Stripping thinking without
        // it leaves an orphan that Anthropic (via Converse) 400s. Drop the
        // effort sub-key; any orthogonal sibling (e.g. `format`) survives.
        crate::effort::drop_orphaned_output_config_effort(bag);
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
            tool_choice_type = %sanitize_for_log(variant),
            "stripped thinking from Converse additionalModelRequestFields: \
             toolChoice forces tool use; Anthropic forbids the combo"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{ConverseSpecificTool, ConverseToolChoice, EmptyObject};
    use super::{
        build_additional_fields, is_converse_managed_key,
        strip_thinking_when_tool_choice_forces_use,
    };
    use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};
    use tracing_test::traced_test;

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
        for k in ["top_k", "service_tier", "container"] {
            assert!(!is_converse_managed_key(k), "expected {k:?} NOT managed");
        }
    }

    /// The Anthropic `metadata` block carries client identity
    /// (`user_id`, `account_uuid`) and must NOT reach AWS via the
    /// CLIENT provider_extras path -- Bedrock is always a third-party
    /// upstream. `insert_provider_extras` skips it so it never lands in
    /// `additionalModelRequestFields`. (Operator-set metadata via config
    /// flows through `insert_operator_extras`, which is NOT gated here.)
    #[test]
    fn client_metadata_fingerprint_skipped_from_converse_bag() {
        use serde_json::{Map, json};
        // Arrange: client supplies a metadata fingerprint via
        // provider_extras and sets req.user (the canonical mirror).
        let cfg = fake_cfg();
        let mut req = req_with_thinking();
        req.user = Some("u-1".into());
        req.provider_extras = Some(json!({
            "metadata": {"user_id": "u-1", "account_uuid": "a-2"}
        }));

        // Act: drive insert_provider_extras directly so the assertion
        // targets the client path in isolation.
        let mut bag: Map<String, serde_json::Value> = Map::new();
        super::insert_provider_extras(&cfg, &req, &mut bag);

        // Assert: no metadata key, and no fingerprint substring.
        assert!(
            !bag.contains_key("metadata"),
            "client metadata fingerprint leaked into Converse bag: {bag:?}"
        );
        let serialized = serde_json::Value::Object(bag).to_string();
        assert!(
            !serialized.contains("u-1"),
            "user_id fingerprint leaked into Converse bag: {serialized}"
        );
        assert!(
            !serialized.contains("a-2"),
            "account_uuid fingerprint leaked into Converse bag: {serialized}"
        );
    }

    /// A top-level cache_control on the Converse path is forwarded inert
    /// into the bag (no wire change) but must emit a WARN so a caller who
    /// asked for caching knows it will not happen via this path.
    #[traced_test]
    #[test]
    fn top_level_cache_control_warns_and_forwards_inert() {
        // Arrange
        let cfg = fake_cfg();
        let mut req = req_with_thinking();
        req.cache_control = Some(routectl_core::cache_control::CacheControl::ephemeral_1h());

        // Act
        let bag = build_additional_fields(&cfg, &req, None).expect("bag should be present");

        // Assert: WARN fired, and the marker is still forwarded inert
        // (wire shape unchanged from the prior drop-silently behavior
        // except for the log).
        assert!(
            logs_contain("top-level cache_control on Converse path does not produce caching"),
            "expected a WARN when top-level cache_control reaches Converse"
        );
        assert!(
            bag.get("cache_control")
                .and_then(|v| v.as_object())
                .is_some_and(|o| !o.is_empty()),
            "top-level cache_control must still be forwarded inert as a \
             non-empty object: {bag:?}"
        );
    }

    /// No top-level cache_control means no WARN -- the common path stays
    /// quiet.
    #[traced_test]
    #[test]
    fn no_top_level_cache_control_does_not_warn() {
        // Arrange: req_with_thinking carries no cache_control.
        let cfg = fake_cfg();
        let req = req_with_thinking();

        // Act
        let _ = build_additional_fields(&cfg, &req, None);

        // Assert
        assert!(
            !logs_contain("top-level cache_control on Converse path does not produce caching"),
            "WARN must not fire when no top-level cache_control is present"
        );
    }

    /// Operator-deliberate `metadata` set via
    /// `additional_model_request_fields` is the operator's choice and
    /// survives into the bag -- the skip applies ONLY to the client
    /// provider_extras path, not `insert_operator_extras`.
    #[test]
    fn operator_metadata_survives_in_converse_bag() {
        use serde_json::{Map, json};
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            "metadata": {"trace": "operator-set"}
        }));

        let mut bag: Map<String, serde_json::Value> = Map::new();
        super::insert_operator_extras(&cfg, &mut bag);

        assert_eq!(
            bag.get("metadata").and_then(|m| m.get("trace")),
            Some(&serde_json::Value::String("operator-set".into())),
            "operator-deliberate metadata must survive: {bag:?}"
        );
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
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
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
            Some(v) => v.as_object().is_none_or(|o| o.get("thinking").is_none()),
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
    fn adaptive_forced_tool_choice_strips_thinking_and_output_config_effort() {
        // Arrange: adaptive thinking emits both `thinking:{type:adaptive}`
        // AND `output_config:{effort}` into the bag. A forcing toolChoice
        // must strip BOTH -- output_config.effort is only valid alongside
        // adaptive thinking, so an orphaned effort 400s on Anthropic.
        let mut cfg = fake_cfg();
        cfg.adaptive_thinking = Some(true);
        let req = req_with_thinking();
        let tc = ConverseToolChoice::Tool {
            tool: ConverseSpecificTool {
                name: "web_search".into(),
            },
        };

        // Act
        let bag = build_additional_fields(&cfg, &req, Some(&tc));

        // Assert: thinking gone AND the orphaned output_config.effort gone.
        assert!(
            bag_thinking_absent(&bag),
            "thinking must be stripped on adaptive forced tool_choice, got: {bag:?}"
        );
        let effort_present = bag
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("output_config"))
            .and_then(|oc| oc.get("effort"))
            .is_some();
        assert!(
            !effort_present,
            "output_config.effort must be stripped alongside thinking, got: {bag:?}"
        );
    }

    #[test]
    fn forced_tool_choice_strips_effort_but_preserves_sibling_format() {
        // Arrange: directly invoke the strip function on a bag carrying
        // adaptive thinking + output_config with both effort and a
        // structured-output format sibling. The strip must drop only
        // effort; format is orthogonal and must survive -- parallel to
        // the anthropic_api request.rs test
        // `forced_tool_choice_strips_effort_but_preserves_sibling_format`.
        use serde_json::{Map, json};

        let cfg = fake_cfg();
        let mut bag: Map<String, serde_json::Value> = Map::new();
        bag.insert("thinking".to_string(), json!({"type": "adaptive"}));
        bag.insert(
            "output_config".to_string(),
            json!({
                "effort": "high",
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "required": ["x"]}
                }
            }),
        );
        let tc = ConverseToolChoice::Tool {
            tool: ConverseSpecificTool {
                name: "web_search".into(),
            },
        };

        // Act
        strip_thinking_when_tool_choice_forces_use(&cfg, &mut bag, Some(&tc));

        // Assert: thinking gone, effort gone, format preserved.
        assert!(
            !bag.contains_key("thinking"),
            "thinking must be stripped; got: {bag:?}"
        );
        let oc = bag
            .get("output_config")
            .expect("output_config must survive when format sibling remains");
        assert!(
            oc.get("effort").is_none(),
            "effort must be stripped; got: {oc}"
        );
        assert_eq!(oc["format"]["type"], "json_schema");
        assert_eq!(oc["format"]["schema"]["required"][0], "x");
    }

    /// A request carrying `response_format` maps to `output_config.format`
    /// in the Converse bag; the structured-outputs beta it gates must ride
    /// along in `anthropic_beta` even when a NON-EMPTY `[bedrock]
    /// allowed_betas` omits the flag. The union is a routectl-derived server
    /// requirement implied by the shipped field, not a client-opted beta, so
    /// it bypasses the allowlist -- parallel to the Bedrock-Invoke test
    /// `structured_outputs_beta_survives_a_restrictive_bedrock_allowlist`.
    #[test]
    fn structured_outputs_beta_survives_restrictive_converse_allowlist() {
        use serde_json::json;

        // Arrange: restrictive allowlist that omits the flag, plus a
        // structured-output directive on the request.
        let flag = routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA;
        let mut cfg = fake_cfg();
        cfg.allowed_betas = vec!["context-1m-2025-08-07".into()];
        assert!(
            !cfg.allowed_betas.iter().any(|b| b == flag),
            "precondition: the allowlist must omit the structured-outputs flag"
        );
        assert!(
            !cfg.anthropic_beta.iter().any(|b| b == flag),
            "precondition: the operator floor must not supply the flag either"
        );

        let mut req = req_with_thinking();
        req.response_format = Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "widget", "schema": {"type": "object"}},
        }));

        // Act
        let bag = build_additional_fields(&cfg, &req, None).expect("bag should be present");
        let bag = bag.as_object().expect("bag is an object");

        // Assert: the directive reached the bag, and its gating beta rode
        // along despite the restrictive allowlist.
        assert!(
            bag.get("output_config")
                .and_then(|oc| oc.get("format"))
                .is_some(),
            "precondition: the structured-output directive must reach the bag; got: {bag:?}"
        );
        let betas: Vec<&str> = bag["anthropic_beta"]
            .as_array()
            .expect("the gating beta must be on the final bag")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(
            betas.contains(&flag),
            "output_config.format must never ship without its gating beta; got: {betas:?}"
        );
    }

    /// Regression guard: a bag with no `output_config.format` gains no
    /// structured-outputs beta -- the union is strictly feature-triggered.
    #[test]
    fn no_structured_output_format_gains_no_beta() {
        let flag = routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA;
        let cfg = fake_cfg();
        let req = req_with_thinking();

        let bag = build_additional_fields(&cfg, &req, None).expect("bag should be present");
        let bag = bag.as_object().expect("bag is an object");

        assert!(
            bag.get("output_config")
                .and_then(|oc| oc.get("format"))
                .is_none(),
            "precondition: no structured-output directive in this request; got: {bag:?}"
        );
        let carries_flag = bag
            .get("anthropic_beta")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| arr.iter().any(|b| b.as_str() == Some(flag)));
        assert!(
            !carries_flag,
            "no structured-output format must yield no structured-outputs beta; got: {bag:?}"
        );
    }

    /// The Converse seam emits the SAME single drop diagnostic as the
    /// Anthropic egress -- one WARN per bag assembly, naming which keys were
    /// omitted and never the caller's schema name.
    #[test]
    #[traced_test]
    fn bag_assembly_warns_once_for_the_dropped_format_keys() {
        use serde_json::json;

        let cfg = fake_cfg();
        let mut req = req_with_thinking();
        req.response_format = Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "secret-widget-name",
                "schema": {"type": "object"},
                "strict": true
            }
        }));

        let bag = build_additional_fields(&cfg, &req, None).expect("bag should be present");

        let fmt = bag["output_config"]["format"]
            .as_object()
            .expect("format must be an object");
        assert!(
            fmt.get("name").is_none() && fmt.get("strict").is_none(),
            "neither key may reach the Converse bag; got: {bag}"
        );
        logs_assert(|lines: &[&str]| {
            let matches: Vec<&&str> = lines
                .iter()
                .filter(|l| l.contains("output_config_format_keys_dropped"))
                .collect();
            let warns = matches.iter().filter(|l| l.contains("WARN")).count();
            if matches.len() == 1 && warns == 1 {
                return Ok(());
            }
            Err(format!(
                "expected exactly one WARN for the dropped format keys; got \
                 {} line(s), {warns} at WARN: {matches:?}",
                matches.len()
            ))
        });
        assert!(logs_contain("dropped_name=true"));
        assert!(logs_contain("dropped_strict=true"));
        assert!(
            !logs_contain("secret-widget-name"),
            "the caller-controlled schema name must never be logged"
        );
    }

    // -- thinking.display strip ----------------------------------------

    /// Helper: `req_with_thinking()` plus an explicit display request.
    fn req_with_thinking_display(exclude: bool) -> ChatRequest {
        let mut req = req_with_thinking();
        req.reasoning.as_mut().expect("reasoning set").exclude = Some(exclude);
        req
    }

    /// Negative: `display` never reaches the Converse bag, because its
    /// acceptance on additionalModelRequestFields is unverified.
    #[traced_test]
    #[test]
    fn converse_bag_thinking_never_carries_display() {
        for exclude in [true, false] {
            // Arrange
            let cfg = fake_cfg();
            let req = req_with_thinking_display(exclude);

            // Act
            let bag = build_additional_fields(&cfg, &req, None).expect("thinking fills the bag");

            // Assert
            let thinking = bag["thinking"]
                .as_object()
                .expect("thinking must be an object");
            assert!(
                thinking.get("display").is_none(),
                "display must be stripped from the Converse bag; got: {bag}"
            );
            assert!(
                thinking.get("type").is_some(),
                "positive control: the rest of the thinking shape survives"
            );
        }
        assert!(
            logs_contain("dropping thinking.display"),
            "the strip must WARN so an operator can see the discard"
        );
    }

    /// Positive control for the strip above: the SAME canonical input on
    /// the direct-Anthropic path DOES carry `display`, so the negative
    /// cannot pass vacuously via a build_thinking that never emits it.
    #[test]
    fn direct_anthropic_path_keeps_display_for_the_same_input() {
        let req = req_with_thinking_display(true);

        let thinking =
            crate::anthropic_api::request::build_thinking(&req, false).expect("thinking is active");
        let body = serde_json::to_value(&thinking).expect("thinking serializes");

        assert_eq!(
            body["display"], "omitted",
            "direct Anthropic keeps display; only Converse strips it"
        );
    }

    /// Helper: the shape an Anthropic ingress produces for
    /// `thinking.display: "updates"` -- the unmodeled string on the
    /// carrier plus the semantic boolean it maps to.
    fn req_with_updates_display_carrier() -> ChatRequest {
        let mut req = req_with_thinking_display(true);
        req.routectl_internal.anthropic_thinking_display = Some("updates".into());
        req
    }

    /// The carrier holds a display string this hub does not model, so it
    /// bypasses the canonical boolean entirely -- the Converse strip must
    /// still catch it.
    #[traced_test]
    #[test]
    fn converse_bag_thinking_strips_updates_display_carrier() {
        // Arrange
        let cfg = fake_cfg();
        let req = req_with_updates_display_carrier();

        // Act
        let bag = build_additional_fields(&cfg, &req, None).expect("thinking fills the bag");

        // Assert
        let thinking = bag["thinking"]
            .as_object()
            .expect("thinking must be an object");
        assert!(
            thinking.get("display").is_none(),
            "the carrier's display must be stripped from the Converse bag; got: {bag}"
        );
        assert!(
            thinking.get("type").is_some(),
            "positive control: the rest of the thinking shape survives"
        );
        assert!(
            logs_contain("dropping thinking.display"),
            "the strip must WARN so an operator can see the discard"
        );
    }

    /// Positive control for the strip above: the SAME canonical input on
    /// the direct-Anthropic path forwards the carrier string verbatim.
    #[test]
    fn direct_anthropic_path_keeps_updates_display_for_the_same_input() {
        let req = req_with_updates_display_carrier();

        let thinking =
            crate::anthropic_api::request::build_thinking(&req, false).expect("thinking is active");
        let body = serde_json::to_value(&thinking).expect("thinking serializes");

        assert_eq!(
            body["display"], "updates",
            "direct Anthropic forwards the carrier string; only Converse strips it"
        );
    }

    /// No display requested -> nothing to strip and no WARN.
    #[traced_test]
    #[test]
    fn converse_bag_without_requested_display_logs_no_strip_warn() {
        let cfg = fake_cfg();
        let req = req_with_thinking();

        let bag = build_additional_fields(&cfg, &req, None).expect("thinking fills the bag");

        assert!(bag["thinking"].get("display").is_none());
        assert!(
            !logs_contain("dropping thinking.display"),
            "no display requested -> no strip WARN"
        );
    }
}
