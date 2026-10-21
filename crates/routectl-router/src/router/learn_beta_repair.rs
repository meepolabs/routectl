//! Child module of `learn_engine.rs`: the `RepairSink` the arbitration
//! applies repairs through, and the beta repair's effect on a live attempt.
//!
//! The beta repair edits nothing but the attempt's withheld set. Every
//! anthropic-vocabulary egress already drops a withheld client flag before
//! the wire, keeps its floor, and re-adds the body-implied set after the
//! withhold, so those guards keep holding on the repaired attempt.

use std::sync::Arc;

use routectl_core::{ChatRequest, Error, sanitize_for_log};

use super::super::rejection_parser::NamedRejection;
use super::super::repair_budget::RepairBudget;
use super::super::{DispatchMeta, DispatchTarget};
use super::{AttemptLearning, RepairSink};

/// The repair effects against one live attempt: the target's learner state
/// plus the request-scoped budget and the attempt body it may edit.
pub(super) struct EngineSink<'s, 'a> {
    pub(super) learning: &'s mut AttemptLearning<'a>,
    pub(super) err: &'s Error,
    pub(super) target: &'s DispatchTarget,
    pub(super) attempt_req: &'s mut ChatRequest,
    pub(super) meta: &'s mut DispatchMeta,
    pub(super) budget: &'s mut RepairBudget,
}

impl RepairSink for EngineSink<'_, '_> {
    fn strip_replay(&mut self) -> bool {
        self.learning.try_replay_repair(
            self.err,
            self.target,
            self.attempt_req,
            self.meta,
            self.budget,
        )
    }

    fn repair_field(&mut self, named: NamedRejection) -> bool {
        self.learning.try_field_repair(
            named,
            self.err,
            self.target,
            self.attempt_req,
            self.meta,
            self.budget,
        )
    }

    fn canary_retry(&mut self, named: NamedRejection) -> bool {
        self.learning.try_canary_repair(
            named,
            self.err,
            self.target,
            self.attempt_req,
            self.meta,
            self.budget,
        )
    }

    fn withhold_betas(&mut self, flags: Vec<String>) -> bool {
        self.learning
            .try_beta_repair(flags, self.target, self.attempt_req, self.budget)
    }
}

impl AttemptLearning<'_> {
    /// Withhold `flags` from `attempt_req` and re-dispatch, at most once per
    /// target. Refused, charging nothing, when this target already repaired
    /// a beta rejection; `budget.draw()` is the last condition.
    ///
    /// The repaired flags are kept for the rest of the chain iteration, so a
    /// same-target retry that succeeds after the repaired attempt failed
    /// still settles them: that success was served without them too.
    pub(super) fn try_beta_repair(
        &mut self,
        flags: Vec<String>,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
        budget: &mut RepairBudget,
    ) -> bool {
        if !self.repaired_betas.is_empty() {
            return false;
        }
        if !budget.draw() {
            return false;
        }
        let mut next: Vec<String> = attempt_req.routectl_internal.withheld_betas.to_vec();
        next.extend(flags.iter().cloned());
        attempt_req.routectl_internal.withheld_betas = Arc::from(next);
        tracing::debug!(
            state_key = %sanitize_for_log(&target.state_key),
            count = flags.len(),
            flags = %sanitize_for_log(&flags.join(",")),
            "upstream rejected named beta flags; retrying once without them",
        );
        self.repaired_betas = flags;
        self.skip_replay_backoff = true;
        true
    }
}

#[cfg(test)]
#[path = "learn_beta_repair_tests.rs"]
mod learn_beta_repair_tests;
