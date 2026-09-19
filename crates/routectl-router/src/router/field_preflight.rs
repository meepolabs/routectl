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
//! [`crate::field_verdict::FieldVerdictRegistry::preflight_eligible_incarnation`]), the
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

use std::time::Instant;

use routectl_core::{ChatRequest, sanitize_for_log};

use crate::field_canary::{CanaryClaimGuard, CanaryOutcome, ModifiedRequestGuard};
use crate::field_verdict::FieldVerdictKey;

use super::class_observe::DispatchSurface;
use super::field_repair::{ANTHROPIC_API_KIND, FieldSurface, first_present_row};
use super::repair_budget::RepairBudget;
use super::{CapabilityClearedEvent, CapabilityLearnEvent, DispatchTarget, FieldPreflight, Router};

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

/// Reason token: this request is the re-verification canary. The verdict was
/// eligible and the cadence came due, so the tested field was RESTORED rather
/// than dropped -- the planner deliberately did not act, and the outcome the
/// upstream returns re-verifies or disproves the verdict.
pub(super) const FIELD_PREFLIGHT_CANARY_RESTORED: &str = "canary_restored";

/// Action token: the pre-flight planner dropped the mapped envelope field
/// from the per-target request before first dispatch.
pub(super) const FIELD_PREFLIGHT_ACTION_DROP: &str = "field_preflight_drop";

/// What one planning decision leaves the dispatch arm holding, beyond the
/// per-target request and its record.
///
/// A three-state type rather than two booleans, because the three states own
/// DIFFERENT state and the arm's settlement differs per state: only the canary
/// state carries a claim to settle, only the repaired state carries a
/// wrong-repair tally entry, and the inert state carries neither. Encoding this
/// as flags is how a settlement comes to fire on a request that never claimed
/// anything.
#[derive(Debug, Default)]
pub(super) enum FieldPreflightPlan<'a> {
    /// Nothing was planned for this target: no grounded field, an excluded
    /// lane, or no eligible verdict. There is nothing to settle.
    #[default]
    Inert,
    /// The field was DROPPED before dispatch. The guard holds this request's
    /// entry in the identity's wrong-repair accounting; it needs no settlement
    /// because an outcome says nothing about a repair the verdict already
    /// justified -- only the canary's outcome does.
    ///
    /// The guard is never READ, and that is the point: holding it for the
    /// chain iteration IS its contract, and its `Drop` is what clears the
    /// in-flight half of the count on every exit path. Binding it to `_` at the
    /// call site instead would drop it immediately and leave every repaired
    /// request reporting zero in flight.
    Repaired(
        #[expect(dead_code, reason = "held for its Drop; see the variant docs")]
        ModifiedRequestGuard<'a>,
    ),
    /// This request is the re-verification CANARY: the field was restored and
    /// the upstream's answer re-verifies or disproves the verdict. The arm owes
    /// this plan exactly one settlement, and RAII releases the claim on every
    /// path that reaches none.
    Canary(Box<CanaryPlan<'a>>),
}

/// The canary claim plus the identity and generation its settlement must carry.
///
/// Dropping this plan without naming an outcome settles it as
/// [`CanaryOutcome::Inconclusive`] -- which is what a cancellation, a timeout, a
/// dropped future, or a walk leaving by an unrouted error path IS. Inconclusive
/// rather than a bare release, because the two differ observably: a release
/// records no outcome, so the operator-facing last-outcome field would report
/// the PREVIOUS interval's result and an abandoned re-verification would look
/// like a settled one. Nothing verdict-facing can move from here regardless --
/// `Inconclusive` touches no verdict, no wrong-repair tally, and no alarm.
pub(super) struct CanaryPlan<'a> {
    /// `None` once a settlement has consumed the claim, which is what makes the
    /// `Drop` default fire exactly on the paths that named no outcome.
    claim: Option<CanaryClaimGuard<'a>>,
    key: FieldVerdictKey,
    surface: FieldSurface,
    /// The generation the ELIGIBILITY decision was validated under, carried so
    /// a settlement presents the same token rather than re-reading one a
    /// boundary may have moved in the meantime.
    generation: u64,
}

impl Drop for CanaryPlan<'_> {
    fn drop(&mut self) {
        if let Some(claim) = self.claim.take() {
            claim.settle(CanaryOutcome::Inconclusive);
        }
    }
}

impl std::fmt::Debug for CanaryPlan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because the claim guard's own Debug would print the
        // whole shared registry. The identity is what a diagnostic wants, and
        // the capability key is already a normalized token rather than
        // upstream text.
        f.debug_struct("CanaryPlan")
            .field("capability_key", &self.key.capability_key())
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl<'a> FieldPreflightPlan<'a> {
    /// The canary plan this decision holds, if it is a canary. Taken by value
    /// so a settlement consumes the claim -- a plan cannot be settled twice.
    pub(super) fn into_canary(self) -> Option<Box<CanaryPlan<'a>>> {
        match self {
            Self::Canary(plan) => Some(plan),
            Self::Inert | Self::Repaired(_) => None,
        }
    }

    /// The closed-table surface a canary restored, so the arm's repaired retry
    /// drops the SAME surface the restoration put back rather than re-deriving
    /// it from the rejection.
    pub(super) const fn canary_surface(&self) -> Option<FieldSurface> {
        match self {
            Self::Canary(plan) => Some(plan.surface),
            Self::Inert | Self::Repaired(_) => None,
        }
    }
}

impl CanaryPlan<'_> {
    /// Settle as CONFIRMED: the restored field drew the same structured
    /// rejection and the repaired retry succeeded. Persists the confirmation
    /// and returns the row for the caller to drain to the ledger.
    pub(super) fn settle_confirmed(
        mut self,
        registry: &crate::field_verdict::FieldVerdictRegistry,
        upstream_status: u16,
        request_features: Vec<String>,
        now: Instant,
    ) -> Option<CapabilityLearnEvent> {
        // The claim is HANDED OVER rather than settled here: recording the
        // confirmation and settling it are one registry-owned operation, so the
        // claim's release and the incarnation carry are never separately
        // observable. The learned-registry observation that MINTS the new
        // incarnation necessarily precedes that critical section, so there is a
        // window in which the row is minted and the canary state has not moved
        // yet; a straggler arriving in it is refused on ownership rather than
        // being allowed to write (see
        // `FieldVerdictRegistry::record_canary_confirmation`).
        let claim = self.claim.take()?;
        registry.record_canary_confirmation(
            &self.key,
            claim,
            self.generation,
            upstream_status,
            request_features,
            now,
        )
    }

    /// Settle as DISPROVED: the restored field was accepted unrepaired.
    ///
    /// The claim is HANDED OVER rather than settled here, for the same reason the
    /// confirmation above hands it over: settling suspends pre-flight and
    /// transfers the affected-request tally into the alarm, the durable clear that
    /// follows names only the identity and not the lifecycle, and the two must not
    /// be separately decidable. A claim the settlement finds SUPERSEDED authorizes
    /// no clear at all -- see
    /// `FieldVerdictRegistry::record_canary_disproof`.
    pub(super) fn settle_disproved(
        mut self,
        registry: &crate::field_verdict::FieldVerdictRegistry,
    ) -> Option<CapabilityClearedEvent> {
        let claim = self.claim.take()?;
        registry.record_canary_disproof(&self.key, claim, self.generation)
    }
}

impl Router {
    /// Plan the per-target request for `target`, starting from
    /// `original_req` -- the canonical request as the client sent it,
    /// upstream of any per-attempt overlay or strip this walk has already
    /// applied for a PRIOR target in the same chain.
    ///
    /// Returns a fresh clone, an immutable decision record, and the plan whose
    /// settlement the caller owes (see [`FieldPreflightPlan`]). The clone
    /// carries the pre-flight rewrite only when a resident verdict is
    /// ACTING and pre-flight eligible for the one grounded field this
    /// request carries AND this request is not the re-verification canary;
    /// every other case returns a clone of the original, unchanged.
    ///
    /// `surface` selects whether this request participates in the canary
    /// cadence. Only [`DispatchSurface::Complete`] does, and the exclusion is
    /// upstream of both the countdown tick and the claim rather than a filter on
    /// the settlement: a stream or a token count that advanced the cadence would
    /// consume the interval a completion request is supposed to fill, so the
    /// identity would be re-verified on a surface whose outcome the walk cannot
    /// settle (no assembled response on a stream, no envelope verdict from a
    /// token count).
    ///
    /// `budget` remains the caller's request-scoped reactive-repair ceiling.
    /// Neither a pre-flight rewrite nor a canary draws from it: both act on a
    /// verdict that is already resident and settled, and the canary's own
    /// repaired retry is the re-verification itself rather than a reactive
    /// repair of a fresh rejection. The parameter stays so a later
    /// prefix-impacting transform can consult it without a signature change.
    pub(super) fn plan_field_preflight<'a>(
        &'a self,
        original_req: &ChatRequest,
        target: &DispatchTarget,
        surface: DispatchSurface,
        _budget: &RepairBudget,
    ) -> (ChatRequest, FieldPreflight, FieldPreflightPlan<'a>) {
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
                FieldPreflightPlan::Inert,
            )
        };
        let Some((path, surface_row)) = first_present_row(original_req) else {
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
        let now = Instant::now();
        let generation = self.registry_generation();
        // The eligibility read hands back the INCARNATION its decision rests
        // on, rather than the caller re-reading one afterwards: a canary claim
        // and its settlement must carry the same incarnation the authorization
        // was validated against, or a settlement could name a lifecycle the
        // decision never checked.
        let Some(incarnation) = self
            .field_verdicts()
            .preflight_eligible_incarnation(&key, generation, now)
        else {
            return unchanged(Some(path), FIELD_PREFLIGHT_NOT_ELIGIBLE);
        };
        let canaries = self.field_verdicts().canaries();
        // CADENCE, and the two operations are separate for a reason the claim
        // depends on: the tick is atomic per identity (so concurrent callers
        // cannot both observe one trip), and the claim is atomic per identity
        // (so concurrent callers cannot both hold the slot). A caller that
        // trips the countdown and then finds the slot taken -- a canary from an
        // earlier interval still in flight -- repairs normally rather than
        // dispatching a second unrepaired request, which is what makes "exactly
        // one in-flight canary" hold under real concurrency and not merely
        // under a single thread.
        let canary_due =
            surface == DispatchSurface::Complete && canaries.tick_cadence(&key, incarnation);
        if canary_due && let Some(claim) = canaries.claim_canary(&key, incarnation) {
            // RESTORED, not rewritten: the returned request is a clone of the
            // original, carrying the tested field exactly as the client sent
            // it. Unrelated eligible repairs are not in play here because the
            // closed table has one row -- a second row would restore only the
            // row under test and still drop the others, which is why the
            // restoration is expressed as "decline to transform THIS surface"
            // rather than as a revert of an already-planned body.
            return (
                original_req.clone(),
                FieldPreflight {
                    acted: false,
                    state_key: sanitize_for_log(&target.state_key),
                    field_path: Some(path),
                    reason: FIELD_PREFLIGHT_CANARY_RESTORED,
                },
                FieldPreflightPlan::Canary(Box::new(CanaryPlan {
                    claim: Some(claim),
                    key,
                    surface: surface_row,
                    generation,
                })),
            );
        }
        // The transform is applied through the ONE production helper below,
        // so the scratch-clone-and-adopt discipline has a single
        // implementation rather than a copy per caller.
        match apply_transform(original_req, surface_row) {
            // The request is now counted against this identity's wrong-repair
            // exposure, RAII: the guard's Drop clears its in-flight half on every
            // exit, and the tally half stands until a canary vouches for it or
            // charges it to the alarm.
            //
            // `None` from the accounting means this planner is a STRAGGLER: a
            // confirmation carried the identity forward after the eligibility
            // read authorized this request. Fail open and forward unchanged
            // rather than apply a repair whose exposure nothing would count --
            // an unaccounted repair is invisible to a later disproof's alarm.
            Some(planned) => match canaries.begin_modified_request(&key, incarnation) {
                Some(accounting) => (
                    planned,
                    FieldPreflight {
                        acted: true,
                        state_key: sanitize_for_log(&target.state_key),
                        field_path: Some(path),
                        reason: FIELD_PREFLIGHT_ACTION_DROP,
                    },
                    FieldPreflightPlan::Repaired(accounting),
                ),
                None => unchanged(Some(path), FIELD_PREFLIGHT_NOT_ELIGIBLE),
            },
            None => unchanged(Some(path), FIELD_PREFLIGHT_AMBIGUOUS_MUTATION),
        }
    }

    /// Whether `plan`'s canary may repair and re-dispatch over the rejection
    /// `err`, and if so drop the tested field from `attempt_req` -- all or
    /// nothing, mirroring the reactive `FieldRepairPlan::apply`.
    ///
    /// `Some(status)` means the field is gone, the estimate describes the new
    /// payload, and the caller must re-dispatch this same target and later
    /// settle the plan as CONFIRMED on success. `None` means the rejection did
    /// not name the tested field, or the drop removed nothing: nothing was
    /// mutated and the caller must leave the rejection on its ordinary path.
    ///
    /// Deliberately draws NO repair budget, unlike the reactive arm. The two
    /// spend for different things: a reactive repair pays for the chance that a
    /// fresh rejection is repairable, while this retry is the second half of a
    /// re-verification routectl itself scheduled. Charging it would let a
    /// request that happened to carry the canary silently lose the reactive
    /// allowance it is separately entitled to, and would make the canary's
    /// completion depend on a ceiling unrelated to re-verification.
    ///
    /// The NATIVE class is the input, never the operator-remapped one, for the
    /// same reason the reactive arm reads it: a `[class_overrides]` entry states
    /// how a status should be ROUTED, not what the upstream said, so reading it
    /// would let an override turn a 429 into a canary confirmation.
    pub(super) fn canary_repaired_retry(
        plan: &FieldPreflightPlan<'_>,
        attempt_req: &mut ChatRequest,
        meta: &mut super::DispatchMeta,
        native_class: &routectl_core::failure_class::FailureClass,
        err: &routectl_core::Error,
        provider_kind: &str,
    ) -> Option<u16> {
        let surface = plan.canary_surface()?;
        // The rejection must name the SAME row the canary restored, compared on
        // the surface rather than the path string: the surface is what the drop
        // acts on, and two rows sharing one surface would be the same mutation
        // under two identities.
        if !Self::rejection_names_field_surface(surface, native_class, err, provider_kind) {
            return None;
        }
        // Presence is re-read here rather than assumed from the restoration:
        // the request has been through a dispatch since, and a drop that
        // removes nothing must not be reported as a repair.
        if !surface.present_in(attempt_req) {
            return None;
        }
        if !surface.drop_from(attempt_req) {
            return None;
        }
        super::dispatch::restamp_calibration_estimate(attempt_req, meta);
        Some(
            super::class_observe::upstream_facts(err)
                .status
                .unwrap_or(0),
        )
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
