//! Shared `allowed_body_fields` filter for Bedrock adapters.
//!
//! Bedrock's strict-schema validator 400s any unrecognized field with
//! `"Extra inputs are not permitted"`. Two surfaces are exposed to
//! drift:
//!
//!   - Invoke: the top-level Anthropic Messages body. The Anthropic
//!     ingress's forward-compat sweep
//!     (`crates/routectl-cli/src/ingress/anthropic.rs::translate_request`)
//!     forwards quarterly-added Anthropic fields like
//!     `context_management`, `context_hint`, `speed`, `diagnostics`,
//!     `mcp_servers` into `provider_extras`, which the Anthropic-API
//!     egress then merges into the body. Bedrock rejects every entry
//!     not on its per-account allowlist.
//!   - Converse: the `additionalModelRequestFields` bag that AWS
//!     forwards verbatim to Anthropic. The same schema check applies.
//!
//! Filter shape mirrors `super::betas`:
//!
//! - `allowed_body_fields` is operator-supplied via
//!   `[bedrock] allowed_body_fields` TOML. routectl ships no const
//!   default; the empirical 2026-05-12 baseline is in
//!   `examples/bedrock.toml`.
//! - Startup validation in `routectl-router::factory` rejects an
//!   empty list when any `[providers.X]` has `kind = "bedrock"` and
//!   rejects a list missing the routectl-mandatory keys
//!   (`messages`, `anthropic_version`, `max_tokens`).
//! - Unknown keys drop at `tracing::debug!` (not WARN) -- the
//!   forward-compat sweep produces forwarded keys on every request,
//!   WARN would flood `routectl-warn.log`.
//! - On Invoke, structural keys routectl writes from canonical
//!   (`messages`, `system`, `tools`, ...) are kept by the allowlist;
//!   on Converse, those same keys must NEVER appear in the bag (they
//!   live in the AWS top-level Converse schema instead). The filter
//!   surface differs per `FilterContext`.

use serde_json::{Map, Value};

/// Which surface this filter is being applied to. Drives logging
/// context only -- the filter shape is identical (drop unknown).
#[derive(Debug, Clone, Copy)]
pub(super) enum FilterContext {
    /// Top-level Anthropic Messages body (Invoke).
    InvokeBody,
    /// `additionalModelRequestFields` bag (Converse).
    ConverseAdditionalFields,
}

impl FilterContext {
    fn as_str(self) -> &'static str {
        match self {
            FilterContext::InvokeBody => "invoke_body",
            FilterContext::ConverseAdditionalFields => "converse_additional_fields",
        }
    }
}

/// Filter `bag` in place: every key not on `allowed` drops with a
/// debug log line. Operates on the `Map` so callers can pass either
/// the top-level Invoke body (after `as_object_mut`) or the
/// Converse extras bag.
///
/// `allowed` is sourced from `[bedrock] allowed_body_fields` TOML.
/// routectl ships no const default; an empty list drops everything
/// (defense in depth -- startup validation rejects this state when
/// any provider has `kind = "bedrock"`).
pub(super) fn filter_bedrock_body_fields(
    provider_id: &str,
    bag: &mut Map<String, Value>,
    allowed: &[String],
    surface: FilterContext,
) {
    // Avoid an O(n*m) scan against `allowed.iter().any(...)` when the
    // list grows. The empirical baseline is ~16 entries today, but the
    // TOML side is operator-tunable so guard for larger lists too.
    let allowed_set: std::collections::HashSet<&str> = allowed.iter().map(String::as_str).collect();

    let to_drop: Vec<String> = bag
        .keys()
        .filter(|k| !allowed_set.contains(k.as_str()))
        .cloned()
        .collect();

    for key in to_drop {
        tracing::debug!(
            provider = %provider_id,
            field = %key,
            surface = surface.as_str(),
            "dropping body field not in operator-supplied [bedrock] allowed_body_fields"
        );
        bag.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn baseline() -> Vec<String> {
        vec![
            "anthropic_version".into(),
            "anthropic_beta".into(),
            "max_tokens".into(),
            "messages".into(),
            "system".into(),
            "tools".into(),
            "tool_choice".into(),
            "thinking".into(),
            "cache_control".into(),
            "metadata".into(),
        ]
    }

    #[test]
    fn drops_unknown_keys_keeps_known() {
        // Arrange
        let mut body = json!({
            "anthropic_version": "bedrock-2023-05-31",
            "messages": [],
            "max_tokens": 1024,
            "diagnostics": {"trace_id": "abc"},   // forward-compat sweep
            "mcp_servers": [],                    // forward-compat sweep
            "speed": "fast",                      // forward-compat sweep
        });
        let bag = body.as_object_mut().unwrap();

        // Act
        filter_bedrock_body_fields("bedrock:test", bag, &baseline(), FilterContext::InvokeBody);

        // Assert
        assert!(bag.contains_key("anthropic_version"));
        assert!(bag.contains_key("messages"));
        assert!(bag.contains_key("max_tokens"));
        assert!(!bag.contains_key("diagnostics"));
        assert!(!bag.contains_key("mcp_servers"));
        assert!(!bag.contains_key("speed"));
    }

    #[test]
    fn empty_allowlist_drops_everything() {
        // Defense in depth: if startup validation is somehow bypassed
        // and the list is empty at request time, EVERY field drops
        // (fails closed). The AWS upstream then returns a clear schema
        // error rather than silently forwarding unknown values.
        let mut body = json!({
            "anthropic_version": "bedrock-2023-05-31",
            "messages": [],
        });
        let bag = body.as_object_mut().unwrap();

        filter_bedrock_body_fields("bedrock:test", bag, &[], FilterContext::InvokeBody);

        assert!(bag.is_empty(), "expected all fields dropped, got {bag:?}");
    }

    #[test]
    fn empty_bag_is_a_noop() {
        let mut body = json!({});
        let bag = body.as_object_mut().unwrap();
        filter_bedrock_body_fields(
            "bedrock:test",
            bag,
            &baseline(),
            FilterContext::ConverseAdditionalFields,
        );
        assert!(bag.is_empty());
    }
}
