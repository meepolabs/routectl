//! How each carry the engine holds settles on a successful attempt: the
//! reasoning-replay carry, the envelope-field carry, and the field canary.

use std::time::Instant;

use routectl_core::ChatRequest;

use crate::feature_keys::derive_feature_keys;

use super::super::DispatchMeta;
use super::super::replay_repair::settle_replay_repair;
use super::AttemptLearning;

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

    /// A repaired retry that reached success confirms the verdict (commit);
    /// an UNREPAIRED success proves the field is accepted, so any resident
    /// verdict clears.
    pub(super) fn settle_field_success(&mut self, req: &ChatRequest, meta: &mut DispatchMeta) {
        let Some(plan) = self.field_plan.take() else {
            return;
        };
        if let Some(repair) = self.reactive_repair {
            let features = super::super::field_repair::request_feature_keys(
                self.router.field_repair_table(),
                req,
            );
            // The `learned` claim is derived from what the commit actually
            // PERSISTED, never asserted alongside it: a non-settling plan
            // commits nothing, and a summary that claimed otherwise would be a
            // false persistence claim.
            let learned = plan.commit(self.field_reject_status, features, Instant::now());
            let persisted = learned.is_some();
            meta.learned_capabilities.extend(learned);
            self.router
                .note_field_repair_succeeded(meta, repair, persisted);
        } else {
            meta.cleared_capabilities.extend(plan.settle_success());
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
