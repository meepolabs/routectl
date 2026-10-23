//! Child module of `learn_engine.rs`: how [`AttemptLearning`] answers one
//! rejection. The declared-edit undo runs first, then the moved-on settlement
//! of pending field steps, then the arbitration over the proposers; the whole
//! decision is wrapped so a repair the request's allowance refused is counted
//! once per rejection.

use routectl_core::ChatRequest;
use routectl_core::wire_check::EditSite;

use super::super::DispatchMeta;
use super::super::dispatch::replay_rejection_body_free;
use super::super::field_repair::{rejected_field, rerepairs_same_field};
use super::super::learn_revert::{MatchedEdit, answer_declared_edit};
use super::super::repair_budget::RepairBudget;
use super::learn_beta_repair::EngineSink;
use super::{
    Arbitration, AttemptLearning, FailureDecision, Rejection, RejectionInput, RejectionSite,
    arbitrate,
};

impl AttemptLearning<'_> {
    /// Arbitrate a rejection that already passed auth recovery: every
    /// proposer reads it, and the first proposal in [`ARBITRATION_ORDER`] that
    /// takes decides it -- replay strip, envelope field (the canary's repaired
    /// retry answering the same proposal when the reactive repair does not),
    /// beta withhold, then the generic capability learner. The field repair
    /// fires once per field and every other repair at most once per target;
    /// none nests inside the per-target retry or the fallback walk, so the
    /// call count stays additive. Before the proposers, a repaired retry
    /// refused for a different field commits the field steps it proves.
    ///
    /// On `Proceed` the generic capability proposal is held for
    /// [`Self::observe_generic`]; a `Retry` never reaches it, because the walk
    /// re-dispatches first.
    ///
    /// `rejection.site` is what the attempt's settlement read off the wire;
    /// `None` arbitrates exactly as a rejection with no wire record. A site
    /// naming a path one or more declared routectl edits put on the wire is
    /// answered by undoing that edit, or not at all, and no proposer reads
    /// it: such a rejection says nothing about what the upstream accepts.
    /// The one exception is a single required field routectl wrote with its
    /// own value: when that edit is not undone, the proposers read the
    /// rejection as if no edit explained it.
    ///
    /// On `Retry` after the replay repair the running error is the
    /// rejection's body-free form, so a re-gate refusal cannot surface the
    /// reasoning blob a replay rejection may echo; the field, canary and beta
    /// repairs keep the raw rejection.
    ///
    /// A rejection laid to the request's defective transcript (see
    /// [`Rejection::transcript_rejection`]) proceeds without consulting any
    /// proposer, after the declared-edit undo, and is held for
    /// [`Self::observe_generic`] so the generic learner skips it too.
    ///
    /// A rejection whose proposed repair the request's allowance could not pay
    /// for is counted once on the target's lane, whichever arm was refused.
    pub(in crate::router) fn on_rejection(
        &mut self,
        rejection: Rejection<'_>,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> FailureDecision {
        let target = rejection.target;
        budget.take_refusal();
        let decision = self.decide_rejection(rejection, attempt_req, meta, budget);
        if budget.take_refusal() {
            self.router
                .metrics
                .incr_repair_budget_exhausted(target.learned_lane.as_ref());
        }
        decision
    }

    /// The body of [`Self::on_rejection`], every exit of which the caller
    /// reads the budget's refusal mark after.
    fn decide_rejection(
        &mut self,
        rejection: Rejection<'_>,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> FailureDecision {
        self.pending_capability = None;
        self.transcript_rejection = rejection.transcript_rejection;
        if let Some(decision) = self.answer_declared(&rejection, attempt_req, meta, budget) {
            return decision;
        }
        if self.transcript_rejection {
            return FailureDecision::Proceed;
        }
        self.settle_field_moved_on(&rejection, meta);
        self.arbitrate_proposers(rejection, attempt_req, meta, budget)
    }

    /// The decision for a rejection laid to declared routectl edits: a retry
    /// when the one edit was undone, otherwise `Proceed` with no proposer
    /// consulted. `None` when no declared edit explains the rejection, or
    /// when the one edit not undone is a required-field default or a field
    /// repair whose row re-repairs its field, which the proposers then answer.
    fn answer_declared(
        &mut self,
        rejection: &Rejection<'_>,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> Option<FailureDecision> {
        let reverted = answer_declared_edit(
            &mut self.revert,
            rejection.site.as_ref(),
            rejection.client_req,
            attempt_req,
            meta,
            budget,
        )?;
        if !reverted {
            if RejectionSite::unreverted_reaches_proposers(rejection.site.as_ref())
                || self.rerepairs_declared_field(rejection)
            {
                return None;
            }
            return Some(FailureDecision::Proceed);
        }
        self.skip_replay_backoff = true;
        Some(FailureDecision::Retry {
            last_err_body_free: None,
        })
    }

    /// Whether the one declared edit a rejection names is an earlier field
    /// repair, and the row the rejection names re-repairs its field: the
    /// upstream refused the value that repair wrote, which a further repair
    /// of the same field may answer.
    fn rerepairs_declared_field(&self, rejection: &Rejection<'_>) -> bool {
        let is_field_repair = matches!(
            rejection.site.as_ref().map(|site| &site.matched_edit),
            Some(MatchedEdit::One(edit)) if edit.site() == EditSite::FieldRepair
        );
        is_field_repair
            && rejected_field(
                self.router.field_repair_table(),
                rejection.native_class,
                rejection.err,
                rejection.target.provider_kind.unwrap_or(""),
                RejectionSite::sent_effort_of(rejection.site.as_ref()),
            )
            .is_some_and(|named| rerepairs_same_field(named.row))
    }

    /// Run [`arbitrate`] over the proposers and turn its verdict into the
    /// walk's decision, holding a `Proceed`'s capability proposal.
    fn arbitrate_proposers(
        &mut self,
        rejection: Rejection<'_>,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> FailureDecision {
        let (err, class, target) = (rejection.err, rejection.class, rejection.target);
        let withheld_betas = std::sync::Arc::clone(&attempt_req.routectl_internal.withheld_betas);
        let pending_rejection = rejection
            .site
            .as_ref()
            .filter(|_| !self.count_surface)
            .map(|site| site.pending);
        let input = RejectionInput {
            err,
            class,
            native_class: rejection.native_class,
            target,
            replay_carried: self.replay_plan.is_some(),
            table: self.router.field_repair_table(),
            site: rejection.site,
            client_betas: &rejection.client_req.anthropic_beta,
            withheld_betas: &withheld_betas,
            beta_assembly: rejection.beta_assembly,
        };
        let mut sink = EngineSink {
            learning: self,
            err,
            target,
            attempt_req,
            meta,
            budget,
            pending_rejection,
        };
        match arbitrate(&input, &mut sink) {
            Arbitration::ReplayStrip => FailureDecision::Retry {
                last_err_body_free: replay_rejection_body_free(
                    err,
                    &class.class,
                    &target.provider_name,
                ),
            },
            Arbitration::FieldRepair | Arbitration::CanaryRetry | Arbitration::BetaRepair => {
                FailureDecision::Retry {
                    last_err_body_free: None,
                }
            }
            Arbitration::Proceed(resolved) => {
                self.pending_capability = resolved;
                FailureDecision::Proceed
            }
        }
    }
}
