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

use super::super::betas::{filter_bedrock_betas, operator_floor, union_feature_implied_betas};
use super::super::{BedrockApiShape, BedrockConfig};
use super::request::ClientFingerprintStripTally;
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
    fingerprint: &mut ClientFingerprintStripTally,
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
    let provider_actions = insert_provider_extras(cfg, req, &mut bag, fingerprint);
    let operator_actions = insert_operator_extras(cfg, &mut bag);
    // Once per REQUEST per class, never once per withheld key: a bag whose
    // extras collide on three managed keys is one policy-action event against
    // this lane's request-volume denominator. Flushed here, outside any
    // fallible body, so a request that later fails still counts.
    //
    // The fingerprint strip is deliberately NOT among these: it fires from
    // three surfaces of one request across two modules, so its tally is owned
    // by the whole-request translation and flushed there.
    provider_actions.flush();
    operator_actions.flush();

    // Filter anthropic_beta against the operator-supplied
    // `[bedrock] allowed_betas` list (no default) and drop the request's
    // router-withheld flags.
    // Operator-supplied flags from cfg.anthropic_beta pass through
    // unconditionally; flags lifted from the inbound `anthropic-beta`
    // HTTP header that are not on the operator's accepted list drop
    // at DEBUG. The override hooks (`[bedrock] allowed_betas` global,
    // `[providers.X] anthropic_beta` per-provider floor) apply
    // identically to both Invoke and Converse paths. See
    // `super::super::betas` for the full contract.
    filter_anthropic_beta(cfg, req, &mut bag);

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

    // The display-updates and structured-outputs betas are implied by the
    // final bag rather than opted into by the client, so they union here,
    // after every filter and strip that could change what ships: a
    // restrictive `allowed_betas` cannot drop them, and a bag whose
    // `output_config` or `thinking` was removed gains neither. An empty bag
    // implies nothing, so the union never fills one.
    union_feature_implied_betas(BedrockApiShape::Converse, &mut bag);
    let mut bag = Value::Object(bag);

    if let Some(obj) = bag.as_object_mut() {
        super::super::body_fields::drop_unrepresentable_body_fields(
            &cfg.id,
            obj,
            super::super::body_fields::FilterContext::ConverseAdditionalFields,
        );
        if obj.is_empty() {
            return None;
        }
    }
    Some(bag)
}

/// Per-request record of `additionalModelRequestFields` entries WITHHELD
/// during bag assembly by the PROVIDER-EXTRAS path (the Anthropic ingress's
/// forward-compat sweep). Lane: bedrock-converse. The class here is a policy
/// action rather than a drop -- the Converse bag could carry the value and the
/// upstream would accept it; routectl refuses to let a swept key override one
/// it manages. The field is a per-request FLAG rather than a key count: the
/// per-key log already names the offending key, and the `(lane, class)`
/// counters are per-REQUEST by contract.
///
/// Split from the operator path's tally deliberately. One shared type would
/// let a future edit set the same flag from both paths, and the caller flushes
/// both, so that class would count twice for one request -- breaking the
/// per-request contract with no test to catch it. Separate types make the
/// disjointness a compile error instead of a convention.
///
/// The client-fingerprint strip this path also performs is NOT a field here:
/// it fires from two further surfaces outside this module, so the whole-request
/// translation owns that tally and this path records into the one it is handed.
#[must_use = "the tally must be flushed once per request or the policy-action count is lost"]
#[derive(Default)]
struct ProviderExtrasPolicyActions {
    provider_extra_managed_key_conflict: bool,
}

impl ProviderExtrasPolicyActions {
    fn flush(&self) {
        if self.provider_extra_managed_key_conflict {
            crate::translation_drop_metrics::record_translation_policy_action(
                super::LANE,
                "provider_extra_managed_key_conflict",
            );
        }
    }
}

/// Per-request record of entries WITHHELD by the OPERATOR-EXTRAS path.
/// See [`ProviderExtrasPolicyActions`] for why the two paths carry separate
/// types rather than one shared tally.
#[must_use = "the tally must be flushed once per request or the policy-action count is lost"]
#[derive(Default)]
struct OperatorExtrasPolicyActions {
    operator_extra_managed_key_conflict: bool,
}

impl OperatorExtrasPolicyActions {
    fn flush(&self) {
        if self.operator_extra_managed_key_conflict {
            crate::translation_drop_metrics::record_translation_policy_action(
                super::LANE,
                "operator_extra_managed_key_conflict",
            );
        }
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
/// Anthropic ingress's forward-compat sweep. Withholding here is by design
/// (the swept key conflicts with a key routectl builds itself, e.g.
/// `thinking`) and was flooding `routectl-warn.log` on every
/// claude-code request. The refusal log fires at DEBUG with neutral
/// phrasing. Operator-config extras flow through
/// `insert_operator_extras` below, which keeps the WARN-level
/// adversarial phrasing because that path IS an operator misconfig.
///
/// Returns the request's policy-action flags for the caller to flush once.
fn insert_provider_extras(
    cfg: &BedrockConfig,
    req: &ChatRequest,
    bag: &mut Map<String, Value>,
    fingerprint: &mut ClientFingerprintStripTally,
) -> ProviderExtrasPolicyActions {
    let mut actions = ProviderExtrasPolicyActions::default();
    let Some(extras) = req.provider_extras.as_ref().and_then(|v| v.as_object()) else {
        return actions;
    };
    for (k, v) in extras {
        // A swept forward-compat key that collides with a field routectl
        // itself computes cannot be forwarded: the bag has exactly one slot
        // per key, and letting the client value win would silently replace
        // the `thinking` block (or the Converse body field) routectl derived
        // from canonical. Baked seed verdict per foundations sec 14.
        // TRANSLATION-DROP: policy-action class=provider_extra_managed_key_conflict test=provider_extra_managed_key_conflict_bumps_the_policy_action_counter_once
        if is_converse_managed_key(k) {
            actions.provider_extra_managed_key_conflict = true;
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
        // The drop is deliberate and NOT representability-driven: the wire
        // would carry the block fine, and routectl declines to send it.
        //
        // One of THREE surfaces of this lane that withhold the same
        // fingerprint; they share the request's tally so a client sending it
        // on more than one still counts as one policy action.
        // TRANSLATION-DROP: policy-action class=client_fingerprint_stripped test=client_fingerprint_strip_bumps_the_policy_action_counter_once
        if k == crate::bedrock::CLIENT_FINGERPRINT_METADATA_KEY {
            fingerprint.record();
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
    actions
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
    // `display` rides verbatim on both shapes and an absent one stays absent:
    // Converse accepted and honored the field on `enabled` and `adaptive`
    // when measured live, and the upstream default is model-dependent.
    if let Ok(v) = serde_json::to_value(&thinking) {
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

/// Apply the shared Bedrock beta filter to the bag and count a withheld
/// client flag once per request, not once per flag.
fn filter_anthropic_beta(cfg: &BedrockConfig, req: &ChatRequest, bag: &mut Map<String, Value>) {
    // The withheld set is dropped even in pass-through mode: the router put
    // each flag in it because its seed or a learned verdict says this lane's
    // upstream rejects it, so the upstream compels the loss.
    // TRANSLATION-DROP: lane=bedrock-converse class=anthropic_beta_rejected_by_bedrock test=bedrock_rejected_beta_withhold_bumps_the_drop_counter_once
    if filter_bedrock_betas(
        &cfg.id,
        bag,
        &cfg.anthropic_beta,
        &operator_floor(cfg, req),
        &req.routectl_internal.withheld_betas,
        &cfg.allowed_betas,
    ) {
        crate::translation_drop_metrics::record_translation_drop(
            super::LANE,
            "anthropic_beta_rejected_by_bedrock",
        );
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
/// are withheld with a WARN to match the Invoke egress's
/// `is_bedrock_invoke_managed_key` policy.
///
/// Returns the request's policy-action flags for the caller to flush once.
fn insert_operator_extras(
    cfg: &BedrockConfig,
    bag: &mut Map<String, Value>,
) -> OperatorExtrasPolicyActions {
    let mut actions = OperatorExtrasPolicyActions::default();
    let Some(extras) = cfg
        .additional_model_request_fields
        .as_ref()
        .and_then(|v| v.as_object())
    else {
        return actions;
    };
    for (k, v) in extras {
        // Same one-slot-per-key constraint as the client path above, with
        // the operator as the source: forwarding the configured value would
        // replace a field routectl derived from the canonical request. WARN
        // rather than DEBUG because this path IS an operator misconfig, not
        // a forward-compat sweep. Baked seed verdict per foundations sec 14.
        // TRANSLATION-DROP: policy-action class=operator_extra_managed_key_conflict test=operator_extra_managed_key_conflict_bumps_the_policy_action_counter_once
        if is_converse_managed_key(k) {
            actions.operator_extra_managed_key_conflict = true;
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
    actions
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
    use super::super::request::ClientFingerprintStripTally;
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
    // No serial guard: this test drives the helper in isolation and never
    // flushes, so it touches no counter key. A guard here would tell the next
    // reader it mutates one.
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
        // Drives the helper in isolation and never flushes, so the tally is
        // deliberately discarded and no serial guard is owed.
        let _ = super::insert_provider_extras(
            &cfg,
            &req,
            &mut bag,
            &mut ClientFingerprintStripTally::default(),
        );

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
        let bag = build_additional_fields(
            &cfg,
            &req,
            None,
            &mut ClientFingerprintStripTally::default(),
        )
        .expect("bag should be present");

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
    #[test]
    fn no_top_level_cache_control_does_not_warn() {
        // Arrange: req_with_thinking carries no cache_control; the control
        // carries one, proving the capture would see the WARN.
        let cfg = fake_cfg();
        let req = req_with_thinking();
        let mut control = req_with_thinking();
        control.cache_control = Some(routectl_core::cache_control::CacheControl::ephemeral_1h());

        // Act: the WARN carries no provider field, so the two assemblies run
        // in separate captures and the control's count is pinned alongside.
        let assemble = |r: &ChatRequest| {
            routectl_testkit::capture_events(|| {
                let _ = build_additional_fields(
                    &cfg,
                    r,
                    None,
                    &mut ClientFingerprintStripTally::default(),
                );
            })
        };
        let control_events = assemble(&control);
        let events = assemble(&req);

        // Assert
        assert_eq!(
            converse_cache_control_warns(&control_events),
            1,
            "the marked control must warn once; captured {control_events:?}"
        );
        assert_eq!(
            converse_cache_control_warns(&events),
            0,
            "WARN must not fire when no top-level cache_control is present; captured {events:?}"
        );
    }

    fn converse_cache_control_warns(events: &[routectl_testkit::CapturedEvent]) -> usize {
        events
            .iter()
            .filter(|e| {
                e.message
                    .contains("top-level cache_control on Converse path does not produce caching")
            })
            .count()
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
        // Drives the helper in isolation and never flushes, so the tally is
        // deliberately discarded and no serial guard is owed.
        let _ = super::insert_operator_extras(&cfg, &mut bag);

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
        let bag = build_additional_fields(
            &cfg,
            &req,
            Some(&tc),
            &mut ClientFingerprintStripTally::default(),
        );

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
        let bag = build_additional_fields(
            &cfg,
            &req,
            Some(&tc),
            &mut ClientFingerprintStripTally::default(),
        );

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

        let bag = build_additional_fields(
            &cfg,
            &req,
            Some(&tc),
            &mut ClientFingerprintStripTally::default(),
        )
        .expect("bag should be present");
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

        let bag = build_additional_fields(
            &cfg,
            &req,
            None,
            &mut ClientFingerprintStripTally::default(),
        )
        .expect("bag should be present");
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
        let bag = build_additional_fields(
            &cfg,
            &req,
            Some(&tc),
            &mut ClientFingerprintStripTally::default(),
        );

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
        let bag = build_additional_fields(
            &cfg,
            &req,
            None,
            &mut ClientFingerprintStripTally::default(),
        )
        .expect("bag should be present");
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

        let bag = build_additional_fields(
            &cfg,
            &req,
            None,
            &mut ClientFingerprintStripTally::default(),
        )
        .expect("bag should be present");
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

        let mut bag = None;
        let events = routectl_testkit::capture_events(|| {
            bag = build_additional_fields(
                &cfg,
                &req,
                None,
                &mut ClientFingerprintStripTally::default(),
            );
        });
        let bag = bag.expect("bag should be present");

        let fmt = bag["output_config"]["format"]
            .as_object()
            .expect("format must be an object");
        assert!(
            fmt.get("name").is_none() && fmt.get("strict").is_none(),
            "neither key may reach the Converse bag; got: {bag}"
        );
        let drops: Vec<_> = events
            .iter()
            .filter(|e| {
                e.field("event")
                    == Some(crate::anthropic_api::request::OUTPUT_FORMAT_KEY_DROP_EVENT)
            })
            .collect();
        assert_eq!(
            drops.len(),
            1,
            "expected exactly one dropped-format-key event; captured {events:?}"
        );
        assert_eq!(drops[0].level, tracing::Level::WARN, "{:?}", drops[0]);
        assert_eq!(drops[0].field("provider"), Some(cfg.id.as_str()));
        assert_eq!(drops[0].field("dropped_name"), Some("true"));
        assert_eq!(drops[0].field("dropped_strict"), Some("true"));
        // The drop event above is the capture-is-live control for this scan.
        assert!(
            events
                .iter()
                .all(|e| !format!("{e:?}").contains("secret-widget-name")),
            "the caller-controlled schema name must never be logged; captured {events:?}"
        );
    }

    // -- thinking.display forwarding -----------------------------------

    const THINKING_DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";

    /// Helper: `req_with_thinking()` with the display string an Anthropic
    /// ingress captured on the carrier (`None` = the caller sent no display).
    fn req_with_display(display: Option<&str>) -> ChatRequest {
        let mut req = req_with_thinking();
        req.routectl_internal.anthropic_thinking_display = display.map(str::to_string);
        req
    }

    fn cfg_with_shape(adaptive: bool) -> BedrockConfig {
        let mut cfg = fake_cfg();
        cfg.adaptive_thinking = Some(adaptive);
        cfg
    }

    fn bag_for(cfg: &BedrockConfig, req: &ChatRequest) -> serde_json::Value {
        build_additional_fields(cfg, req, None, &mut ClientFingerprintStripTally::default())
            .expect("thinking fills the bag")
    }

    fn bag_betas(bag: &serde_json::Value) -> Vec<String> {
        bag.get("anthropic_beta")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| b.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Both thinking shapes, each display state a caller can send: the bag's
    /// `thinking` object is byte-identical to the direct-Anthropic
    /// serialization of the same request, so a present display rides
    /// verbatim and an absent one stays absent (never defaulted). Converse
    /// accepted and honored `display` on both the `enabled` shape
    /// (sonnet-4-6, opus-4-6) and the `adaptive` shape when measured live.
    #[test]
    fn converse_bag_forwards_display_verbatim_on_both_thinking_shapes() {
        for adaptive in [false, true] {
            for display in [None, Some("summarized"), Some("omitted")] {
                // Arrange
                let cfg = cfg_with_shape(adaptive);
                let req = req_with_display(display);
                let direct = serde_json::to_value(
                    crate::anthropic_api::request::build_thinking(&req, adaptive)
                        .expect("thinking is active"),
                )
                .expect("thinking serializes");

                // Act
                let bag = bag_for(&cfg, &req);

                // Assert
                let expected_type = if adaptive { "adaptive" } else { "enabled" };
                assert_eq!(
                    bag["thinking"]["type"], expected_type,
                    "precondition: the {expected_type} shape must be the one under test"
                );
                assert_eq!(
                    bag["thinking"], direct,
                    "Converse must carry the same thinking bytes as the direct path \
                     (adaptive={adaptive}, display={display:?})"
                );
                assert_eq!(
                    bag["thinking"].get("display").and_then(|d| d.as_str()),
                    display,
                    "display must be forwarded verbatim, absent staying absent \
                     (adaptive={adaptive})"
                );
                assert!(
                    !bag_betas(&bag)
                        .iter()
                        .any(|b| b == THINKING_DISPLAY_UPDATES_BETA),
                    "only `updates` earns the display beta; got: {bag}"
                );
            }
        }
    }

    /// A display value this hub does not model forwards verbatim, without
    /// gaining the `updates` beta: upstream owns the vocabulary.
    #[test]
    fn converse_bag_forwards_unknown_display_without_a_beta() {
        // Arrange
        let cfg = fake_cfg();
        let req = req_with_display(Some("future-mode"));

        // Act
        let bag = bag_for(&cfg, &req);

        // Assert
        assert_eq!(bag["thinking"]["display"], "future-mode");
        assert!(
            bag.get("anthropic_beta").is_none(),
            "an unknown display must not manufacture any beta; got: {bag}"
        );
    }

    /// `updates` is gated behind its own beta. The flag is implied by the
    /// shipped bag rather than opted into by the client, so a restrictive
    /// allowlist that omits it cannot drop it.
    #[test]
    fn updates_display_unions_its_beta_past_a_restrictive_allowlist() {
        for adaptive in [false, true] {
            // Arrange
            let mut cfg = cfg_with_shape(adaptive);
            cfg.allowed_betas = vec!["context-1m-2025-08-07".into()];
            let req = req_with_display(Some("updates"));

            // Act
            let bag = bag_for(&cfg, &req);

            // Assert
            assert_eq!(bag["thinking"]["display"], "updates");
            assert_eq!(
                bag_betas(&bag),
                vec![THINKING_DISPLAY_UPDATES_BETA.to_string()],
                "the updates beta must ride exactly once (adaptive={adaptive}); got: {bag}"
            );
        }
    }

    /// A client that already sent the beta (header-lifted) gets it once,
    /// in its original position, not a second copy appended by the union.
    #[test]
    fn updates_display_beta_is_not_duplicated_when_the_client_sent_it() {
        // Arrange
        let cfg = fake_cfg();
        let mut req = req_with_display(Some("updates"));
        req.anthropic_beta = vec![
            THINKING_DISPLAY_UPDATES_BETA.to_string(),
            "context-1m-2025-08-07".to_string(),
        ];

        // Act
        let bag = bag_for(&cfg, &req);

        // Assert
        assert_eq!(
            bag_betas(&bag),
            vec![
                THINKING_DISPLAY_UPDATES_BETA.to_string(),
                "context-1m-2025-08-07".to_string(),
            ],
            "the client-sent flag must be neither duplicated nor reordered; got: {bag}"
        );
    }

    /// When a forcing tool_choice strips thinking, no display ships, so its
    /// beta must not ship either. The non-forcing control proves the same
    /// request otherwise gains the flag.
    #[test]
    fn updates_display_beta_is_withheld_when_forced_tool_choice_strips_thinking() {
        // Arrange
        let cfg = fake_cfg();
        let req = req_with_display(Some("updates"));
        let forced = ConverseToolChoice::Any {
            any: EmptyObject {},
        };

        // Act
        let stripped = build_additional_fields(
            &cfg,
            &req,
            Some(&forced),
            &mut ClientFingerprintStripTally::default(),
        );
        let control = bag_for(&cfg, &req);

        // Assert
        assert!(
            bag_thinking_absent(&stripped),
            "precondition: the forcing tool_choice strips thinking; got: {stripped:?}"
        );
        let stripped_betas = stripped.as_ref().map(bag_betas).unwrap_or_default();
        assert!(
            !stripped_betas
                .iter()
                .any(|b| b == THINKING_DISPLAY_UPDATES_BETA),
            "no thinking on the wire -> no display beta; got: {stripped:?}"
        );
        assert!(
            bag_betas(&control)
                .iter()
                .any(|b| b == THINKING_DISPLAY_UPDATES_BETA),
            "positive control: without the forcing choice the beta rides; got: {control}"
        );
    }

    // -----------------------------------------------------------------
    // The three `additionalModelRequestFields` policy-action classes and
    // their per-request counters. None is a wire-representability loss: the
    // bag could carry every one of these values, and routectl withholds
    // them, so they count on the policy-action counter and never on the drop
    // counter. Log capture uses `routectl_testkit::capture_events` because
    // the withheld KEY rides as a structured field, which a substring match
    // on rendered output cannot assert. Each test is serialized on its own
    // class guard -- the registry is process-global and this crate's runner
    // is threaded -- and the same guard is applied to every other test in
    // the crate that reaches the same arm incidentally.
    // -----------------------------------------------------------------

    fn bag_policy_action_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_policy_action_snapshot()
            .into_iter()
            .find(|e| e.lane == "bedrock-converse" && e.policy_class == class)
            .map_or(0, |e| e.action_count)
    }

    /// The drop counter for the same `(lane, class)` pair, read so each
    /// pinning test can prove the class left the DROP vocabulary rather than
    /// merely arriving in the policy one.
    fn bag_drop_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "bedrock-converse" && e.drop_class == class)
            .map_or(0, |e| e.drop_count)
    }

    /// The EMITTED WIRE VALUE for the bag: what actually rides in
    /// `additionalModelRequestFields`. A key can only be proven dropped
    /// against this, never against an intermediate typed view.
    /// Drive the bag builder the way the request translation does: with a
    /// fingerprint tally that is flushed once after the build. The flush is
    /// the translation's in production, so a test reading the strip counter
    /// back has to close the same loop or it reads a tally nobody emptied.
    fn emitted_bag(cfg: &BedrockConfig, req: &ChatRequest) -> serde_json::Value {
        let mut fingerprint = ClientFingerprintStripTally::default();
        let bag = build_additional_fields(cfg, req, None, &mut fingerprint)
            .unwrap_or(serde_json::Value::Null);
        super::super::request::flush_fingerprint_tally(&fingerprint);
        bag
    }

    /// NEGATIVE CONTROL. A forward-compat swept key colliding with a
    /// routectl-managed field cannot be forwarded (one slot per key), so it
    /// is withheld at DEBUG and the counter advances once for the request.
    #[test]
    #[serial_test::serial(bedrock_converse_provider_extra_managed_key_conflict)]
    fn provider_extra_managed_key_conflict_bumps_the_policy_action_counter_once() {
        // Arrange -- `thinking` is managed; `top_k` is representable.
        let before = bag_policy_action_count("provider_extra_managed_key_conflict");
        let drops_before = bag_drop_count("provider_extra_managed_key_conflict");
        let cfg = fake_cfg();
        let mut req = req_with_thinking();
        req.provider_extras = Some(serde_json::json!({
            "thinking": {"type": "sentinel-client-thinking"},
            "top_k": 40,
        }));

        // Act
        let mut bag = serde_json::Value::Null;
        let events = routectl_testkit::capture_events(|| {
            bag = emitted_bag(&cfg, &req);
        });
        let after = bag_policy_action_count("provider_extra_managed_key_conflict");
        let drops_after = bag_drop_count("provider_extra_managed_key_conflict");

        // Assert 1 -- the DEBUG fired, naming the key as a structured field.
        let event = events
            .iter()
            .find(|e| {
                e.message
                    .contains("forward-compat extra would override routectl-managed key")
            })
            .unwrap_or_else(|| panic!("the conflict must be logged; got: {events:?}"));
        assert_eq!(event.field("key"), Some("thinking"));

        // Assert 2 -- the client's colliding value is absent from the
        // EMITTED bag; the routectl-derived `thinking` is what shipped.
        assert!(
            !bag.to_string().contains("sentinel-client-thinking"),
            "the colliding client value must not reach the upstream; emitted bag: {bag}"
        );

        // Assert 3 -- positive control: the non-colliding sibling survived
        // in that same emitted bag.
        assert_eq!(
            bag.get("top_k"),
            Some(&serde_json::json!(40)),
            "a non-managed extra must survive the drop; emitted bag: {bag}"
        );

        assert_eq!(
            after - before,
            1,
            "the conflict counter must advance by exactly one for this request"
        );
        // Assert 4 -- the class left the DROP vocabulary: refusing an
        // override is not a representability loss, and the two vocabularies
        // must stay disjoint.
        assert_eq!(
            drops_after, drops_before,
            "a policy action must not also count as a translation drop"
        );
    }

    /// Two colliding keys in ONE request is one policy-action EVENT, not two.
    #[test]
    #[serial_test::serial(bedrock_converse_provider_extra_managed_key_conflict)]
    fn two_provider_extra_conflicts_bump_the_policy_action_counter_once() {
        // Arrange
        let before = bag_policy_action_count("provider_extra_managed_key_conflict");
        let cfg = fake_cfg();
        let mut req = req_with_thinking();
        req.provider_extras = Some(serde_json::json!({
            "thinking": {"type": "evil"},
            "toolConfig": {"tools": []},
        }));

        // Act
        let _ = emitted_bag(&cfg, &req);
        let after = bag_policy_action_count("provider_extra_managed_key_conflict");

        // Assert
        assert_eq!(
            after - before,
            1,
            "two colliding keys in one request is one policy-action event, not two"
        );
    }

    /// POSITIVE CONTROL: extras that collide with nothing advance no
    /// conflict counter and log no conflict at all.
    #[test]
    #[serial_test::serial(bedrock_converse_provider_extra_managed_key_conflict)]
    fn non_colliding_provider_extras_advance_no_conflict_counter() {
        // Arrange
        let before = bag_policy_action_count("provider_extra_managed_key_conflict");
        let cfg = fake_cfg();
        let mut req = req_with_thinking();
        req.provider_extras = Some(serde_json::json!({"top_k": 40}));

        // Act
        let mut bag = serde_json::Value::Null;
        let events = routectl_testkit::capture_events(|| {
            bag = emitted_bag(&cfg, &req);
        });
        let after = bag_policy_action_count("provider_extra_managed_key_conflict");

        // Assert
        assert_eq!(bag.get("top_k"), Some(&serde_json::json!(40)));
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("would override routectl-managed key")),
            "nothing collided, so nothing is owed a conflict log; got: {events:?}"
        );
        assert_eq!(after, before);
    }

    /// NEGATIVE CONTROL. The client fingerprint block is stripped on this
    /// seam because Bedrock is always a third-party upstream. The strip is
    /// deliberate and NOT representability-driven -- the wire would carry
    /// the block fine, and routectl declines to send it -- so it counts as a
    /// policy action, never as a translation drop.
    #[test]
    #[serial_test::serial(bedrock_converse_client_fingerprint_stripped)]
    fn client_fingerprint_strip_bumps_the_policy_action_counter_once() {
        // Arrange
        let before = bag_policy_action_count("client_fingerprint_stripped");
        let drops_before = bag_drop_count("client_fingerprint_stripped");
        let cfg = fake_cfg();
        let mut req = req_with_thinking();
        req.provider_extras = Some(serde_json::json!({
            "metadata": {"user_id": "sentinel-user", "account_uuid": "sentinel-account"},
            "top_k": 40,
        }));

        // Act
        let mut bag = serde_json::Value::Null;
        let events = routectl_testkit::capture_events(|| {
            bag = emitted_bag(&cfg, &req);
        });
        let after = bag_policy_action_count("client_fingerprint_stripped");
        let drops_after = bag_drop_count("client_fingerprint_stripped");

        // Assert 1 -- the strip was logged.
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("stripped client metadata fingerprint")),
            "the strip must be logged; got: {events:?}"
        );

        // Assert 2 -- neither fingerprint value reaches the upstream in ANY
        // form. Asserting against the serialized bag rather than a key check
        // is what catches a value riding inside a nested member.
        let serialized = bag.to_string();
        for leak in ["sentinel-user", "sentinel-account"] {
            assert!(
                !serialized.contains(leak),
                "`{leak}` must not reach the third-party upstream; emitted bag: {serialized}"
            );
        }

        // Assert 3 -- positive control: the non-fingerprint sibling survived.
        assert_eq!(
            bag.get("top_k"),
            Some(&serde_json::json!(40)),
            "a non-fingerprint extra must survive the strip; emitted bag: {bag}"
        );

        assert_eq!(
            after - before,
            1,
            "the fingerprint-strip counter must advance by exactly one"
        );
        // Assert 4 -- the class left the DROP vocabulary. This class fires
        // on nearly every request to this lane, which is exactly why it must
        // never be in the drop rate's numerator.
        assert_eq!(
            drops_after, drops_before,
            "a privacy strip must not also count as a translation drop"
        );
    }

    /// POSITIVE CONTROL: extras carrying no fingerprint block advance no
    /// strip counter.
    #[test]
    #[serial_test::serial(bedrock_converse_client_fingerprint_stripped)]
    fn extras_without_a_fingerprint_advance_no_strip_counter() {
        // Arrange
        let before = bag_policy_action_count("client_fingerprint_stripped");
        let cfg = fake_cfg();
        let mut req = req_with_thinking();
        req.provider_extras = Some(serde_json::json!({"top_k": 40}));

        // Act
        let bag = emitted_bag(&cfg, &req);
        let after = bag_policy_action_count("client_fingerprint_stripped");

        // Assert
        assert_eq!(bag.get("top_k"), Some(&serde_json::json!(40)));
        assert_eq!(after, before);
    }

    /// NEGATIVE CONTROL. The operator path carries the identical one-slot
    /// constraint, at WARN rather than DEBUG because this path IS a
    /// misconfiguration rather than a forward-compat sweep.
    #[test]
    #[serial_test::serial(bedrock_converse_operator_extra_managed_key_conflict)]
    fn operator_extra_managed_key_conflict_bumps_the_policy_action_counter_once() {
        // Arrange
        let before = bag_policy_action_count("operator_extra_managed_key_conflict");
        let drops_before = bag_drop_count("operator_extra_managed_key_conflict");
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(serde_json::json!({
            "thinking": {"type": "sentinel-operator-thinking"},
            "top_k": 40,
        }));
        let req = req_with_thinking();

        // Act
        let mut bag = serde_json::Value::Null;
        let events = routectl_testkit::capture_events(|| {
            bag = emitted_bag(&cfg, &req);
        });
        let after = bag_policy_action_count("operator_extra_managed_key_conflict");
        let drops_after = bag_drop_count("operator_extra_managed_key_conflict");

        // Assert 1 -- the WARN fired, naming the key as a structured field.
        let warn = events
            .iter()
            .find(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains(
                        "additional_model_request_fields attempted to override routectl-managed key",
                    )
            })
            .unwrap_or_else(|| panic!("the operator conflict must WARN; got: {events:?}"));
        assert_eq!(warn.field("key"), Some("thinking"));

        // Assert 2 -- the operator's colliding value is absent from the
        // emitted bag.
        assert!(
            !bag.to_string().contains("sentinel-operator-thinking"),
            "the colliding operator value must not reach the upstream; emitted bag: {bag}"
        );

        // Assert 3 -- positive control: the non-colliding operator key
        // survived in that same emitted bag.
        assert_eq!(
            bag.get("top_k"),
            Some(&serde_json::json!(40)),
            "a non-managed operator extra must survive; emitted bag: {bag}"
        );

        assert_eq!(after - before, 1);
        // Assert 4 -- the class left the DROP vocabulary.
        assert_eq!(
            drops_after, drops_before,
            "a refused override must not also count as a translation drop"
        );
    }

    /// POSITIVE CONTROL: operator extras that collide with nothing advance
    /// no counter and emit no conflict WARN.
    #[test]
    #[serial_test::serial(bedrock_converse_operator_extra_managed_key_conflict)]
    fn non_colliding_operator_extras_advance_no_conflict_counter() {
        // Arrange
        let before = bag_policy_action_count("operator_extra_managed_key_conflict");
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(serde_json::json!({"top_k": 40}));
        let req = req_with_thinking();

        // Act
        let mut bag = serde_json::Value::Null;
        let events = routectl_testkit::capture_events(|| {
            bag = emitted_bag(&cfg, &req);
        });
        let after = bag_policy_action_count("operator_extra_managed_key_conflict");

        // Assert
        assert_eq!(bag.get("top_k"), Some(&serde_json::json!(40)));
        assert!(
            !events.iter().any(|e| e
                .message
                .contains("attempted to override routectl-managed key")),
            "nothing collided, so no conflict WARN is owed; got: {events:?}"
        );
        assert_eq!(after, before);
    }
}
