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
//! [`crate::field_verdict::FieldVerdictRegistry::preflight_authorization`]), the
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
//! # Every present row, each gated on its own terms
//!
//! The planner scans EVERY closed-table row the request carries, not the
//! first one: a request can carry rows of two transform classes at once, and
//! each class clears its own gates. One decision record is produced per
//! CONSIDERED row, so a request whose envelope row acted and whose
//! prefix-impacting row was blocked reports both facts rather than only the
//! last. Table order is the enumeration order and nothing more -- each
//! decision is computed from that row's own class, verdict, and gates, so
//! reordering the table reorders the records without changing any of them.
//!
//! # The content gate
//!
//! An ENVELOPE-class row acts on one acknowledged confirmation. A
//! PREFIX-IMPACTING row additionally needs the confirmation quorum
//! ([`crate::config::PREFIX_QUORUM`]) and an explicit `[fidelity]
//! prefix_impact_opt_in` entry for the target, resolved through the SAME
//! two-tier target-spec grammar `[capability.overrides]` uses -- so a
//! prefix-impacting rewrite stays dormant until the operator names the
//! target, however well confirmed the verdict is. Both the quorum and the
//! cadence are code constants: a knob an operator (or an agent) can turn
//! mid-session with no diff is an unlogged exemption, not a config option.
//!
//! # The canary, and why it is one row's business
//!
//! The re-verification canary belongs to ONE identity, so it restores the
//! row under test and leaves every other row's decision alone. A request
//! carrying two rows can therefore dispatch one restored and one rewritten,
//! which is the correct shape: the cadence measures a verdict, not a request.
//! At most one canary is claimed per planning call, because the arm settles
//! exactly one.

use std::time::Instant;

use routectl_core::{ChatRequest, sanitize_for_log};

use crate::field_canary::{CanaryClaimGuard, CanaryOutcome, ModifiedRequestGuard};
use crate::field_verdict::{FieldVerdictKey, PreflightAuthorization};

use super::class_observe::DispatchSurface;
use super::field_repair::{
    ANTHROPIC_API_KIND, FieldRepairRow, FieldSurface, TransformClass, present_rows,
};
use super::{
    CapabilityClearedEvent, CapabilityLearnEvent, DispatchTarget, FieldPreflight,
    FieldPreflightAuthorizationRecord, Router,
};

/// Reason token: no closed-table field is present in the request at all, so
/// there is nothing for a pre-flight rewrite to act on.
pub(super) const FIELD_PREFLIGHT_NO_GROUNDED_FIELD: &str = "no_grounded_field";

/// Reason token: durable capability-event persistence cannot be guaranteed right
/// now, so learned pre-flight is suspended.
///
/// Every escape hatch a pre-flight rewrite depends on is a capability-event
/// WRITE -- the durable clear a disproving canary performs, the operator purge,
/// and the confirmation acknowledgment that made the verdict eligible. While
/// those cannot be guaranteed, a verdict that turns out to be wrong cannot be
/// taken out of service durably: the in-memory suspension lasts only until the
/// process restarts, and the next boot's replay restores it. Reactive
/// forward-and-repair is unaffected and still serves every request.
///
/// Checked BEFORE any verdict is read, alongside the lane gates, because it
/// refuses the whole learned-pre-flight mechanism rather than one verdict's
/// evidence.
pub(super) const FIELD_PREFLIGHT_WRITER_UNHEALTHY: &str = "capability_writer_unhealthy";

/// Reason token: the capability learning kill switch is off, or the target is
/// not on the one lane this stage acts on. Checked before any verdict is read.
///
/// Does NOT cover the forwarded-credential refusal. That one reports
/// [`FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET`], because it is decided inside the
/// shared attributability decision rather than by a lane check here -- keeping
/// the two apart is what stops a second copy of the refusal from drifting from
/// the first.
pub(super) const FIELD_PREFLIGHT_UNSUPPORTED_LANE: &str = "unsupported_lane";

/// Reason token: this stage could not attribute a rejection from the target to
/// a routectl-owned seat. Three causes, all decided by the one shared
/// decision (`field_repair::attributable_anthropic_base_url`):
///
/// - the target authenticates with a FORWARDED client credential, so one
///   client's rejection must not mint a verdict steering every other client;
/// - the target carries no attributable Anthropic API base URL (a Bedrock
///   Mantle entry reports none and is refused here);
/// - the URL it carries names a local hop.
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

/// Reason token: the verdict is acting and acknowledged, but its
/// acknowledged confirmation count is below the quorum this transform
/// class requires. The prefix-impacting class needs two confirmed
/// reject-unrepaired -> accept-repaired cycles; one is not enough.
pub(super) const FIELD_PREFLIGHT_BELOW_QUORUM: &str = "below_quorum";

/// Reason token: a prefix-impacting transform whose quorum is satisfied but
/// whose target is not named in `[fidelity] prefix_impact_opt_in`. A content
/// rewrite stays dormant until the operator opts the target in, however well
/// confirmed the verdict.
pub(super) const FIELD_PREFLIGHT_NO_TARGET_OPT_IN: &str = "no_target_opt_in";

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

/// What one planning call leaves the dispatch arm holding, beyond the
/// per-target request and its records.
///
/// A struct rather than an enum, because the two things it carries are not
/// alternatives: a request can both rewrite rows AND be one identity's canary,
/// and it holds one accounting entry PER acting row. The earlier enum shape
/// forced those into a single slot, so a second acting row's accounting had
/// nowhere to live and was dropped on the spot -- which left the identity
/// reporting zero requests in flight while one was.
///
/// At most ONE canary, however many rows are present, because the arm settles
/// exactly one; every acting row's accounting guard is kept.
#[derive(Debug, Default)]
pub(super) struct FieldPreflightPlan<'a> {
    /// The re-verification CANARY this request is, if any: the field was
    /// restored and the upstream's answer re-verifies or disproves that
    /// identity's verdict. The arm owes this exactly one settlement, and RAII
    /// releases the claim on every path that reaches none.
    canary: Option<Box<CanaryPlan<'a>>>,
    /// One entry per row whose field was DROPPED before dispatch. Each holds
    /// that identity's wrong-repair accounting; none needs a settlement,
    /// because an outcome says nothing about a repair the verdict already
    /// justified -- only the canary's outcome does.
    ///
    /// The guards are never READ, and that is the point: holding them for the
    /// chain iteration IS their contract, and their `Drop` is what clears the
    /// in-flight half of each count on every exit path. Dropping one at the
    /// planning site instead would leave that row's identity reporting zero in
    /// flight while the request it modified was still outstanding. No
    /// `expect(dead_code)` is needed (unlike the guard's previous single-slot
    /// shape): the vector is written through, which is use enough for the lint.
    accounting: Vec<ModifiedRequestGuard<'a>>,
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

/// What one row's planning produced: its immutable record, plus whatever that
/// row leaves the arm holding.
///
/// A struct rather than a tuple because the halves are settled independently --
/// every row contributes a record, at most one contributes a canary, and every
/// acting row contributes an accounting guard.
struct RowDecision<'a> {
    record: FieldPreflight,
    /// The canary this row claimed, if it did.
    canary: Option<Box<CanaryPlan<'a>>>,
    /// This row's wrong-repair accounting, if it acted.
    accounting: Option<ModifiedRequestGuard<'a>>,
}

impl<'a> FieldPreflightPlan<'a> {
    /// The canary plan this request holds, if it is one identity's canary.
    /// Taken by value so a settlement consumes the claim -- a plan cannot be
    /// settled twice.
    pub(super) fn into_canary(self) -> Option<Box<CanaryPlan<'a>>> {
        self.canary
    }

    /// The closed-table surface a canary restored, so the arm's repaired retry
    /// drops the SAME surface the restoration put back rather than re-deriving
    /// it from the rejection.
    pub(super) fn canary_surface(&self) -> Option<FieldSurface> {
        self.canary.as_ref().map(|plan| plan.surface)
    }

    /// How many acting rows' accounting entries this plan holds. Test-only: the
    /// production contract is that the guards are HELD, which a reader of the
    /// count could mistake for a reason to inspect them.
    #[cfg(test)]
    pub(super) const fn accounting_len(&self) -> usize {
        self.accounting.len()
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
    /// Returns a fresh clone, one immutable decision record PER CONSIDERED
    /// closed-table row, and the plan whose settlement the caller owes (see
    /// [`FieldPreflightPlan`]). The clone carries a row's pre-flight rewrite
    /// only when a resident verdict is ACTING, pre-flight eligible, past every
    /// gate that row's transform class requires, and that row is not the one
    /// under re-verification this request; every other case leaves that row's
    /// surface exactly as the client sent it.
    ///
    /// Each row's transform is applied to the request the PREVIOUS row's
    /// decision produced, through the same scratch-clone-and-adopt helper, so
    /// two acting rows compose onto one body while a refusal in between leaves
    /// the accumulated body untouched rather than discarding an earlier row's
    /// rewrite.
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
    /// At most ONE canary is claimed per call, however many rows are present,
    /// because the dispatch arm settles exactly one. A row reached after a
    /// canary has been claimed still gets its own decision and still advances
    /// its own cadence -- it simply cannot claim a second canary, and a trip it
    /// cannot claim stays DUE for the next request rather than being postponed.
    ///
    /// Deliberately takes NO repair budget. Neither a pre-flight rewrite nor a
    /// canary draws from the caller's reactive ceiling -- both act on a verdict
    /// already resident and settled, and the canary's own repaired retry is the
    /// re-verification itself rather than a reactive repair of a fresh
    /// rejection -- so a threaded-but-unread parameter would be a false
    /// signal that some invariant here consults it.
    pub(super) fn plan_field_preflight<'a>(
        &'a self,
        original_req: &ChatRequest,
        target: &DispatchTarget,
        surface: DispatchSurface,
    ) -> (ChatRequest, Vec<FieldPreflight>, FieldPreflightPlan<'a>) {
        let mut rows = present_rows(original_req).peekable();
        if rows.peek().is_none() {
            return (
                original_req.clone(),
                vec![unchanged_record(
                    target,
                    None,
                    None,
                    FIELD_PREFLIGHT_NO_GROUNDED_FIELD,
                )],
                FieldPreflightPlan::default(),
            );
        }
        // Accumulates the adopted rewrites. Starts as a clone of the ORIGINAL,
        // so a walk where no row acts hands back bytes identical to what the
        // client sent, and a row that refuses leaves whatever earlier rows
        // adopted rather than reverting it.
        let mut planned = original_req.clone();
        let mut records = Vec::new();
        let mut plan = FieldPreflightPlan::default();
        for row in rows {
            // The canary slot is offered to a row only while it is still free:
            // the arm settles one plan, so a second claim could never be
            // reported. A row denied the slot still ticks its own cadence and
            // stays due, which is what stops a consistently-later row from
            // being starved of re-verification.
            let can_claim_canary = plan.canary.is_none();
            let decided = self.plan_one_row(&mut planned, target, row, surface, can_claim_canary);
            records.push(decided.record);
            if let Some(canary) = decided.canary {
                debug_assert!(
                    plan.canary.is_none(),
                    "a row may only claim the canary slot while it is free",
                );
                plan.canary = Some(canary);
            }
            // Every acting row's accounting is RETAINED, not just the first:
            // each entry belongs to a different identity, and a dropped one
            // would report zero requests in flight for an identity this
            // request is in fact modifying.
            plan.accounting.extend(decided.accounting);
        }
        (planned, records, plan)
    }

    /// Decide `row` for `target` and, when every gate clears, adopt its
    /// transform onto `planned`.
    ///
    /// `planned` is mutated ONLY through [`apply_transform`], which works on
    /// a scratch clone and is adopted whole on success -- so a refusal at any
    /// point below, and an ambiguous transform, leave `planned` exactly as
    /// this call received it.
    ///
    /// `can_claim_canary` is whether the request's sole settlement slot is still
    /// free. A row denied it still ticks its own cadence and stays DUE, so being
    /// second in the closed table delays a re-verification by one request rather
    /// than starving it.
    fn plan_one_row<'a>(
        &'a self,
        planned: &mut ChatRequest,
        target: &DispatchTarget,
        row: FieldRepairRow,
        surface: DispatchSurface,
        can_claim_canary: bool,
    ) -> RowDecision<'a> {
        let refuse = |reason| RowDecision {
            record: unchanged_record(target, Some(row.path), Some(row.class), reason),
            canary: None,
            accounting: None,
        };
        let Some(provider_kind) = self.preflight_lane_admits(target) else {
            return refuse(FIELD_PREFLIGHT_UNSUPPORTED_LANE);
        };
        let key = match self.preflight_identity(target, row.path, provider_kind) {
            Ok(key) => key,
            Err(reason) => return refuse(reason),
        };
        let gates = GateInputs {
            target,
            key: &key,
            provider_kind,
            class: row.class,
            generation: self.registry_generation(),
            now: Instant::now(),
        };
        let generation = gates.generation;
        let authorization = match self.preflight_authorization_for(gates) {
            Ok(authorization) => authorization,
            Err(reason) => return refuse(reason),
        };
        let claim = self.try_claim_canary(
            key,
            row,
            surface,
            can_claim_canary,
            authorization,
            generation,
        );
        let key = match claim {
            // RESTORED, not rewritten: THIS row's surface is left exactly as
            // the client sent it, expressed as "decline to transform this
            // surface" rather than as a revert of an already-planned body --
            // which is what leaves every OTHER row's adopted rewrite standing
            // on the accumulated body. A canary re-verifies one identity, so
            // unrelated eligible repairs remain applied.
            Err(canary) => return restored_decision(target, row, canary, authorization),
            Ok(key) => key,
        };
        match self.adopt_row(planned, target, row, &key, authorization) {
            Ok(decision) => decision,
            Err(reason) => refuse(reason),
        }
    }

    /// Apply `row`'s transform onto `planned` and open its wrong-repair
    /// accounting -- the last step, reached only once every gate has cleared and
    /// this row is not the canary.
    ///
    /// `Err(reason)` on either of two fail-open cases, each carrying its own
    /// closed-set token because they are different operator situations: an
    /// AMBIGUOUS transform (removed nothing, or reported success while its
    /// surface is still readable), and a STRAGGLER whose accounting the identity
    /// refuses because a confirmation carried it forward after the authorization
    /// read. Both leave `planned` exactly as this call received it.
    fn adopt_row<'a>(
        &'a self,
        planned: &mut ChatRequest,
        target: &DispatchTarget,
        row: FieldRepairRow,
        key: &FieldVerdictKey,
        authorization: PreflightAuthorization,
    ) -> Result<RowDecision<'a>, &'static str> {
        // The transform is applied through the ONE production helper below,
        // so the scratch-clone-and-adopt discipline has a single
        // implementation rather than a copy per caller.
        let adopted =
            apply_transform(planned, row.surface).ok_or(FIELD_PREFLIGHT_AMBIGUOUS_MUTATION)?;
        // The request is now counted against this identity's wrong-repair
        // exposure, RAII: the guard's Drop clears its in-flight half on every
        // exit, and the tally half stands until a canary vouches for it or
        // charges it to the alarm. The guard is RETURNED for the plan to hold,
        // never dropped here -- a dropped one would report zero in flight for
        // an identity whose request is outstanding.
        //
        // `None` here means this planner is a STRAGGLER: a confirmation carried
        // the identity forward after the authorization read admitted this
        // request. Fail open and forward unchanged rather than apply a repair
        // whose exposure nothing would count -- an unaccounted repair is
        // invisible to a later disproof's alarm.
        let accounting = self
            .field_verdicts()
            .canaries()
            .begin_modified_request(key, authorization.incarnation)
            .ok_or(FIELD_PREFLIGHT_NOT_ELIGIBLE)?;
        *planned = adopted;
        // Counted HERE, at the one site that swaps the rewritten body in, so the
        // count cannot outrun what actually went upstream: a decision refused by
        // any gate above never reaches this line, and the routine fail-open is
        // most decisions on most requests. Per ADOPTED ROW rather than per
        // request, because a request carrying two classes rewrites two surfaces
        // and exposes two identities.
        self.metrics.incr_field_preflight_action();
        Ok(RowDecision {
            record: FieldPreflight {
                acted: true,
                state_key: sanitize_for_log(&target.state_key),
                field_path: Some(row.path),
                transform_class: Some(row.class.as_str()),
                reason: FIELD_PREFLIGHT_ACTION_DROP,
                // The EXACT authorization this rewrite rested on, carried rather
                // than re-read: the identity's state can move after the gates
                // clear, and a record assembled from a later read would report a
                // provenance or a canary posture this rewrite was never
                // authorized under.
                authorization: Some(authorization_record(authorization)),
            },
            canary: None,
            accounting: Some(accounting),
        })
    }

    /// Advance `row`'s identity cadence and, when it is due and the request's sole
    /// settlement slot is still free, CLAIM its canary.
    ///
    /// `Err(plan)` is the CLAIM -- the `Result` is control flow, not an error
    /// signal: a claimed canary is the caller's early exit, and handing the key
    /// back on `Ok` is what makes the compiler enforce that a key consumed into a
    /// claim cannot also plan a rewrite for the same row.
    ///
    /// The tick is atomic per identity (so concurrent callers cannot both observe
    /// one trip) and the claim is atomic per identity (so concurrent callers
    /// cannot both hold the slot). A caller that finds the identity due and then
    /// finds the slot taken -- an earlier interval's canary still in flight --
    /// repairs normally rather than dispatching a second unrepaired request,
    /// which is what makes "exactly one in-flight canary" hold under real
    /// concurrency and not merely under one thread.
    ///
    /// EVERY eligible complete request ticks, including one whose settlement slot
    /// a sibling row already owns: the cadence measures this verdict's exposure,
    /// and skipping the tick would mean a row that is consistently second in the
    /// table never reaches its interval at all. Dueness is STICKY, so the trip
    /// such a request observes is not spent -- the next eligible request claims
    /// it, which bounds the delay at one request rather than one interval.
    fn try_claim_canary(
        &self,
        key: FieldVerdictKey,
        row: FieldRepairRow,
        surface: DispatchSurface,
        can_claim_canary: bool,
        authorization: PreflightAuthorization,
        generation: u64,
    ) -> Result<FieldVerdictKey, Box<CanaryPlan<'_>>> {
        let canaries = self.field_verdicts().canaries();
        // The incarnation comes from the caller's ONE authorization read, never
        // from a fresh snapshot: a claim and its later settlement must carry the
        // incarnation the authorization was validated against.
        let incarnation = authorization.incarnation;
        let due = surface == DispatchSurface::Complete && canaries.tick_cadence(&key, incarnation);
        if due
            && can_claim_canary
            && let Some(claim) = canaries.claim_canary(&key, incarnation)
        {
            return Err(Box::new(CanaryPlan {
                claim: Some(claim),
                key,
                surface: row.surface,
                generation,
            }));
        }
        Ok(key)
    }

    /// The LANE gates, shared with the reactive admission's exclusions: the
    /// capability kill switch and the one provider kind this stage acts on.
    /// `Some(provider_kind)` when the lane admits.
    ///
    /// The forwarded-credential refusal is deliberately NOT repeated here. It
    /// lives in the shared attributability decision
    /// (`field_repair::attributable_anthropic_base_url`), which
    /// `preflight_identity` calls with this target's own flag, so every stage
    /// draws its verdict from one refusal set rather than from a local copy.
    ///
    /// What is CHECKED is the verdict, not the absence of a copy: a parity test
    /// drives every stage over the provider shapes it varies -- base URL,
    /// credential source, entry presence, and the Mantle sub-lane -- and reds
    /// when a stage narrows or widens its verdict on one of those. A decision
    /// keyed on some OTHER entry fact needs a new discriminating row before any
    /// test can see it, and a local check that merely restates part of the
    /// shared set changes no verdict at all. Calling through is a maintenance
    /// convention here, not an enforced one.
    fn preflight_lane_admits(&self, target: &DispatchTarget) -> Option<&'static str> {
        if !self.config.capability.enabled {
            return None;
        }
        let provider_kind = target.provider_kind?;
        if provider_kind != ANTHROPIC_API_KIND {
            return None;
        }
        Some(provider_kind)
    }

    /// The ATTRIBUTION gates plus the field identity, mirroring the reactive
    /// admission rather than paraphrasing it: read the base URL from the
    /// operator's own provider entry (the target carries none), and refuse both
    /// a lane that reports no attributable Anthropic API URL -- which is how a
    /// Bedrock Mantle entry is refused, since its accessor answers `None` -- and
    /// a URL naming a local hop. A rejection this stage could not have
    /// attributed to the upstream is a verdict this stage must not act on
    /// proactively either.
    ///
    /// `Err(reason)` carries the closed-set token the refusal reports.
    fn preflight_identity(
        &self,
        target: &DispatchTarget,
        path: &'static str,
        provider_kind: &'static str,
    ) -> Result<FieldVerdictKey, &'static str> {
        // THE shared attributability read, so this stage cannot refuse a
        // target the reactive arm would act on, or act on one it would refuse.
        crate::router::field_repair::attributable_anthropic_base_url(
            &self.config,
            &target.provider_name,
            target.use_forwarded_credential,
        )
        .ok_or(FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET)?;
        FieldVerdictKey::new(&target.state_key, path, provider_kind)
            .ok_or(FIELD_PREFLIGHT_NO_IDENTITY)
    }

    /// The VERDICT gates for one row's class: the operator mask, eligibility,
    /// this class's confirmation quorum, and the content opt-in.
    ///
    /// `Err(reason)` carries the closed-set token the refusal reports; each gate
    /// has its own, because they are different operator situations and
    /// collapsing them hides which one is holding.
    fn preflight_authorization_for(
        &self,
        gates: GateInputs<'_>,
    ) -> Result<PreflightAuthorization, &'static str> {
        let GateInputs {
            target,
            key,
            provider_kind,
            class,
            generation,
            now,
        } = gates;
        // The operator `force_supported` mask, through the SAME two-tier
        // resolver the act and learn sides share, so a masked cell cannot be
        // honored on one side and missed here. The operator has said to send
        // this field; a learned verdict does not override that. Checked
        // BEFORE the quorum so a masked cell reports the mask rather than a
        // confirmation shortfall -- the mask is the operative reason, and it
        // holds at any confirmation count.
        if self.override_forces_supported(target, key.capability_key(), provider_kind) {
            return Err(FIELD_PREFLIGHT_MASKED_BY_OVERRIDE);
        }
        // PERSISTENCE, after the operator mask and before every EVIDENCE gate.
        //
        // After the mask because the mask is the operative reason at any writer
        // health: the operator has said to send this field, so a healthy writer
        // would not change this row's fate and reporting the writer would send
        // them to fix the wrong thing.
        //
        // Before eligibility and quorum because it is not a verdict gate at all.
        // Every escape hatch a pre-flight rewrite depends on is a capability-event
        // WRITE -- the durable clear a disproving canary performs, the operator
        // purge, the confirmation acknowledgment -- so while those cannot be
        // guaranteed, a verdict that turns out to be wrong could not be taken out
        // of service durably: the in-memory suspension lasts only until the
        // process restarts, and the next boot's replay restores it. Reporting a
        // confirmation shortfall here would make an unhealthy writer look like
        // missing evidence, and an operator would go looking for confirmations
        // that would never help.
        //
        // Reactive forward-and-repair is untouched by this and still serves every
        // request: it acts only after an upstream rejection, so its evidence is in
        // hand on the request it serves and it needs no durable record to be
        // correct.
        if !self.capability_writes_durable() {
            return Err(FIELD_PREFLIGHT_WRITER_UNHEALTHY);
        }
        // ONE authorization read backs both gates below: the incarnation a
        // canary claim and its settlement must carry, plus the acknowledged
        // confirmation count this class's quorum is a threshold on. Reading
        // the count separately would let a concurrent relearn produce a
        // confirmation shortfall the eligibility decision never saw.
        let authorization = self
            .field_verdicts()
            .preflight_authorization(key, generation, now)
            .ok_or(FIELD_PREFLIGHT_NOT_ELIGIBLE)?;
        // This class's own quorum, against the SAME acknowledged count. A
        // verdict that is not eligible at all reports `not_eligible`; one that
        // is eligible but short of a higher quorum reports `below_quorum`.
        if authorization.confirmations < class.required_quorum() {
            return Err(FIELD_PREFLIGHT_BELOW_QUORUM);
        }
        // The CONTENT gate: a prefix-impacting rewrite additionally needs the
        // operator to have named this target in `[fidelity]`. Last of the
        // gates deliberately -- an opted-in target with no confirmed verdict
        // must still report the verdict gate, so the opt-in cannot be read as
        // the only thing standing between a target and a content rewrite.
        if class.requires_target_opt_in() && !self.prefix_impact_opted_in(target) {
            return Err(FIELD_PREFLIGHT_NO_TARGET_OPT_IN);
        }
        Ok(authorization)
    }

    /// Whether the operator opted `target` into prefix-impacting pre-flight
    /// through `[fidelity] prefix_impact_opt_in`.
    ///
    /// POSITIVE-ONLY MEMBERSHIP, which is what distinguishes this from
    /// `[capability.overrides]` resolution: that surface has two verdicts
    /// (`unsupported` / `force_supported`) and therefore needs a precedence rule
    /// deciding which tier wins. This list has one verdict -- listed, or not --
    /// so a target is opted in when EITHER tier matches and there is nothing for
    /// a precedence rule to arbitrate. What is shared is the target-spec
    /// GRAMMAR, read through the same splitter
    /// ([`crate::override_registry::split_target_spec`]): a
    /// `"provider:nickname"` entry names exactly that model, and a bare
    /// `"provider"` entry names every model dispatched through that provider
    /// entry. A model-scoped entry for a DIFFERENT model on the same provider
    /// matches neither tier, so it opts this target in not at all.
    ///
    /// No second grammar and no second store: the entries are plain config
    /// strings validated at load time against the provider/model directory,
    /// and this is a pure read over them.
    fn prefix_impact_opted_in(&self, target: &DispatchTarget) -> bool {
        let nickname = target.nickname.as_deref().unwrap_or("");
        self.config
            .fidelity
            .prefix_impact_opt_in
            .iter()
            .any(|spec| {
                let (provider, model) = crate::override_registry::split_target_spec(spec);
                provider == target.provider_name
                    && match model {
                        Some(model) => model == nickname,
                        None => true,
                    }
            })
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

/// The inputs the VERDICT gates read, grouped so the call site is one argument
/// rather than six positional ones.
///
/// A struct rather than a long parameter list because five of the six are
/// borrowed or `Copy` scalars that would transpose silently at the call site --
/// `provider_kind` and a state key are both `&str`, and `generation` is a bare
/// `u64` beside a `TransformClass`.
struct GateInputs<'g> {
    target: &'g DispatchTarget,
    key: &'g FieldVerdictKey,
    provider_kind: &'g str,
    class: TransformClass,
    generation: u64,
    now: Instant,
}

/// The decision for a row this request RESTORED as its canary.
///
/// Carries the authorization record even though `acted` is false, and the
/// distinction is exactly the point: a canary is not a refusal. The
/// authorization DID permit an action here -- the planner spent it on a
/// re-verification instead of a rewrite -- so the provenance that permitted it
/// is reportable, and an operator reading a restored row needs to know which
/// evidence the identity under test rests on.
fn restored_decision<'a>(
    target: &DispatchTarget,
    row: FieldRepairRow,
    canary: Box<CanaryPlan<'a>>,
    authorization: PreflightAuthorization,
) -> RowDecision<'a> {
    let mut record = unchanged_record(
        target,
        Some(row.path),
        Some(row.class),
        FIELD_PREFLIGHT_CANARY_RESTORED,
    );
    record.authorization = Some(authorization_record(authorization));
    RowDecision {
        record,
        canary: Some(canary),
        accounting: None,
    }
}

/// A record for a row (or a whole request) the planner did not act on.
///
/// A free function rather than a method: it reads nothing from the router, so a
/// `&self` receiver would suggest the record depends on router state when the
/// only inputs are the target's sanitized key and the row's own facts.
///
/// Carries NO authorization record, and that is the contract: a refusal never
/// held one, so filling the field would attribute permission to a decision that
/// had none.
fn unchanged_record(
    target: &DispatchTarget,
    field_path: Option<&'static str>,
    class: Option<TransformClass>,
    reason: &'static str,
) -> FieldPreflight {
    FieldPreflight {
        acted: false,
        state_key: sanitize_for_log(&target.state_key),
        field_path,
        transform_class: class.map(TransformClass::as_str),
        reason,
        authorization: None,
    }
}

// The authorization-provenance rendering used above and by `emit_field_preflight`
// below lives in a sibling file to keep this one under the size ceiling. It
// compiles into THIS module via `include!`, so no call site's path changes.
include!("field_preflight_provenance.rs");

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
    let class = Some(TransformClass::Envelope.as_str());
    match apply_transform(original_req, surface) {
        Some(planned) => (
            planned,
            FieldPreflight {
                acted: true,
                state_key: sanitize_for_log(state_key),
                field_path: Some(path),
                transform_class: class,
                reason: FIELD_PREFLIGHT_ACTION_DROP,
                // This driver starts AFTER every admission check, so it holds no
                // authorization to report -- it exercises the transform, not the
                // gates that authorize one.
                authorization: None,
            },
        ),
        None => (
            original_req.clone(),
            FieldPreflight {
                acted: false,
                state_key: sanitize_for_log(state_key),
                field_path: Some(path),
                transform_class: class,
                reason: FIELD_PREFLIGHT_AMBIGUOUS_MUTATION,
                authorization: None,
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
///   the case an operator needs. The per-decision records are RETAINED in full
///   (see [`super::DispatchMeta::field_preflight`]); only the routine ones are
///   quiet.
/// - Exactly ONE request-level WARN, and only when at least one decision
///   actually acted. A request that rewrote a client's request before
///   dispatch is the reportable event; a request that changed nothing is not.
///   One line per REQUEST rather than per acting decision, matching the
///   aggregate-diagnostic contract the rest of this surface uses.
///
/// The WARN's counts are DECISIONS, not targets: one target contributes one
/// decision per closed-table row it considered, so a target-named count would
/// overstate the chain whenever a request carries rows of two classes. The line
/// NAMES the highest-impact acting decision (prefix-impacting over envelope),
/// because one line has to stand for the whole request and what an operator
/// needs from it is the worst thing that happened; among equal-impact decisions
/// it names the FIRST planned (see [`warn_headline`]). Naming one decision never
/// narrows the counts, which always describe every decision recorded.
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
///
/// # The authorization provenance, on both tiers
///
/// A decision whose authorization permitted something carries the provenance
/// that permitted it (see [`super::FieldPreflightAuthorizationRecord`]), and
/// both tiers render it: the DEBUG line for the decision it belongs to, and the
/// WARN for the headline decision the line names. That pairing is what makes the
/// two correlate -- an operator reading a WARN and scanning the DEBUG lines for
/// the `state_key` it named finds the same phase, source, confirmation count,
/// and canary posture on the decision the WARN stood for.
///
/// A refusal has no authorization, so its DEBUG line renders the absent tokens
/// explicitly rather than omitting the fields: a line whose field set varies by
/// outcome is one an operator cannot query uniformly, and an absent field reads
/// as an unavailable one rather than as an inapplicable one.
pub(super) fn emit_field_preflight(meta: &super::DispatchMeta) {
    for record in &meta.field_preflight {
        // Rendered ONCE per record, before the severity split, so the acting and
        // non-acting tiers cannot render the same provenance two ways.
        let provenance = RenderedAuthorization::of(record.authorization);
        if record.acted {
            tracing::debug!(
                event = FIELD_PREFLIGHT_EVENT,
                action = FIELD_PREFLIGHT_ACTION_DROP,
                acted = true,
                state_key = %sanitize_for_log(&record.state_key),
                field_path = record.field_path.unwrap_or(FIELD_PATH_NONE),
                transform_class = record.transform_class.unwrap_or(TRANSFORM_CLASS_NONE),
                reason = record.reason,
                provenance_phase = provenance.phase,
                provenance_source = provenance.source,
                confirmations = provenance.confirmations,
                canary = provenance.canary,
                canary_last_outcome = provenance.canary_last_outcome,
                "envelope-field pre-flight decision",
            );
        } else {
            // No `action`: nothing was done, so there is no action to name.
            tracing::debug!(
                event = FIELD_PREFLIGHT_EVENT,
                acted = false,
                state_key = %sanitize_for_log(&record.state_key),
                field_path = record.field_path.unwrap_or(FIELD_PATH_NONE),
                transform_class = record.transform_class.unwrap_or(TRANSFORM_CLASS_NONE),
                reason = record.reason,
                provenance_phase = provenance.phase,
                provenance_source = provenance.source,
                confirmations = provenance.confirmations,
                canary = provenance.canary,
                canary_last_outcome = provenance.canary_last_outcome,
                "envelope-field pre-flight decision",
            );
        }
    }
    let acted: Vec<&super::FieldPreflight> =
        meta.field_preflight.iter().filter(|r| r.acted).collect();
    if acted.is_empty() {
        // Nothing acted: this request rewrote nothing, so it earns no
        // request-level WARN at all.
        return;
    }
    // The line names the HIGHEST-IMPACT acting decision, not the first one
    // planned. One WARN has to stand for the whole request, and what an operator
    // needs from it is the worst thing that happened: a request that rewrote a
    // cache prefix AND an envelope field is a prefix-impacting event, and
    // reporting the envelope row because it came first in the table would
    // understate it -- a wrong prefix verdict degrades every later request on the
    // lane, while a wrong envelope verdict costs one field. The AGGREGATE counts
    // below still describe every decision, so nothing is hidden by the choice of
    // which one to name.
    let headline = warn_headline(&acted).expect("the non-empty check above guarantees one");
    // The HEADLINE's own provenance, carried through from the authorization that
    // permitted the rewrite this line names. Read off the selected headline
    // rather than aggregated across the acting set: the four values describe ONE
    // identity's evidence, and an aggregate of two identities' phases or canary
    // postures would be a fact about neither.
    let provenance = RenderedAuthorization::of(headline.authorization);
    tracing::warn!(
        event = FIELD_PREFLIGHT_EVENT,
        action = FIELD_PREFLIGHT_ACTION_DROP,
        state_key = %sanitize_for_log(&headline.state_key),
        field_path = headline.field_path.unwrap_or(FIELD_PATH_NONE),
        transform_class = headline.transform_class.unwrap_or(TRANSFORM_CLASS_NONE),
        provenance_phase = provenance.phase,
        provenance_source = provenance.source,
        confirmations = provenance.confirmations,
        canary = provenance.canary,
        canary_last_outcome = provenance.canary_last_outcome,
        decisions_acted = acted.len(),
        decisions_planned = meta.field_preflight.len(),
        "{FIELD_PREFLIGHT_WARN_MESSAGE}",
    );
}

/// The acting decision the request WARN names: the highest-impact one, and among
/// equals the FIRST the walk planned.
///
/// Extracted so the tie rule is stated and testable in one place, because ties
/// are ORDINARY here: a two-seat chain whose targets both act on the same class
/// produces one on every such request.
///
/// `Iterator::max_by_key` is documented to return the LAST maximum, so the
/// previous call site was deterministic -- but it named the last acting decision
/// of the winning class, which is the wrong end. The DEBUG tier emits in planning
/// order, so an operator reading a WARN and then scanning the DEBUG lines for the
/// `state_key` it named should land on the FIRST one; naming the last made that
/// correlation silently off by however many equal-impact decisions preceded it.
/// This returns the first instead.
///
/// `None` only for an empty slice; the caller checks that separately so it can
/// skip the WARN entirely rather than emit a line about nothing.
fn warn_headline<'r>(acted: &[&'r super::FieldPreflight]) -> Option<&'r super::FieldPreflight> {
    acted
        .iter()
        .copied()
        // `>` rather than `>=`: a later decision of EQUAL rank does not displace
        // the one already chosen, which is what makes first-acted-wins hold.
        .reduce(|chosen, candidate| {
            if warn_impact_rank(candidate.transform_class)
                > warn_impact_rank(chosen.transform_class)
            {
                candidate
            } else {
                chosen
            }
        })
}

/// Operator IMPACT rank of an acting decision's transform class: higher is
/// worse, and the request WARN names the worst.
///
/// Keyed on the class token rather than on a method of [`TransformClass`]
/// because the records that reach the emitter carry the token, not the enum --
/// `DispatchMeta` is a public, closed-token surface. An unrecognized token ranks
/// LOWEST rather than panicking or ranking highest: a diagnostic must not become
/// the thing that fails, and a new class added without revisiting this function
/// should under-report rather than silently outrank a prefix rewrite. The
/// `transform_class` round-trip is asserted in the tests, so a token that
/// stopped matching is caught there rather than degrading quietly.
fn warn_impact_rank(transform_class: Option<&'static str>) -> u8 {
    match transform_class {
        Some(token) if token == TransformClass::PrefixImpacting.as_str() => 2,
        Some(token) if token == TransformClass::Envelope.as_str() => 1,
        _ => 0,
    }
}

/// Rendered `field_path` for a decision that considered no field. A literal
/// rather than an empty string so the line is unambiguous to a reader.
const FIELD_PATH_NONE: &str = "none";

/// Rendered `transform_class` for a decision that considered no row, and so
/// has no class. Mirrors [`FIELD_PATH_NONE`].
const TRANSFORM_CLASS_NONE: &str = "none";

/// Message of the single request-level pre-flight WARN. Stable and greppable,
/// and named so a test can assert on it without restating the string.
pub(super) const FIELD_PREFLIGHT_WARN_MESSAGE: &str =
    "envelope-field pre-flight rewrote a request before dispatch";

#[cfg(test)]
#[path = "field_preflight_tests.rs"]
mod tests;
