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

use super::super::betas::filter_bedrock_betas;
use super::super::BedrockConfig;

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
pub(super) fn build_additional_fields(cfg: &BedrockConfig, req: &ChatRequest) -> Option<Value> {
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
/// Mirrors `crate::anthropic_api::request::merge_provider_extras`:
/// caller-supplied keys win EXCEPT for routectl-managed Converse keys
/// (`is_converse_managed_key`), which drop with a WARN.
fn insert_provider_extras(cfg: &BedrockConfig, req: &ChatRequest, bag: &mut Map<String, Value>) {
    let Some(extras) = req.provider_extras.as_ref().and_then(|v| v.as_object()) else {
        return;
    };
    for (k, v) in extras {
        if is_converse_managed_key(k) {
            tracing::warn!(
                provider = %cfg.id,
                key = %k,
                "provider_extras attempted to override routectl-managed key; \
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
        let effort = req
            .reasoning
            .as_ref()
            .and_then(|r| r.effort.clone())
            .unwrap_or_else(|| "medium".to_string());
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

#[cfg(test)]
mod tests {
    use super::is_converse_managed_key;

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
}
