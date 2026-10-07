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
//!   list from `[bedrock]` TOML. Empty list (the default when `[bedrock]`
//!   is absent or `allowed_betas = []`) puts the filter in PASS-THROUGH
//!   mode -- apart from the request's withheld set below, no flags are
//!   dropped and the upstream sees what the ingress sent. This
//!   is the discovery-mode default: operators bring up routectl,
//!   observe which betas the SDK ships via
//!   `ROUTECTL_LOG=routectl_providers::bedrock=trace`, and populate
//!   `allowed_betas` with what they want to allow. See
//!   `examples/bedrock.toml` for the empirical 2026-05-12 baseline.
//! - The request's `routectl_internal.withheld_betas` (the client flags the
//!   caller decided this lane must not send) is withheld from client-lifted
//!   flags in BOTH modes, before the pass-through return, and even when
//!   `allowed_betas` names one of them. A caller that leaves the set empty
//!   withholds nothing here.
//! - Operator-supplied flags from `cfg.anthropic_beta`
//!   (`[providers.X] anthropic_beta`) pass through unconditionally
//!   because the operator typed them into TOML -- including a withheld
//!   flag. A flag pinned through provider or model
//!   `header_extras["anthropic-beta"]` (`routectl_internal.operator_betas`)
//!   is likewise exempt from the withhold, but stays subject to
//!   a non-empty `allowed_betas` as before.
//! - When the allowlist is non-empty and a flag is dropped, the drop
//!   logs at `tracing::debug!` (not WARN) -- claude-code reliably ships
//!   a handful of unsupported flags per request, WARN would flood
//!   `routectl-warn.log`.
//! - When the filtered array is empty, the field is removed entirely
//!   so we don't send `anthropic_beta: []`.

use serde_json::{Map, Value};

use routectl_core::{ChatRequest, sanitize_for_log};

use super::{BedrockApiShape, BedrockConfig};

/// The `anthropic-beta` flag gating `thinking.display: "updates"`.
const THINKING_DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";

/// The betas `fields` itself requires, in union order: `fields` is the
/// Invoke body or the Converse `additionalModelRequestFields` bag, as it
/// ships. Each is implied by a body field rather than opted into by the
/// client, so [`union_feature_implied_betas`] adds it after every allowlist
/// filter.
///
/// - `thinking.display: "updates"` gates on `thinking-display-updates`
///   (Converse only).
/// - `output_config.format` gains `STRUCTURED_OUTPUTS_BETA` on both
///   carriers. Kept as belt-and-braces: api.anthropic.com accepted the field
///   with and without the flag on one measured lane; whether AWS rejects an
///   ungated body is unmeasured.
pub(super) fn feature_implied_betas(
    shape: BedrockApiShape,
    fields: &Map<String, Value>,
) -> Vec<&'static str> {
    let mut implied = Vec::new();
    let display_updates = fields
        .get("thinking")
        .and_then(|t| t.get("display"))
        .and_then(Value::as_str)
        == Some("updates");
    if shape == BedrockApiShape::Converse && display_updates {
        implied.push(THINKING_DISPLAY_UPDATES_BETA);
    }
    if fields
        .get("output_config")
        .and_then(|oc| oc.get("format"))
        .is_some()
    {
        implied.push(routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA);
    }
    implied
}

/// Union [`feature_implied_betas`] into `fields["anthropic_beta"]`. Must run
/// after the beta and body-field filters and every strip that can change
/// the implying fields, so a restrictive allowlist cannot drop a flag the
/// shipped body needs. Idempotent: a present flag is neither duplicated nor
/// reordered.
pub(super) fn union_feature_implied_betas(shape: BedrockApiShape, fields: &mut Map<String, Value>) {
    let implied = feature_implied_betas(shape, fields);
    if implied.is_empty() {
        return;
    }
    let betas = fields
        .entry("anthropic_beta")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(arr) = betas.as_array_mut() else {
        return;
    };
    for flag in implied {
        if !arr.iter().any(|b| b.as_str() == Some(flag)) {
            arr.push(Value::from(flag));
        }
    }
}

/// The operator-asserted beta floor for `req` on this lane: the provider
/// `anthropic_beta` config plus every flag pinned through provider or model
/// `header_extras["anthropic-beta"]`. Never withheld or repaired.
pub(super) fn operator_floor(cfg: &BedrockConfig, req: &ChatRequest) -> Vec<String> {
    let mut floor = cfg.anthropic_beta.clone();
    for flag in &req.routectl_internal.operator_betas {
        if !floor.contains(flag) {
            floor.push(flag.clone());
        }
    }
    floor
}

/// Filter `bag["anthropic_beta"]` in place against the union of
/// `allowed_betas` and `cfg_betas` (the operator-asserted extension
/// hatch).
///
/// `bag` is the container that holds `anthropic_beta`:
/// - For Invoke: the top-level Anthropic Messages body.
/// - For Converse: the `additionalModelRequestFields` map.
///
/// `allowed_betas` is sourced from `[bedrock] allowed_betas` TOML.
/// **Empty list = pass-through**: apart from `withheld_betas`, the array is
/// forwarded to AWS as-is. The empirical 2026-05-12 baseline lives in
/// `examples/bedrock.toml` for operators to copy after observing their
/// actual traffic.
///
/// `withheld_betas` (the request's `routectl_internal.withheld_betas`) is
/// withheld in either mode unless the flag is in `pinned_betas` (from
/// [`operator_floor`]): the floor always wins. A pin does not bypass the
/// allowlist.
///
/// Returns whether any withheld flag was dropped, so the caller can count
/// the request once on its own lane.
#[must_use = "the withheld-beta signal must be counted or deliberately discarded"]
pub(super) fn filter_bedrock_betas(
    provider_id: &str,
    bag: &mut Map<String, Value>,
    cfg_betas: &[String],
    pinned_betas: &[String],
    withheld_betas: &[String],
    allowed_betas: &[String],
) -> bool {
    let withheld = withhold_betas(provider_id, bag, withheld_betas, pinned_betas);

    // Pass-through mode: empty operator allowlist means routectl is
    // not gating betas. The operator is in discovery mode (capturing
    // observed flags via trace logs) or has explicitly opted out of
    // routectl-side filtering. Either way, nothing else drops here.
    if allowed_betas.is_empty() {
        return withheld;
    }
    filter_against_allowlist(provider_id, bag, cfg_betas, allowed_betas);
    withheld
}

/// Remove `withheld_betas` entries not in `floor_betas`, leaving every other
/// entry (order, duplicates, non-strings) as it was so pass-through mode
/// stays verbatim apart from this set.
fn withhold_betas(
    provider_id: &str,
    bag: &mut Map<String, Value>,
    withheld_betas: &[String],
    floor_betas: &[String],
) -> bool {
    let is_withheld = |item: &Value| {
        item.as_str().is_some_and(|flag| {
            withheld_betas.iter().any(|w| w == flag) && !floor_betas.iter().any(|s| s == flag)
        })
    };
    let Some(arr) = bag.get("anthropic_beta").and_then(Value::as_array) else {
        return false;
    };
    if !arr.iter().any(is_withheld) {
        return false;
    }
    let kept: Vec<Value> = arr
        .iter()
        .filter(|item| {
            let withheld = is_withheld(item);
            if withheld {
                tracing::debug!(
                    provider = %provider_id,
                    flag = %sanitize_for_log(item.as_str().unwrap_or_default()),
                    "dropping beta flag withheld for this lane"
                );
            }
            !withheld
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        bag.remove("anthropic_beta");
    } else {
        bag.insert("anthropic_beta".into(), Value::Array(kept));
    }
    true
}

fn filter_against_allowlist(
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
                flag = %sanitize_for_log(flag),
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
