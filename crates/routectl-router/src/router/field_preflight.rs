//! Canonical pre-flight planner shared by the complete, stream, and
//! count_tokens dispatch walks.
//!
//! One function computes, per fallback target, a per-target clone of the
//! ORIGINAL canonical request plus an immutable decision record -- before
//! provider translation or local token-count calculation. Every fallback
//! target plans from the original request, never from a sibling target's
//! clone, so a repair applied for one target can never leak onto another.
//!
//! # Why this is a rewrite, not a mutation
//!
//! [`Router::plan_field_preflight`] takes the original request by shared
//! reference and returns an owned clone: the compiler makes mutating the
//! caller's request impossible, rather than merely making a clone-then-check
//! test pass by inspection. Every transform runs against a SCRATCH clone and
//! is adopted only once it reports success, so a transform that removes
//! nothing -- or removes something unexpected -- returns a request
//! byte-equivalent to the original rather than a half-mutated one.
//!
//! # Relationship to the reactive repair
//!
//! [`super::field_repair`] admits and settles the REACTIVE, post-rejection
//! L0 repair: it fires only after an upstream names a field, and its
//! admission explicitly refuses to act when a verdict is already ACTING
//! (see [`Router::plan_field_carry`](super::field_repair)). This module
//! closes that gap from the other side: when a verdict is ACTING and
//! pre-flight ELIGIBLE (see
//! [`crate::field_verdict::FieldVerdictRegistry::preflight_eligible`]), the
//! same closed-table transform is applied BEFORE dispatch, so the upstream
//! never sees the rejected field at all. The two paths never compete for
//! the same budget or the same guard: pre-flight spends no repair-budget
//! draw and claims no single-flight slot, because it acts on a verdict that
//! is already resident and settled, not one this attempt is establishing.
//!
//! Every refusal the reactive admission applies for ATTRIBUTION reasons is
//! mirrored here, and for the same reason rather than by analogy: a target
//! whose rejection this stage could not have attributed is a target whose
//! resident verdict this stage must not act on either. That covers the
//! operator `force_supported` mask, the local-hop suppression, and any lane
//! carrying no attributable Anthropic API base URL (which is how a Bedrock
//! Mantle entry is refused -- it reports none).
//!
//! # Fail-open discipline
//!
//! An unknown transform, a stale canary incarnation, a request that does not
//! carry the grounded field, or any ambiguity in between resolves to a fresh
//! clone of the UNCHANGED original request plus a bounded, closed-set
//! diagnostic -- the same closed-set-token discipline [`super::field_repair`]
//! uses for its own WARN, so a fail-open decision can never carry upstream
//! bytes.
//!
//! # Extension seam for a second transform row
//!
//! The closed table (`super::field_repair`'s `FIELD_REPAIRS`) has exactly
//! ONE row in this build, so this planner deliberately plans the single
//! present row rather than iterating a set. That is a scope boundary, not an
//! oversight: applying several transforms in one plan is the
//! prefix-impacting transform's problem, and it needs the target opt-in and
//! quorum this stage does not implement. The seam is kept clean for it --
//! `first_present_row` is the shared scan, the transform is applied through
//! a scratch clone that a loop can reuse verbatim, and the decision record
//! is already a per-decision value rather than a per-request singleton. A
//! change adding a second row must revisit exactly this function.

use routectl_core::{ChatRequest, sanitize_for_log};

use super::class_observe::DispatchSurface;
use super::field_repair::{ANTHROPIC_API_KIND, FieldSurface, first_present_row};
use super::repair_budget::RepairBudget;
use super::{DispatchTarget, FieldPreflight, Router};

/// Reason token: no closed-table field is present in the request at all, so
/// there is nothing for a pre-flight rewrite to act on.
pub(super) const FIELD_PREFLIGHT_NO_GROUNDED_FIELD: &str = "no_grounded_field";

/// Reason token: the capability learning kill switch is off, the target is
/// not on the one lane this stage acts on, or the target authenticates with
/// a forwarded client credential -- the same exclusions the reactive
/// admission applies, checked here before any verdict is read.
pub(super) const FIELD_PREFLIGHT_UNSUPPORTED_LANE: &str = "unsupported_lane";

/// Reason token: the target carries no attributable Anthropic API base URL,
/// or the URL it carries names a local hop. A Bedrock Mantle entry reports
/// no such URL and is refused here.
pub(super) const FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET: &str = "unattributable_target";

/// Reason token: an operator `force_supported` override masks this field's
/// capability cell for this target, so the operator has said to send the
/// field regardless of what was learned.
pub(super) const FIELD_PREFLIGHT_MASKED_BY_OVERRIDE: &str = "masked_by_override";

/// Reason token: no identity could be minted for the grounded field on this
/// target (malformed path, or a lane whose normalization would rewrite the
/// minted key).
pub(super) const FIELD_PREFLIGHT_NO_IDENTITY: &str = "no_identity";

/// Reason token: a verdict exists but is not both ACTING and pre-flight
/// eligible -- covers absent, lapsed, an acting verdict with zero
/// acknowledged confirmations for its current incarnation, and a verdict
/// whose state moved under the eligibility read, since none of those give a
/// pre-flight rewrite grounds to act.
pub(super) const FIELD_PREFLIGHT_NOT_ELIGIBLE: &str = "not_eligible";

/// Reason token: the transform was authorized but removed nothing, or
/// removed something the presence check did not predict. Fails open to a
/// byte-equivalent original.
pub(super) const FIELD_PREFLIGHT_AMBIGUOUS_MUTATION: &str = "ambiguous_mutation";

/// Action token: the pre-flight planner dropped the mapped envelope field
/// from the per-target request before first dispatch.
pub(super) const FIELD_PREFLIGHT_ACTION_DROP: &str = "field_preflight_drop";

impl Router {
    /// Plan the per-target request for `target`, starting from
    /// `original_req` -- the canonical request as the client sent it,
    /// upstream of any per-attempt overlay or strip this walk has already
    /// applied for a PRIOR target in the same chain.
    ///
    /// Returns a fresh clone plus an immutable decision record. The clone
    /// carries the pre-flight rewrite only when a resident verdict is
    /// ACTING and pre-flight eligible for the one grounded field this
    /// request carries; every other case returns a clone of the original,
    /// unchanged.
    ///
    /// `surface` and `budget` are threaded through deliberately: `surface`
    /// distinguishes complete/stream/count_tokens for the canary cadence a
    /// later change wires (only `Complete` will ever decrement or claim
    /// one), and `budget` is the same request-scoped reactive-repair
    /// ceiling the caller's chain loop already threads. Envelope pre-flight
    /// spends neither today -- it acts on a verdict that is already
    /// resident and settled, not one this attempt is establishing, so it
    /// claims no repair-budget draw and no canary slot. Both parameters
    /// exist so a later prefix-impacting transform can consult them without
    /// a signature change.
    pub(super) fn plan_field_preflight(
        &self,
        original_req: &ChatRequest,
        target: &DispatchTarget,
        _surface: DispatchSurface,
        _budget: &RepairBudget,
    ) -> (ChatRequest, FieldPreflight) {
        // Every refusal below returns a FRESH clone of the original rather
        // than a partially-planned one, so no arm can hand back bytes the
        // client did not send.
        let unchanged = |field_path, reason| {
            (
                original_req.clone(),
                FieldPreflight {
                    acted: false,
                    state_key: sanitize_for_log(&target.state_key),
                    field_path,
                    reason,
                },
            )
        };
        let Some((path, surface)) = first_present_row(original_req) else {
            return unchanged(None, FIELD_PREFLIGHT_NO_GROUNDED_FIELD);
        };
        if !self.config.capability.enabled {
            return unchanged(Some(path), FIELD_PREFLIGHT_UNSUPPORTED_LANE);
        }
        let Some(provider_kind) = target.provider_kind else {
            return unchanged(Some(path), FIELD_PREFLIGHT_UNSUPPORTED_LANE);
        };
        if provider_kind != ANTHROPIC_API_KIND || target.use_forwarded_credential {
            return unchanged(Some(path), FIELD_PREFLIGHT_UNSUPPORTED_LANE);
        }
        // ATTRIBUTION, mirroring the reactive admission rather than
        // paraphrasing it: read the base URL from the operator's own
        // provider entry (the target carries none), and refuse both a lane
        // that reports no attributable Anthropic API URL -- which is how a
        // Bedrock Mantle entry is refused, since its accessor answers
        // `None` -- and a URL naming a local hop. A rejection this stage
        // could not have attributed to the upstream is a verdict this stage
        // must not act on proactively either.
        let Some(base_url) = self
            .config
            .providers
            .get(&target.provider_name)
            .and_then(crate::config::ProviderEntry::anthropic_api_base_url)
        else {
            return unchanged(Some(path), FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET);
        };
        if crate::field_verdict::loopback_target_suppresses_minting(base_url) {
            return unchanged(Some(path), FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET);
        }
        let Some(key) =
            crate::field_verdict::FieldVerdictKey::new(&target.state_key, path, provider_kind)
        else {
            return unchanged(Some(path), FIELD_PREFLIGHT_NO_IDENTITY);
        };
        // The operator `force_supported` mask, through the SAME two-tier
        // resolver the act and learn sides share, so a masked cell cannot be
        // honored on one side and missed here. The operator has said to send
        // this field; a learned verdict does not override that.
        if self.override_forces_supported(target, key.capability_key(), provider_kind) {
            return unchanged(Some(path), FIELD_PREFLIGHT_MASKED_BY_OVERRIDE);
        }
        let now = std::time::Instant::now();
        if !self
            .field_verdicts()
            .preflight_eligible(&key, self.registry_generation(), now)
        {
            return unchanged(Some(path), FIELD_PREFLIGHT_NOT_ELIGIBLE);
        }
        // The transform is applied through the ONE production helper below,
        // so the scratch-clone-and-adopt discipline has a single
        // implementation rather than a copy per caller.
        match apply_transform(original_req, surface) {
            Some(planned) => (
                planned,
                FieldPreflight {
                    acted: true,
                    state_key: sanitize_for_log(&target.state_key),
                    field_path: Some(path),
                    reason: FIELD_PREFLIGHT_ACTION_DROP,
                },
            ),
            None => unchanged(Some(path), FIELD_PREFLIGHT_AMBIGUOUS_MUTATION),
        }
    }
}

/// Apply `surface` to a SCRATCH clone of `original_req` and adopt it only on a
/// reported success whose post-condition holds.
///
/// THE single implementation of the planner's byte-safety contract, shared by
/// `plan_field_preflight` and its test driver -- a second copy for tests would
/// be a copy of the thing under test, so a mutation to production behavior
/// could leave the test copy (and every assertion on it) green.
///
/// `drop_from` mutates IN PLACE, which is the whole reason for the clone:
/// applying it to the request the planner is about to return leaves a
/// partially-mutated body behind on the false branch, reporting "did nothing"
/// while having removed a carrier. The post-condition re-check covers the
/// converse -- a transform reporting success while its surface is still
/// readable has not done what the record would claim.
/// `Some(body)` when the surface was removed and the post-condition holds --
/// adopt that body. `None` when the transform removed nothing or reported
/// success while its own surface is still readable; the ambiguous scratch is
/// DROPPED here rather than returned, so no caller can hand back a
/// half-mutated body by reading the wrong half of a result.
fn apply_transform(original_req: &ChatRequest, surface: FieldSurface) -> Option<ChatRequest> {
    let mut scratch = original_req.clone();
    if !surface.drop_from(&mut scratch) {
        return None;
    }
    if surface.present_in(&scratch) {
        return None;
    }
    Some(scratch)
}

/// Drive [`apply_transform`] for a surface the closed table cannot produce.
///
/// The production helper is called directly, so a mutation to its adoption or
/// failure behavior is observable here. This wrapper only builds the record --
/// it re-implements no part of the transform, and it deliberately starts AFTER
/// every admission check, since the transform is the only thing under test.
#[cfg(test)]
pub(super) fn plan_transform_for_tests(
    original_req: &ChatRequest,
    surface: FieldSurface,
    state_key: &str,
) -> (ChatRequest, FieldPreflight) {
    let path = "thinking.enabled.display";
    match apply_transform(original_req, surface) {
        Some(planned) => (
            planned,
            FieldPreflight {
                acted: true,
                state_key: sanitize_for_log(state_key),
                field_path: Some(path),
                reason: FIELD_PREFLIGHT_ACTION_DROP,
            },
        ),
        None => (
            original_req.clone(),
            FieldPreflight {
                acted: false,
                state_key: sanitize_for_log(state_key),
                field_path: Some(path),
                reason: FIELD_PREFLIGHT_AMBIGUOUS_MUTATION,
            },
        ),
    }
}

/// Message/event name of the pre-flight diagnostics. Stable, greppable,
/// closed-set.
const FIELD_PREFLIGHT_EVENT: &str = "envelope_field_preflight";

/// Emit this request's pre-flight diagnostics, by severity of what happened.
///
/// TWO tiers deliberately, because the two carry different information:
///
/// - Every recorded decision, acting or not, emits at DEBUG. A fail-open is
///   the routine case -- most requests on most targets are not eligible -- so
///   a WARN per decision would make an ordinary request look faulty and bury
///   the case an operator needs. The per-target records are RETAINED in full
///   (see [`super::DispatchMeta::field_preflight`]); only the routine ones are
///   quiet.
/// - Exactly ONE request-level WARN, and only when at least one target
///   actually acted. A request that rewrote a client's envelope before
///   dispatch is the reportable event; a request that changed nothing is not.
///   One line per REQUEST rather than per acting target, matching the
///   aggregate-diagnostic contract the rest of this surface uses.
///
/// `action` is only ever attached to a record that ACTED: labelling a
/// fail-open `field_preflight_drop` would name an action the walk did not
/// take, which is exactly the false-claim shape a diagnostic must not have.
/// The non-acting tier carries the reason token instead.
///
/// Called from the three public wrappers rather than inside the chain loops,
/// so a walk leaving by any exit still reports -- the same reason
/// `emit_field_repair` and `emit_replay_degradation` sit there.
///
/// Every field is a closed-set token, a code-authored path literal, a
/// `sanitize_for_log`-sanitized state key, or a boolean/count. Deliberately NO
/// request values, no response body, no credential, no session key, and no
/// upstream text at any verbosity.
pub(super) fn emit_field_preflight(meta: &super::DispatchMeta) {
    for record in &meta.field_preflight {
        if record.acted {
            tracing::debug!(
                event = FIELD_PREFLIGHT_EVENT,
                action = FIELD_PREFLIGHT_ACTION_DROP,
                acted = true,
                state_key = %sanitize_for_log(&record.state_key),
                field_path = record.field_path.unwrap_or(FIELD_PATH_NONE),
                reason = record.reason,
                "envelope-field pre-flight decision",
            );
        } else {
            // No `action`: nothing was done, so there is no action to name.
            tracing::debug!(
                event = FIELD_PREFLIGHT_EVENT,
                acted = false,
                state_key = %sanitize_for_log(&record.state_key),
                field_path = record.field_path.unwrap_or(FIELD_PATH_NONE),
                reason = record.reason,
                "envelope-field pre-flight decision",
            );
        }
    }
    let acted: Vec<&super::FieldPreflight> =
        meta.field_preflight.iter().filter(|r| r.acted).collect();
    let Some(first) = acted.first() else {
        // Nothing acted: this request rewrote no envelope, so it earns no
        // request-level WARN at all.
        return;
    };
    tracing::warn!(
        event = FIELD_PREFLIGHT_EVENT,
        action = FIELD_PREFLIGHT_ACTION_DROP,
        state_key = %sanitize_for_log(&first.state_key),
        field_path = first.field_path.unwrap_or(FIELD_PATH_NONE),
        targets_acted = acted.len(),
        targets_planned = meta.field_preflight.len(),
        "{FIELD_PREFLIGHT_WARN_MESSAGE}",
    );
}

/// Rendered `field_path` for a decision that considered no field. A literal
/// rather than an empty string so the line is unambiguous to a reader.
const FIELD_PATH_NONE: &str = "none";

/// Message of the single request-level pre-flight WARN. Stable and greppable,
/// and named so a test can assert on it without restating the string.
pub(super) const FIELD_PREFLIGHT_WARN_MESSAGE: &str =
    "envelope-field pre-flight rewrote a request before dispatch";

#[cfg(test)]
#[path = "field_preflight_tests.rs"]
mod tests;
