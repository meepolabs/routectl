//! Shared `anthropic_beta` allowlist and filter for Bedrock adapters.
//!
//! Lifted out of `invoke.rs` so the Converse adapter can apply the same
//! filter against `additionalModelRequestFields.anthropic_beta`. AWS
//! validates each entry of the body's `anthropic_beta` array
//! independently and 400s the entire request on the first unsupported
//! value -- there is no per-flag fallback. claude-code's TS SDK ships
//! up to ten betas via the `anthropic-beta` HTTP header that the
//! Anthropic ingress now lifts into the body; only a subset are gated
//! for Bedrock distribution.
//!
//! Shape contract identical for both adapters:
//!
//! - The effective allowlist is `allowlist_override` when `Some` (the
//!   global `[bedrock] anthropic_beta` TOML field), otherwise the const
//!   `BEDROCK_ACCEPTED_BETAS`.
//! - Operator-supplied flags from `cfg.anthropic_beta`
//!   (`[providers.X] anthropic_beta`) pass through unconditionally
//!   because the operator typed them into TOML.
//! - Unknown values drop at `tracing::debug!` (not WARN) -- claude-code
//!   reliably ships ~5 unsupported flags per request, WARN would flood.
//! - When the filtered array is empty, the field is removed entirely
//!   so we don't send `anthropic_beta: []`.

use serde_json::{Map, Value};

/// Anthropic beta flags Bedrock accepts as of the 2026-05-10 bisect.
/// See `issues.md::INV-6` for the full table and rationale per
/// rejected flag.
///
/// One global allowlist across all Anthropic-on-Bedrock models
/// (haiku-4-5, sonnet-4-6, opus-4-7 verified -- Bedrock has one
/// allowlist, not per-model). Same set applies to both Invoke and
/// Converse adapters: AWS validates `anthropic_beta` in the body
/// shape (Invoke) or in `additionalModelRequestFields` (Converse)
/// against the same per-account gating list.
pub(super) const BEDROCK_ACCEPTED_BETAS: &[&str] = &[
    "context-1m-2025-08-07",
    "claude-code-20250219",
    "interleaved-thinking-2025-05-14",
    "context-management-2025-06-27",
    "effort-2025-11-24",
    "fine-grained-tool-streaming-2025-05-14",
    "computer-use-2025-01-24",
    "computer-use-2024-10-22",
    "mcp-client-2025-04-04",
    "search-results-2025-06-09",
];

/// Filter `bag["anthropic_beta"]` in place against the union of the
/// effective allowlist and `cfg_betas` (the operator-asserted
/// extension hatch).
///
/// `bag` is the container that holds `anthropic_beta`:
/// - For Invoke: the top-level Anthropic Messages body.
/// - For Converse: the `additionalModelRequestFields` map.
///
/// `allowlist_override` is the per-deployment override sourced from
/// `[bedrock] anthropic_beta` TOML; when `None`, the routectl-shipped
/// `BEDROCK_ACCEPTED_BETAS` const applies.
pub(super) fn filter_bedrock_betas(
    provider_id: &str,
    bag: &mut Map<String, Value>,
    cfg_betas: &[String],
    allowlist_override: Option<&[String]>,
) {
    let Some(arr) = bag
        .get("anthropic_beta")
        .and_then(|v| v.as_array())
        .cloned()
    else {
        return;
    };
    let in_allowlist = |flag: &str| -> bool {
        match allowlist_override {
            Some(list) => list.iter().any(|s| s == flag),
            None => BEDROCK_ACCEPTED_BETAS.contains(&flag),
        }
    };
    let mut kept: Vec<Value> = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(flag) = item.as_str() else {
            // Non-string entries should not appear; preserve verbatim
            // so the upstream surfaces a clean validation error
            // instead of a silent drop.
            kept.push(item);
            continue;
        };
        let allowed = in_allowlist(flag);
        let in_cfg = cfg_betas.iter().any(|s| s == flag);
        // Dedup: if `kept` already has this flag, skip. The Anthropic
        // ingress already dedups header-vs-body merges; this catches
        // any direct caller that constructs duplicates explicitly.
        let already_kept = kept.iter().any(|v| v.as_str() == Some(flag));
        if already_kept {
            continue;
        }
        if allowed || in_cfg {
            kept.push(Value::String(flag.to_string()));
        } else {
            tracing::debug!(
                provider = %provider_id,
                flag = %flag,
                "dropping beta flag not in Bedrock accepted set"
            );
        }
    }
    if kept.is_empty() {
        bag.remove("anthropic_beta");
    } else {
        bag.insert("anthropic_beta".into(), Value::Array(kept));
    }
}
