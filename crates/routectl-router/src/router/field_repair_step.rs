//! The repair steps one [`FieldRepairPlan`] has applied and not yet settled,
//! and the plan that owns them.
//!
//! A plan admits every closed-table row its attempt carries, then repairs
//! whichever row a rejection names. Each repair becomes a [`RepairStep`]: the
//! row, the edit it made, what the rejection refused and how confidently it
//! was attributed, the status that rejection carried, the request's repair
//! record, and the single-flight guard the step settles through. Edits
//! accumulate on the attempt request in place and the walk re-dispatches that
//! request; the steps exist for settlement and are never re-applied.
//!
//! Each step settles on its own evidence: [`FieldRepairPlan::commit_pending`]
//! commits every pending step with that step's own tier, value and status,
//! and every other exit releases each step's guard by drop. Two things prove
//! the pending steps: a success of the repaired request, and a repaired
//! retry the upstream refuses for a DIFFERENT field
//! ([`FieldRepairPlan::moved_on_by`]), since an upstream that moved on to
//! another field accepted the fields it passed.
//!
//! A field this target already committed is never repaired again; one still
//! pending is not unless its row says so ([`rerepairs_same_field`], false for
//! every row). For a row that does, a same-field refusal says the value the
//! pending step wrote was refused too, so the new step SUPERSEDES that step:
//! it takes over the pending step's guard (the field's single-flight claim is
//! still held by it, so claiming again would refuse) and the superseded step
//! settles as dropped, never committing on its own, so the repair path holds
//! one pending step per field. The type itself keeps no one-step-per-field
//! invariant, so a later consumer that retains each refused value beside the
//! one that succeeded can hold two.

use std::borrow::Cow;
use std::time::Instant;

use routectl_core::capability::SignalTier;
use routectl_core::failure_class::FailureClass;
use routectl_core::wire_check::{EditKind, IngressDialect};
use routectl_core::{ChatRequest, Error};

use crate::field_verdict::{FieldRepairGuard, FieldVerdictKey, FieldVerdictRegistry};
use crate::field_verdict_value::VerdictValue;

use super::super::field_verdict_observability::ChainStepTally;
use super::super::rejection_parser::{
    FieldIdentity, NamedRejection, RejectionKind, RejectionValue, localized_field,
};
use super::super::repair_budget::RepairBudget;
use super::super::{CapabilityClearedEvent, CapabilityLearnEvent, DispatchMeta};
#[cfg(test)]
use super::rejected_field;
use super::{
    FIELD_ACTION_CEILING_FILL_REPAIR, FIELD_ACTION_CEILING_REPAIR, FIELD_ACTION_DROP_REPAIR,
    FIELD_ACTION_LOSSY_DROP_REPAIR, FIELD_ACTION_REWRITE_REPAIR, FieldRepairHandle, FieldRepairRow,
    FieldRepairTable, FieldTransform,
};

/// Whether `row` may be repaired again while an earlier step on the same
/// field is still pending. False for every row: no row has a second repair
/// that differs from its first.
#[cfg_attr(not(test), allow(clippy::missing_const_for_fn))]
pub(in crate::router) fn rerepairs_same_field(
    #[cfg_attr(not(test), allow(unused_variables))] row: &FieldRepairRow,
) -> bool {
    #[cfg(test)]
    if rerepair_opt_in::covers(row) {
        return true;
    }
    false
}

/// A test build's scoped opt-in of one row into [`rerepairs_same_field`].
#[cfg(test)]
#[path = "field_repair_rerepair_test_support.rs"]
pub(in crate::router) mod rerepair_opt_in;

/// The admitted L0 repair for one attempt: EVERY closed-table row the attempt
/// carries, each with the two-phase guard whose settlement the dispatch arm
/// owes, plus the steps the arm has repaired and not yet settled.
///
/// One candidate per present row rather than only the first, because the
/// rejection decides WHICH row is repaired and the upstream can name any of
/// them: an admission holding only the first present row starves a rejection
/// naming a later one -- the arm would find the plan's row unnamed, leave the
/// rejection on its ordinary terminal path, and the repairable field would
/// never be repaired on that lane.
///
/// The FIRST repair releases every other candidate's guard (see
/// [`Self::apply_named`]), so a sibling request that can settle those rows is
/// not starved while this walk retries. A later step on a released row claims
/// its guard again through the same admission, which is why the plan keeps
/// the admission's inputs.
///
/// Holding this is the PROVISIONAL phase -- nothing is persisted while it
/// lives. The dispatch arm settles each step exactly once:
/// [`Self::commit_pending`] when the repaired retry succeeded or was refused
/// for a different field, and release-by-drop on every other exit (a
/// same-field rejection, an unrelated error, a fallback hop, a client
/// disconnect), which learns nothing and strands no slot. A plan that never
/// applied a repair settles through [`Self::settle_success`] when its
/// attempt succeeded outright.
pub(in crate::router) struct FieldRepairPlan<'a> {
    /// The table the candidates were admitted from, and so the only rows a
    /// rejection may name for this plan.
    #[cfg(test)]
    table: FieldRepairTable,
    candidates: Vec<FieldRepairCandidate<'a>>,
    /// The repairs applied and not yet settled, in the order they were made.
    pending: Vec<RepairStep<'a>>,
    /// The fields whose steps already committed on this target. A committed
    /// field is never repaired again in the request, so this outlives the
    /// pending list it drains from.
    committed: Vec<FieldIdentity>,
    /// Whether any repair was applied on this target. Once one was, the
    /// request that reaches the upstream is not the unrepaired one, so its
    /// success can never prove the carried fields accepted.
    applied: bool,
    /// The wire dialect the attempt shares with its client, for declaring the
    /// repair's edit; `None` on a translation lane.
    wire: Option<IngressDialect>,
    /// What a later step needs to claim a released row's guard again. `None`
    /// for a non-settling plan, which holds no guard and claims none.
    reclaim: Option<GuardAdmission<'a>>,
    /// Where this plan counts how its steps ended; `None` for a non-settling
    /// plan, whose steps can prove nothing it keeps.
    exits: Option<&'a ChainStepTally>,
}

/// The inputs the plan's admission claimed each guard with.
pub(super) struct GuardAdmission<'a> {
    pub(super) registry: &'a FieldVerdictRegistry,
    pub(super) base_url: Cow<'a, str>,
    pub(super) generation: u64,
    /// The tally the plan's step exits are counted on.
    pub(super) exits: &'a ChainStepTally,
}

/// One admitted row, plus the guard whose settlement this row owes and the
/// identity that guard was claimed for.
pub(super) struct FieldRepairCandidate<'a> {
    row: FieldRepairRow,
    /// `Some` exactly when `guard` was claimed at admission, and kept after
    /// the guard is released so a later step can claim it again.
    key: Option<FieldVerdictKey>,
    /// `Some` only for a SETTLING walk. `None` is the non-settling plan: it
    /// repairs for the answer its caller returns and can settle nothing, so it
    /// holds no single-flight claim (leaving it available to a sibling request
    /// that CAN settle) and consults no resident verdict. Both settlements are
    /// no-ops on it -- the type carries the capability rather than every
    /// settlement site re-checking a mode flag.
    guard: Option<FieldRepairGuard<'a>>,
}

impl<'a> FieldRepairCandidate<'a> {
    /// A row a non-settling walk repairs for its answer alone.
    pub(super) const fn unsettled(row: FieldRepairRow) -> Self {
        Self {
            row,
            key: None,
            guard: None,
        }
    }

    /// A row whose guard the admission claimed for `key`.
    pub(super) const fn admitted(
        row: FieldRepairRow,
        key: FieldVerdictKey,
        guard: FieldRepairGuard<'a>,
    ) -> Self {
        Self {
            row,
            key: Some(key),
            guard: Some(guard),
        }
    }
}

/// One applied repair awaiting settlement.
struct RepairStep<'a> {
    row: FieldRepairRow,
    edit: FieldEdit,
    /// What the rejection that drove the repair refused. A refused FIELD may
    /// commit; a refused value commits only through a rewrite or a ceiling.
    kind: RejectionKind,
    /// How confidently that rejection was attributed to the row. The commit
    /// records it, so an inferred attribution persists without acting until an
    /// independent request corroborates it.
    tier: SignalTier,
    /// Whether the applied ceiling lowered routectl's fill rather than a
    /// client value.
    fill_clamped: bool,
    /// The request's repair record for this step; `None` until the walk
    /// records it.
    handle: Option<FieldRepairHandle>,
    /// The status of the rejection this step answered.
    reject_status: u16,
    guard: Option<FieldRepairGuard<'a>>,
}

/// A committed step: the repair record it settles and the verdict row it
/// persisted, if any.
pub(in crate::router) struct SettledStep {
    pub(in crate::router) handle: Option<FieldRepairHandle>,
    pub(in crate::router) learned: Option<CapabilityLearnEvent>,
}

/// The concrete edit one repair makes: a row's transform resolved against the
/// rejection that named it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FieldEdit {
    Drop,
    Rewrite(&'static str),
    Ceiling(u32),
}

impl FieldEdit {
    /// The edit `transform` makes for `named`, or `None` when it makes none:
    /// a rewrite whose rejection named no shape, a ceiling whose rejection
    /// named no ceiling, and a ranked rewrite, whose replacement level no
    /// rejection names yet.
    pub(super) const fn for_rejection(
        transform: FieldTransform,
        named: &NamedRejection,
    ) -> Option<Self> {
        match (transform, named.value) {
            (FieldTransform::Drop, _) => Some(Self::Drop),
            (FieldTransform::Rewrite { .. }, Some(RejectionValue::Shape(value))) => {
                Some(Self::Rewrite(value))
            }
            (FieldTransform::Ceiling, Some(RejectionValue::Ceiling(ceiling))) => {
                Some(Self::Ceiling(ceiling))
            }
            (
                FieldTransform::Rewrite { .. },
                Some(RejectionValue::Ceiling(_) | RejectionValue::Levels(_)) | None,
            )
            | (
                FieldTransform::Ceiling,
                Some(RejectionValue::Shape(_) | RejectionValue::Levels(_)) | None,
            )
            | (FieldTransform::RankedRewrite { .. }, _) => None,
        }
    }

    fn apply_to(self, surface: super::FieldSurface, req: &mut ChatRequest) -> bool {
        match self {
            Self::Drop => surface.drop_from(req),
            Self::Rewrite(value) => surface.rewrite_in(req, value),
            Self::Ceiling(ceiling) => surface.clamp_to(req, ceiling),
        }
    }

    pub(super) const fn declared_kind(self) -> EditKind {
        match self {
            Self::Drop => EditKind::Remove,
            Self::Rewrite(_) | Self::Ceiling(_) => EditKind::Rewrite,
        }
    }

    /// The value a verdict committed for this edit applies: none for a drop.
    const fn verdict_value(self) -> Option<VerdictValue> {
        match self {
            Self::Drop => None,
            Self::Rewrite(value) => Some(VerdictValue::Shape(value)),
            Self::Ceiling(ceiling) => Some(VerdictValue::Ceiling(ceiling)),
        }
    }
}

/// The edit a repair makes and the attribution of the rejection behind it.
#[derive(Clone, Copy)]
struct Repair {
    edit: FieldEdit,
    kind: RejectionKind,
    tier: SignalTier,
}

impl RepairStep<'_> {
    /// The value this repair wrote, or `None` for a drop.
    const fn written_value(&self) -> Option<VerdictValue> {
        self.edit.verdict_value()
    }

    /// The closed-set WARN action token for this repair.
    fn repair_action(&self) -> &'static str {
        match self.edit {
            FieldEdit::Rewrite(_) => FIELD_ACTION_REWRITE_REPAIR,
            FieldEdit::Ceiling(_) if self.fill_clamped => FIELD_ACTION_CEILING_FILL_REPAIR,
            FieldEdit::Ceiling(_) => FIELD_ACTION_CEILING_REPAIR,
            FieldEdit::Drop if self.kind == RejectionKind::InvalidValue => {
                FIELD_ACTION_LOSSY_DROP_REPAIR
            }
            FieldEdit::Drop => FIELD_ACTION_DROP_REPAIR,
        }
    }

    /// Whether this repair may commit a verdict once its retry succeeds.
    fn repair_teaches(&self) -> bool {
        self.written_value().is_some() || self.kind == RejectionKind::UnsupportedField
    }

    /// Persist this step's verdict, or release its guard when the repair
    /// teaches nothing or the plan cannot settle.
    ///
    /// Only when the rejection refused the FIELD, the repair rewrote it to
    /// the one accepted value the rejection named, or it lowered a token limit
    /// to the ceiling the rejection named. A refused value repaired by a DROP
    /// says the lane accepts the field, so a verdict dropping it would strip
    /// every valid value from every later request on the lane.
    fn commit(self, request_features: &[String], now: Instant) -> SettledStep {
        let learned = if self.repair_teaches() {
            let value = self.written_value();
            self.guard.and_then(|guard| {
                guard.commit(
                    self.tier,
                    value,
                    self.reject_status,
                    request_features.to_vec(),
                    now,
                )
            })
        } else {
            None
        };
        SettledStep {
            handle: self.handle,
            learned,
        }
    }
}

impl<'a> FieldRepairPlan<'a> {
    /// A plan over `candidates`, none of them repaired yet.
    pub(super) const fn admitted(
        #[cfg_attr(not(test), allow(unused_variables))] table: FieldRepairTable,
        candidates: Vec<FieldRepairCandidate<'a>>,
        wire: Option<IngressDialect>,
        reclaim: Option<GuardAdmission<'a>>,
    ) -> Self {
        Self {
            #[cfg(test)]
            table,
            candidates,
            pending: Vec::new(),
            committed: Vec::new(),
            applied: false,
            wire,
            exits: match &reclaim {
                Some(admission) => Some(admission.exits),
                None => None,
            },
            reclaim,
        }
    }

    /// Whether any repair is awaiting settlement.
    #[cfg(test)]
    pub(in crate::router) const fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether this plan applied any repair, settled or not.
    pub(in crate::router) const fn has_applied(&self) -> bool {
        self.applied
    }

    /// Whether `rejection` of a repaired retry proves every pending step: its
    /// NATIVE class can name a field, it names one self-identifyingly, and
    /// that field is none a pending step repaired. The upstream then read
    /// past each pending field to refuse another, so each was accepted.
    ///
    /// An operator remap of the class is not read: it says how a status is
    /// routed, not what the upstream refused. The field is compared by
    /// identity, so a refused rewrite value (`thinking.type.<token>`) reads
    /// as the field it rewrote. The caller still owes the check that the body
    /// that went out carried the named path.
    pub(in crate::router) fn moved_on_by(
        &self,
        native_class: &FailureClass,
        rejection: &Error,
        provider_kind: &str,
    ) -> bool {
        if self.pending.is_empty() || !super::class_can_name_a_field(native_class, provider_kind) {
            return false;
        }
        let Some(status) = super::super::class_observe::upstream_facts(rejection).status else {
            return false;
        };
        localized_field(provider_kind, status, rejection.parsed_body()).is_some_and(|field| {
            field.tier == SignalTier::SelfIdentifying
                && self
                    .pending
                    .iter()
                    .all(|step| FieldIdentity::of_row(&step.row) != field.identity)
        })
    }

    /// The closed-table path the latest repair acted on, or `None` when this
    /// plan has not repaired anything. A code-authored literal, never
    /// upstream text, so it is safe in an operator-facing field.
    #[cfg(test)]
    pub(in crate::router) fn repaired_path(&self) -> Option<&'static str> {
        self.pending.last().map(|step| step.row.path)
    }

    /// The latest repair's path and WARN action token, for its record.
    pub(super) fn latest_record(&self) -> Option<(&'static str, &'static str)> {
        self.pending
            .last()
            .map(|step| (step.row.path, step.repair_action()))
    }

    /// Bind the latest repair to the request record `handle` names.
    pub(super) fn stamp_latest(&mut self, handle: FieldRepairHandle) {
        if let Some(step) = self.pending.last_mut() {
            step.handle = Some(handle);
        }
    }

    /// Every admitted candidate's path, in table order.
    #[cfg(test)]
    pub(in crate::router) fn candidate_paths(&self) -> Vec<&'static str> {
        self.candidates
            .iter()
            .map(|candidate| candidate.row.path)
            .collect()
    }

    /// Every pending step's path, oldest first.
    #[cfg(test)]
    pub(in crate::router) fn pending_paths(&self) -> Vec<&'static str> {
        self.pending.iter().map(|step| step.row.path).collect()
    }

    /// Perform the repair for whichever admitted row `err` names: apply that
    /// row's transform to the attempt request, re-stamp the calibration estimate,
    /// CHARGE the shared per-request budget, and RELEASE every other
    /// candidate's guard -- all or nothing.
    ///
    /// The rejection is matched against every admitted row rather than against
    /// one chosen at admission time: the upstream names the field, so the row
    /// it names is the row to repair. Matching is on the SURFACE (see
    /// [`Router::rejection_names_field_surface`](super::super::Router::rejection_names_field_surface)).
    ///
    /// The budget draw lives here, inside the successful branch, rather than in
    /// the caller's condition chain. The drop can legitimately remove nothing
    /// (the request changed under the arm), and a draw in the condition would
    /// then have charged a request that did not repair -- a charge nothing
    /// observable can see, since the arm falls through to the ordinary error
    /// path either way. Ordering the two the other way means the budget and
    /// the mutation succeed or fail together.
    ///
    /// `Some(status)` -- repaired: the named row's field is changed, the
    /// estimate describes the new payload, one repair was charged, a step was
    /// added, and the caller must re-dispatch and later settle this plan.
    /// `None` -- nothing happened: no mutation, no charge, no guard released,
    /// and the caller must NOT mark the attempt repaired.
    #[cfg(test)]
    pub(in crate::router) fn apply(
        &mut self,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        native_class: &FailureClass,
        rejection: &Error,
        provider_kind: &str,
    ) -> Option<u16> {
        let named = rejected_field(
            self.table,
            native_class,
            rejection,
            provider_kind,
            super::super::learn_engine::SentEffort::Unknown,
        )?;
        self.apply_named(named, attempt_req, meta, budget, rejection)
    }

    /// Repair for a rejection already resolved to its closed-table row: the
    /// admitted candidate whose SURFACE that row names.
    ///
    /// A row whose field this target already committed is refused; one still
    /// pending is refused unless [`rerepairs_same_field`] holds for the row,
    /// in which case the new step supersedes the pending one on that field; a
    /// DIFFERENT field adds a step beside the earlier ones.
    pub(in crate::router) fn apply_named(
        &mut self,
        named: NamedRejection,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        rejection: &Error,
    ) -> Option<u16> {
        if self.repaired_field(named.row) {
            return None;
        }
        // WHICH row: the one the upstream named. Refusing here is
        // pre-mutation, so an unnamed rejection leaves the request and every
        // guard exactly as the caller handed them over.
        let idx = self
            .candidates
            .iter()
            .position(|candidate| candidate.row.surface == named.row.surface)?;
        let edit = FieldEdit::for_rejection(self.candidates[idx].row.transform, &named)?;
        let repair = Repair {
            edit,
            kind: named.kind,
            tier: named.tier,
        };
        self.apply_candidate(idx, repair, attempt_req, meta, budget, rejection)
    }

    /// Whether this target already repaired the field `row` names: in a
    /// committed step, always; in a pending step, unless the row re-repairs.
    fn repaired_field(&self, row: &FieldRepairRow) -> bool {
        let identity = FieldIdentity::of_row(row);
        self.committed.contains(&identity)
            || (!rerepairs_same_field(row)
                && self
                    .pending
                    .iter()
                    .any(|step| FieldIdentity::of_row(&step.row) == identity))
    }

    /// The EFFECT half of [`Self::apply_named`], for the candidate at `idx`: the
    /// presence re-check, the allowance check and the guard claim, then the
    /// edit, then the step's installation.
    ///
    /// Split from the rejection-to-candidate SELECTION above so the fail-closed
    /// branch below is reachable in a release test build: the divergent surface
    /// that manufactures the presence/drop disagreement reports itself present
    /// unconditionally, so giving it a closed-table row would make every request
    /// in a test build carry it. This is the entry point that test drives.
    fn apply_candidate(
        &mut self,
        idx: usize,
        repair: Repair,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        rejection: &Error,
    ) -> Option<u16> {
        let row = self.candidates[idx].row;
        let surface = row.surface;
        // Presence is re-read HERE rather than trusted from admission: the
        // request has been through a dispatch since, and a drop that removes
        // nothing must not consume the allowance.
        if !surface.present_in(attempt_req) {
            return None;
        }
        // Availability is CHECKED, not spent: an exhausted allowance must leave
        // the request untouched, so this precedes the mutation, while the charge
        // in `install_step` follows it.
        if !budget.check_or_mark_refusal() {
            return None;
        }
        // Claimed before the mutation: a field whose claim a sibling request
        // now holds is not repaired here, and the rejection stays terminal.
        let claim = self.step_claim(idx, &row)?;
        let edit = repair.edit;
        let fill_clamped =
            matches!(edit, FieldEdit::Ceiling(_)) && surface.clamp_writes_fill(attempt_req);
        if !edit.apply_to(surface, attempt_req) {
            // FAIL CLOSED. A rewrite or a ceiling may refuse by design; a drop
            // cannot (`present_in` held above), so one here is a divergence the
            // release path refuses without relying on the debug assertion. A
            // claimed guard releases as it drops. The LAST correct `None`.
            debug_assert!(
                edit != FieldEdit::Drop,
                "a present surface must take a drop; presence and the drop have diverged",
            );
            return None;
        }
        let mutated = MutatedCandidate {
            idx,
            repair,
            claim,
            fill_clamped,
        };
        Some(self.install_step(mutated, attempt_req, meta, budget, rejection))
    }

    /// Phase two once the pending steps are proven -- the repaired retry
    /// succeeded, or was refused for a different field: settle every pending
    /// step, oldest first, each with its own tier, written value and
    /// rejection status, and return one entry per step.
    ///
    /// Each step's commit speaks for its own row alone: the rejection named
    /// that row, and the retry that succeeded is evidence about it. A step
    /// whose repair teaches nothing (a refused value repaired by a drop)
    /// releases its guard and persists nothing, and a non-settling plan holds
    /// no guard, so neither yields a verdict row.
    ///
    /// The pending list is empty afterwards, and each committed field is
    /// remembered so the rest of the target never repairs it again.
    pub(in crate::router) fn commit_pending(
        &mut self,
        request_features: &[String],
        now: Instant,
    ) -> Vec<SettledStep> {
        self.committed.extend(
            self.pending
                .iter()
                .map(|step| FieldIdentity::of_row(&step.row)),
        );
        if let Some(exits) = self.exits {
            exits.add_committed(self.pending.len());
        }
        self.pending
            .drain(..)
            .map(|step| step.commit(request_features, now))
            .collect()
    }

    /// Phase two after the UNREPAIRED attempt succeeded: every carried field is
    /// accepted, so drop any resident verdict for each admitted row and return
    /// the cleared rows so the ledger records the clears and a warm rebuild
    /// cannot resurrect them.
    ///
    /// One event per row that actually had a resident entry removed; empty for
    /// an attempt whose rows had none, and for every non-settling plan. A
    /// non-settling walk must NOT clear: the removal would be invisible to the
    /// ledger, so the next warm rebuild would resurrect a verdict this process
    /// had already dropped.
    pub(in crate::router) fn settle_success(mut self) -> Vec<CapabilityClearedEvent> {
        self.candidates
            .iter_mut()
            .filter_map(|candidate| candidate.guard.take()?.clear())
            .collect()
    }
}

/// Every step still pending when the plan goes is dropped unproven: the same
/// field refused again, an unrelated error, a fallback to another target, or
/// the end of the request. [`FieldRepairPlan::commit_pending`] drains the
/// steps it proves first, so none is counted twice.
impl Drop for FieldRepairPlan<'_> {
    fn drop(&mut self) {
        if let Some(exits) = self.exits {
            exits.add_dropped(self.pending.len());
        }
    }
}

#[path = "field_repair_step_apply.rs"]
mod apply;
use apply::MutatedCandidate;

#[cfg(test)]
#[path = "field_repair_step_tests.rs"]
mod tests;
