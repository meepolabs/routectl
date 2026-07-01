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
//!   default. **Empty list = pass-through** (no filtering); the
//!   assembled body / bag is forwarded as-is. This is the discovery-
//!   mode default: operators bring up routectl, observe what fields
//!   are sent via `ROUTECTL_LOG=routectl_providers::bedrock=trace`,
//!   then populate `allowed_body_fields` with what they want to
//!   allow. The empirical 2026-05-12 baseline is in
//!   `examples/bedrock.toml`.
//! - When the allowlist is non-empty, unknown keys drop at
//!   `tracing::debug!` (not WARN) -- the forward-compat sweep
//!   produces forwarded keys on every request, WARN would flood
//!   `routectl-warn.log`.
//! - On Invoke, structural keys routectl writes from canonical
//!   (`messages`, `system`, `tools`, ...) must be on the allowlist
//!   when filtering is active or the assembled body is malformed; on
//!   Converse those keys live at the AWS top level instead and never
//!   appear in the bag. The filter surface differs per
//!   `FilterContext`.

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
    const fn as_str(self) -> &'static str {
        match self {
            Self::InvokeBody => "invoke_body",
            Self::ConverseAdditionalFields => "converse_additional_fields",
        }
    }
}

/// Filter `bag` in place: every key not on `allowed` drops with a
/// debug log line. Operates on the `Map` so callers can pass either
/// the top-level Invoke body (after `as_object_mut`) or the
/// Converse extras bag.
///
/// `allowed` is sourced from `[bedrock] allowed_body_fields` TOML.
/// **Empty list = pass-through**: no filtering, the bag is forwarded
/// as-is. routectl ships no const default; operators populate the
/// list after observing their actual traffic via trace logs.
pub(super) fn filter_bedrock_body_fields(
    provider_id: &str,
    bag: &mut Map<String, Value>,
    allowed: &[String],
    surface: FilterContext,
) {
    // Pass-through mode: empty operator allowlist means routectl is
    // not gating body fields on this surface. The operator is in
    // discovery mode (capturing observed keys via trace logs) or has
    // explicitly opted out of routectl-side filtering. Either way,
    // no keys drop here.
    if allowed.is_empty() {
        return;
    }

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
    fn empty_allowlist_is_pass_through() {
        // Discovery mode: with no operator-supplied list, routectl
        // forwards every field as-is so the operator can observe
        // actual traffic via trace logs and build the allowlist
        // from what they see.
        let mut body = json!({
            "anthropic_version": "bedrock-2023-05-31",
            "messages": [],
            "diagnostics": {"trace_id": "abc"},
            "mcp_servers": [],
        });
        let bag = body.as_object_mut().unwrap();
        let original_keys: Vec<String> = bag.keys().cloned().collect();

        filter_bedrock_body_fields("bedrock:test", bag, &[], FilterContext::InvokeBody);

        let after_keys: Vec<String> = bag.keys().cloned().collect();
        assert_eq!(
            after_keys, original_keys,
            "expected pass-through, got {bag:?}"
        );
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
