//! How each carry the engine holds settles on a successful attempt: the
//! reasoning-replay carry, the envelope-field carry, and the field canary;
//! and how the envelope-field carry's pending steps settle when a repaired
//! retry is refused for a different field.

use std::time::Instant;

use routectl_core::ChatRequest;

use crate::feature_keys::derive_feature_keys;

use super::super::DispatchMeta;
use super::super::field_repair::FieldRepairPlan;
use super::super::replay_repair::settle_replay_repair;
use super::{AttemptLearning, Rejection, RejectionSite};

impl AttemptLearning<'_> {
    /// A stripped repair that reached success confirms the negative
    /// (commit); a carried variant that succeeded outright proves the pair
    /// works, so any resident (lapsed) negative clears.
    ///
    /// A token count settles by drop: dropping the plan releases its slots
    /// and leaves any resident entry as it was. A count success is not the
    /// optimistic probe the two-phase learn settles, so committing or
    /// clearing off it would persist a conclusion nothing tested.
    pub(super) fn settle_replay_success(&mut self, req: &ChatRequest, meta: &mut DispatchMeta) {
        let Some(plan) = self.replay_plan.take() else {
            return;
        };
        if self.count_surface {
            drop(plan);
            if let Some(repair) = self.replay_repair {
                settle_replay_repair(meta, repair, false);
            }
            return;
        }
        if let Some(repair) = self.replay_repair {
            let features = derive_feature_keys(
                req.tools.as_deref().unwrap_or(&[]),
                req.provider_extras.as_ref(),
                req.response_format.as_ref(),
            );
            meta.learned_capabilities.extend(plan.commit(
                self.replay_reject_status,
                &features,
                Instant::now(),
            ));
            settle_replay_repair(meta, repair, true);
        } else {
            meta.cleared_capabilities.extend(plan.settle_success());
        }
    }

    /// A repaired retry that reached success confirms every pending step
    /// (commit); an UNREPAIRED success proves the field is accepted, so any
    /// resident verdict clears. A target that applied a repair never clears,
    /// even when an earlier moved-on commit left nothing pending: the request
    /// that succeeded was not the unrepaired one.
    pub(super) fn settle_field_success(&mut self, req: &ChatRequest, meta: &mut DispatchMeta) {
        let Some(mut plan) = self.field_plan.take() else {
            return;
        };
        if plan.has_applied() {
            self.commit_field_steps(&mut plan, req, meta);
        } else {
            meta.cleared_capabilities.extend(plan.settle_success());
        }
    }

    /// Commit every pending step when a repaired retry was refused for a
    /// different field the body that went out carried: the upstream read past
    /// each pending field, so each is proven even if the new field cannot be
    /// repaired. Features come from the client's request, as on success.
    pub(super) fn settle_field_moved_on(
        &mut self,
        rejection: &Rejection<'_>,
        meta: &mut DispatchMeta,
    ) {
        if RejectionSite::names_absent_path(rejection.site.as_ref()) {
            return;
        }
        let Some(mut plan) = self.field_plan.take() else {
            return;
        };
        let provider_kind = rejection.target.provider_kind.unwrap_or("");
        if plan.moved_on_by(rejection.native_class, rejection.err, provider_kind) {
            self.commit_field_steps(&mut plan, rejection.client_req, meta);
        }
        self.field_plan = Some(plan);
    }

    /// Settle every pending step of `plan` and record each outcome on its
    /// repair record.
    fn commit_field_steps(
        &self,
        plan: &mut FieldRepairPlan<'_>,
        req: &ChatRequest,
        meta: &mut DispatchMeta,
    ) {
        let features =
            super::super::field_repair::request_feature_keys(self.router.field_repair_table(), req);
        // Each `learned` claim is derived from what that step's commit
        // actually PERSISTED, never asserted alongside it: a non-settling
        // plan commits nothing, and a summary that claimed otherwise would
        // be a false persistence claim.
        for step in plan.commit_pending(&features, Instant::now()) {
            let persisted = step.learned.is_some();
            meta.learned_capabilities.extend(step.learned);
            if let Some(repair) = step.handle {
                self.router
                    .note_field_repair_succeeded(meta, repair, persisted);
            }
        }
    }

    /// Two canary success shapes mean opposite things: an UNREPAIRED success
    /// proves the verdict wrong, while a success that arrived only after the
    /// canary's same-field repaired retry re-confirms it. A refused-VALUE
    /// rejection already settled the canary as inconclusive in the repair
    /// branch, so no plan is left here and its lossy repair only reports
    /// success.
    pub(super) fn settle_canary_success(&mut self, req: &ChatRequest, meta: &mut DispatchMeta) {
        if let Some(repair) = self.canary_lossy_repair {
            self.router.note_field_repair_succeeded(meta, repair, false);
        }
        let Some(plan) = std::mem::take(&mut self.preflight).into_canary() else {
            return;
        };
        if self.canary_repair_attempted {
            let features = super::super::field_repair::request_feature_keys(
                self.router.field_repair_table(),
                req,
            );
            meta.learned_capabilities.extend(plan.settle_confirmed(
                self.router.field_verdicts(),
                self.canary_reject_status,
                features,
                Instant::now(),
            ));
        } else {
            meta.cleared_capabilities
                .extend(plan.settle_disproved(self.router.field_verdicts()));
        }
    }
}
