//! Shared `anthropic_beta` handling for Bedrock adapters.
//!
//! Both carriers hold the flags in an `anthropic_beta` array: the top-level
//! Invoke body, or the Converse `additionalModelRequestFields` bag. AWS
//! validates each entry independently and 400s the whole request on the
//! first unsupported value, with no per-flag fallback.
//!
//! Client flags reach the wire verbatim except for the request's
//! `routectl_internal.withheld_betas` (the client flags the caller decided
//! this lane must not send, filled by the router from its seed and learned
//! beta verdicts). Flags in the operator floor are never withheld: the
//! provider `anthropic_beta` config and every flag pinned through provider
//! or model `header_extras["anthropic-beta"]`. A caller that leaves the
//! withheld set empty withholds nothing. Each withheld drop logs at
//! `tracing::debug!` rather than WARN, since clients reliably ship a few
//! such flags per request. When nothing survives, the field is removed so
//! the upstream never sees `anthropic_beta: []`.

use serde_json::{Map, Value};

use routectl_core::{ChatRequest, sanitize_for_log};

use super::{BedrockApiShape, BedrockConfig};

/// The `anthropic-beta` flag gating `thinking.display: "updates"`.
const THINKING_DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";

/// The betas `fields` itself requires, in union order: `fields` is the
/// Invoke body or the Converse `additionalModelRequestFields` bag, as it
/// ships. Each is implied by a body field rather than opted into by the
/// client, so [`union_feature_implied_betas`] adds it after the withhold.
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
/// after the withhold and every strip that can change the implying fields,
/// so a withheld entry cannot drop a flag the shipped body needs. Idempotent: a present flag is neither duplicated nor
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

/// Remove `withheld_betas` entries not in `floor_betas` (from
/// [`operator_floor`]) from `bag["anthropic_beta"]`, leaving every other
/// entry (order, duplicates, non-strings) as it was.
///
/// Returns whether any withheld flag was dropped, so the caller can count
/// the request once on its own lane.
#[must_use = "the withheld-beta signal must be counted or deliberately discarded"]
pub(super) fn withhold_betas(
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
