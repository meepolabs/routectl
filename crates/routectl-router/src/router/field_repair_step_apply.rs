//! The two halves of [`FieldRepairPlan::apply_candidate`] around its mutation:
//! choosing the guard a new step settles through, before the request is
//! touched, and installing the step once it has been.
//!
//! The order between them is the plan's invariant: everything that may refuse
//! runs before the mutation, and nothing after it can fail.

use std::time::Instant;

use routectl_core::wire_check::EditSite;
use routectl_core::{ChatRequest, Error};

use crate::field_verdict::FieldRepairGuard;

use super::super::super::DispatchMeta;
use super::super::super::edit_declare::declare_field_edit;
use super::super::super::rejection_parser::FieldIdentity;
use super::super::super::repair_budget::RepairBudget;
use super::super::FieldSurface;
use super::{FieldEdit, FieldRepairPlan, Repair, RepairStep};

#[cfg(all(test, not(debug_assertions)))]
use {
    super::super::{FieldRepairRow, FieldRepairTable, FieldTransform},
    super::{FieldRepairCandidate, RejectionKind},
    routectl_core::capability::SignalTier,
};

/// Where a step's guard comes from.
pub(super) enum Claim<'a> {
    /// The candidate's own admission guard, or none on a non-settling plan.
    Kept,
    /// A fresh claim for a row an earlier step released, or one admitted
    /// after the plan's admission.
    Reclaimed(FieldRepairGuard<'a>),
    /// No claim: a ranked replacement level whose claim a sibling request
    /// holds, or on which a verdict acts. The refusal fully determines the
    /// edit, so the step still repairs the attempt, but it holds no guard,
    /// so it commits nothing and releases no slot it never took.
    Unheld,
}

/// A candidate whose edit has been applied to the request, and what its step
/// needs to be installed.
pub(super) struct MutatedCandidate<'a> {
    pub(super) idx: usize,
    pub(super) repair: Repair,
    pub(super) claim: Claim<'a>,
    /// Whether the applied ceiling lowered routectl's fill rather than a
    /// client value.
    pub(super) fill_clamped: bool,
}

impl<'a> FieldRepairPlan<'a> {
    /// The guard a step on candidate `idx` settles through, read before the
    /// mutation. A row the first repair released, or one admitted after the
    /// plan's admission, is claimed through the admission's own inputs.
    /// When a sibling request holds the row's claim, or a verdict now acts on
    /// it, a ranked replacement of a level a pending step on the same field
    /// wrote is [`Claim::Unheld`]; every other such row is `None`, so the
    /// rejection stays terminal.
    pub(super) fn claim(&self, idx: usize) -> Option<Claim<'a>> {
        let candidate = &self.candidates[idx];
        if candidate.guard.is_some() {
            return Some(Claim::Kept);
        }
        let (Some(admission), Some(key)) = (self.reclaim.as_ref(), candidate.key.as_ref()) else {
            return Some(Claim::Kept);
        };
        admission
            .registry
            .admit_provisional(
                key,
                &admission.base_url,
                admission.generation,
                Instant::now(),
            )
            .map(Claim::Reclaimed)
            .or_else(|| self.replaces_pending_level(idx).then_some(Claim::Unheld))
    }

    /// Whether candidate `idx` is a ranked row whose field a pending step
    /// already repaired: its level is a replacement this request wrote.
    fn replaces_pending_level(&self, idx: usize) -> bool {
        let row = &self.candidates[idx].row;
        let identity = FieldIdentity::of_row(row);
        super::super::effort::is_ranked(row)
            && self
                .pending
                .iter()
                .any(|step| FieldIdentity::of_row(&step.row) == identity)
    }

    /// Install the step for a candidate whose edit is already in
    /// `attempt_req`: charge the allowance, declare the edit, hand the step
    /// its guard, release every other candidate's guard, re-stamp the
    /// estimate, and record the step. Returns the rejection's status.
    ///
    /// This is the one place that must not have a refusal branch. The field
    /// is already changed, so reporting "no repair" here would leave the
    /// caller's walk holding a silently altered request that it believes it
    /// never repaired, which is strictly worse than either repairing or
    /// refusing. Hence the infallible return.
    pub(super) fn install_step(
        &mut self,
        mutated: MutatedCandidate<'a>,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        rejection: &Error,
    ) -> u16 {
        let MutatedCandidate {
            idx,
            repair,
            claim,
            fill_clamped,
        } = mutated;
        let row = self.candidates[idx].row;
        let mintable = repair.mintable && !matches!(claim, Claim::Unheld);
        // Charged unconditionally: `check_or_mark_refusal` held before the
        // mutation, which is what makes a refusal here impossible, so the
        // invariant is asserted rather than branched on.
        let charged = budget.draw();
        debug_assert!(
            charged,
            "check_or_mark_refusal held before the mutation, so the draw after it cannot fail",
        );
        if !fill_clamped {
            self.declare_repair(attempt_req, row.surface, repair.edit);
        }
        let guard = self.take_step_guard(idx, claim);
        super::super::super::dispatch::restamp_calibration_estimate(attempt_req, meta);
        let reject_status = super::super::super::class_observe::upstream_facts(rejection)
            .status
            .unwrap_or(0);
        self.pending.push(RepairStep {
            row,
            edit: repair.edit,
            kind: repair.kind,
            tier: repair.tier,
            fill_clamped,
            handle: None,
            reject_status,
            mintable,
            guard,
        });
        self.applied = true;
        reject_status
    }

    /// Declare the repair's edit on the attempt's wire. Not called for a
    /// clamped fill: that value is still the egress's declared default, and a
    /// second declaration at the same pointer would read as a conflict to the
    /// next rejection there and keep it from the proposers.
    fn declare_repair(
        &self,
        attempt_req: &mut ChatRequest,
        surface: FieldSurface,
        edit: FieldEdit,
    ) {
        declare_field_edit(
            attempt_req,
            self.wire,
            EditSite::FieldRepair,
            surface,
            edit.declared_kind(),
        );
    }

    /// The guard the step on candidate `idx` settles through, taken from
    /// where `claim` says it lives, then every OTHER candidate's guard
    /// released.
    ///
    /// The others are released now rather than held to settlement, which
    /// frees each unused single-flight slot for a sibling request that CAN
    /// settle that row instead of pinning it for the rest of this walk. Only
    /// the first repair finds any left to release.
    fn take_step_guard(&mut self, idx: usize, claim: Claim<'a>) -> Option<FieldRepairGuard<'a>> {
        let guard = match claim {
            Claim::Reclaimed(guard) => Some(guard),
            Claim::Kept => self.candidates[idx].guard.take(),
            Claim::Unheld => None,
        };
        for (other, candidate) in self.candidates.iter_mut().enumerate() {
            if other != idx {
                drop(candidate.guard.take());
            }
        }
        guard
    }

    /// A guardless plan over the divergent test surface: the shape that reaches
    /// `apply`'s fail-closed branch. Test-only, and deliberately not
    /// constructible any other way.
    #[cfg(all(test, not(debug_assertions)))]
    pub(in crate::router) fn divergent_for_tests() -> Self {
        Self::admitted(
            FieldRepairTable::default(),
            vec![FieldRepairCandidate::unsettled(FieldRepairRow {
                // Outside the closed table deliberately: the surface is not a
                // row (see `FieldRepairTable`), and this path exists only so the
                // candidate is well formed.
                path: "divergent.surface.for.tests",
                surface: super::super::FieldSurface::DivergentForTests,
                transform: FieldTransform::Drop,
                class: super::super::TransformClass::Envelope,
                count_tokens_validates: false,
            })],
            None,
            None,
        )
    }

    /// Drive [`Self::apply_candidate`] for this plan's only candidate, skipping
    /// the rejection-to-candidate selection.
    ///
    /// Test-only, and the ONLY way the fail-closed branch is reachable: the
    /// divergent surface has no closed-table row, so no rejection can select it.
    /// The production effect path is called directly, so a mutation to the
    /// refusal, the charge, or the mutation ordering is observable here.
    #[cfg(all(test, not(debug_assertions)))]
    pub(in crate::router) fn apply_sole_candidate_for_tests(
        &mut self,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        rejection: &Error,
    ) -> Option<u16> {
        assert_eq!(
            self.candidates.len(),
            1,
            "the divergent fixture holds exactly one candidate",
        );
        let repair = Repair {
            edit: FieldEdit::Drop,
            kind: RejectionKind::UnsupportedField,
            tier: SignalTier::SelfIdentifying,
            mintable: true,
        };
        self.apply_candidate(0, repair, attempt_req, meta, budget, rejection)
    }
}
