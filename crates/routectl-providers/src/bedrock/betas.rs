//! Shared `anthropic_beta` allowlist filter for Bedrock adapters.
//!
//! Lifted out of `invoke.rs` so the Converse adapter can apply the same
//! filter against `additionalModelRequestFields.anthropic_beta`. AWS
//! validates each entry of the body's `anthropic_beta` array
//! independently and 400s the entire request on the first unsupported
//! value -- there is no per-flag fallback. claude-code's TS SDK ships
//! up to ten betas via the `anthropic-beta` HTTP header that the
//! Anthropic ingress lifts into the body; only a subset are gated for
//! Bedrock distribution.
//!
//! Shape contract identical for both adapters:
//!
//! - The effective allowlist is the operator-supplied `allowed_betas`
//!   list from `[bedrock]` TOML. routectl ships no const default --
//!   AWS schema drift on the gated set is tracked by the operator,
//!   not by routectl releases. Startup validation in
//!   `routectl-router::factory` rejects an empty list when any
//!   provider has `kind = "bedrock"`, so the empty branch is
//!   unreachable in practice; the empty-fallback exists for defense
//!   in depth.
//! - Operator-supplied flags from `cfg.anthropic_beta`
//!   (`[providers.X] anthropic_beta`) pass through unconditionally
//!   because the operator typed them into TOML.
//! - Unknown values drop at `tracing::debug!` (not WARN) -- claude-code
//!   reliably ships a handful of unsupported flags per request, WARN
//!   would flood `routectl-warn.log`.
//! - When the filtered array is empty, the field is removed entirely
//!   so we don't send `anthropic_beta: []`.

use serde_json::{Map, Value};

/// Filter `bag["anthropic_beta"]` in place against the union of
/// `allowed_betas` and `cfg_betas` (the operator-asserted extension
/// hatch).
///
/// `bag` is the container that holds `anthropic_beta`:
/// - For Invoke: the top-level Anthropic Messages body.
/// - For Converse: the `additionalModelRequestFields` map.
///
/// `allowed_betas` is sourced from `[bedrock] allowed_betas` TOML.
/// routectl ships no const default -- the empirical 2026-05-12
/// baseline lives in `examples/bedrock.toml` for operators to copy.
pub(super) fn filter_bedrock_betas(
    provider_id: &str,
    bag: &mut Map<String, Value>,
    cfg_betas: &[String],
    allowed_betas: &[String],
) {
    let Some(arr) = bag
        .get("anthropic_beta")
        .and_then(|v| v.as_array())
        .cloned()
    else {
        return;
    };
    let in_allowlist = |flag: &str| -> bool { allowed_betas.iter().any(|s| s == flag) };
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
                "dropping beta flag not in operator-supplied [bedrock] allowed_betas"
            );
        }
    }
    if kept.is_empty() {
        bag.remove("anthropic_beta");
    } else {
        bag.insert("anthropic_beta".into(), Value::Array(kept));
    }
}
