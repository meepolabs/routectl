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

use crate::beta_withhold::{BetaLane, body_implied_betas, split_withheld};

use super::{BedrockApiShape, BedrockConfig};

const fn beta_lane(shape: BedrockApiShape) -> BetaLane {
    match shape {
        BedrockApiShape::Invoke => BetaLane::BedrockInvoke,
        BedrockApiShape::Converse => BetaLane::BedrockConverse,
    }
}

/// Union [`body_implied_betas`] into `fields["anthropic_beta"]`. Must run
/// after the withhold and every strip that can change the implying fields,
/// so a withheld entry cannot drop a flag the shipped body needs. Appends
/// only the flags the array lacks, leaving every existing entry (order,
/// duplicates, non-strings) as it was; creates the array only when the body
/// implies a flag, and leaves a non-array value alone. Returns the implied
/// set, whether or not each flag was already present.
fn union_body_implied(
    shape: BedrockApiShape,
    fields: &mut Map<String, Value>,
) -> Vec<&'static str> {
    let implied = body_implied_betas(fields, beta_lane(shape));
    if implied.is_empty() {
        return implied;
    }
    let betas = fields
        .entry("anthropic_beta")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(arr) = betas.as_array_mut() else {
        return implied;
    };
    for flag in &implied {
        if !arr.iter().any(|b| b.as_str() == Some(flag)) {
            arr.push(Value::from(*flag));
        }
    }
    implied
}

/// Union the body-implied betas into `fields` (see [`union_body_implied`])
/// and record the request's beta floor and that implied set on its beta
/// assembly slot. `fields` must be the final carrier: the Invoke body or the
/// Converse bag as it ships.
pub(super) fn union_and_deposit_betas(
    cfg: &BedrockConfig,
    req: &ChatRequest,
    shape: BedrockApiShape,
    fields: &mut Map<String, Value>,
) {
    let implied = union_body_implied(shape, fields);
    let shipped = fields.get("anthropic_beta").and_then(Value::as_array);
    crate::beta_withhold::deposit_beta_assembly(
        req,
        operator_floor(cfg, req),
        implied.into_iter().map(str::to_string).collect(),
        |flag| shipped.is_some_and(|arr| arr.iter().any(|b| b.as_str() == Some(flag))),
    );
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
    let Some(arr) = bag.get("anthropic_beta").and_then(Value::as_array) else {
        return false;
    };
    let split = split_withheld(arr, Value::as_str, withheld_betas, floor_betas);
    if split.dropped.is_empty() {
        return false;
    }
    for item in &split.dropped {
        tracing::debug!(
            provider = %provider_id,
            flag = %sanitize_for_log(item.as_str().unwrap_or_default()),
            "dropping beta flag withheld for this lane"
        );
    }
    if split.kept.is_empty() {
        bag.remove("anthropic_beta");
    } else {
        bag.insert("anthropic_beta".into(), Value::Array(split.kept));
    }
    true
}
