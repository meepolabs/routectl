//! Per-target learning state for one chain iteration of a dispatch walk.
//!
//! [`AttemptLearning`] owns every learner handle a chain target holds while
//! its attempts run: the reasoning-replay carry plan, the envelope-field
//! carry plan, the field pre-flight plan (with its canary on the complete
//! surface), the learned re-probe settle guard, and the repaired-state flags
//! each repair branch sets. One value lives for the whole chain iteration --
//! across same-provider retries -- and a fallback hop replaces it by building
//! the next target's.
//!
//! The request-scoped learner state (the learn dedupe set and the repair
//! budget) is NOT owned here: it outlives every target, so the walk keeps it
//! above its chain loop and lends it to each call.
//!
//! The engine never re-dispatches. [`AttemptLearning::on_rejection`] runs the
//! fixed repair branches against the walk's `attempt_req` in place and
//! returns a [`FailureDecision`]; the walk re-gates on `Retry` and continues
//! its ordinary error path on `Proceed`.

use std::collections::HashSet;
use std::time::Instant;

use routectl_core::failure_class::{
    ClassifiedFailure, FailureClass, classify, classify_with_attempt,
};
use routectl_core::{BetaRepairReport, ChatRequest, Error, sanitize_for_log};

use crate::feature_keys::derive_feature_keys;

use super::capability_learn::LearnDedupeKey;
use super::capability_observe::UntranslatedCapabilities;
use super::class_observe::{DispatchSurface, upstream_facts};
use super::dispatch::{
    REPLAY_ACTION_STRIP_REPAIR, REPLAY_REASON_UPSTREAM_REJECTION, replay_rejection_body_free,
};
use super::field_preflight::FieldPreflightPlan;
use super::field_repair::{FieldRepairHandle, FieldRepairPlan, FieldSettlementMode};
use super::repair_budget::RepairBudget;
use super::replay_repair::{ReplayCarryPlan, strip_replay_artifacts_recalibrating};
use super::runtime_gate::LearnedProbeGuard;
use super::{DispatchMeta, DispatchTarget, ReplayDegradation, Router};

/// What the walk does with a rejection after the repair branches ran.
pub(super) enum FailureDecision {
    /// A repair edited `attempt_req` in place: release the attempt's
    /// half-open slot, keep the rejection as the running error, and
    /// re-dispatch the same target. `last_err_body_free` is the body-free
    /// rebuild to keep in place of the raw rejection, when there is one.
    Retry { last_err_body_free: Option<Error> },
    /// No repair applied: the rejection continues on the ordinary error path.
    Proceed,
}

/// The learner state one chain target holds across its attempts.
pub(super) struct AttemptLearning<'a> {
    router: &'a Router,
    /// Only the complete surface claims a field canary, so only it runs the
    /// canary's repaired retry and settlement.
    canary_enabled: bool,
    // Fields drop in declaration order. The guard-owning fields below are
    // declared in REVERSE of the order the walk acquires them, so every exit
    // path releases the last-acquired guard first.
    field_plan: Option<FieldRepairPlan<'a>>,
    replay_plan: Option<ReplayCarryPlan<'a>>,
    preflight: FieldPreflightPlan<'a>,
    learned_probe_guard: LearnedProbeGuard,
    replay_repair_attempted: bool,
    replay_reject_status: u16,
    reactive_repair: Option<FieldRepairHandle>,
    field_reject_status: u16,
    /// The ONLY thing that separates a canary success that disproves the
    /// verdict from one that re-confirms it, so it is set exclusively by the
    /// canary's repair branch and never inferred from the request's shape.
    canary_repair_attempted: bool,
    canary_reject_status: u16,
    canary_lossy_repair: Option<FieldRepairHandle>,
    skip_replay_backoff: bool,
}

impl<'a> AttemptLearning<'a> {
    /// Learner state for one chain target, holding the re-probe settle guard
    /// the walk took for it. The carry and pre-flight plans arrive later, at
    /// their own positions in the per-target request shaping.
    pub(super) fn new(
        router: &'a Router,
        surface: DispatchSurface,
        learned_probe_guard: LearnedProbeGuard,
    ) -> Self {
        Self {
            router,
            canary_enabled: surface == DispatchSurface::Complete,
            field_plan: None,
            replay_plan: None,
            preflight: FieldPreflightPlan::default(),
            learned_probe_guard,
            replay_repair_attempted: false,
            replay_reject_status: 0,
            reactive_repair: None,
            field_reject_status: 0,
            canary_repair_attempted: false,
            canary_reject_status: 0,
            canary_lossy_repair: None,
            skip_replay_backoff: false,
        }
    }

    /// Hold the field pre-flight plan for the rest of the chain iteration.
    /// Its modified-request accounting guards and canary claim release
    /// through RAII on every exit path, so holding it IS the contract.
    pub(super) fn hold_preflight(&mut self, plan: FieldPreflightPlan<'a>) {
        self.preflight = plan;
    }

    /// Claim the reasoning-replay and envelope-field carry slots for this
    /// target. The replay admission may strip `attempt_req` proactively (an
    /// acting negative or a peer probe); the field admission mutates nothing.
    pub(super) fn admit_carries(
        &mut self,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        now: Instant,
    ) {
        self.replay_plan = self
            .router
            .plan_replay_carry(target, attempt_req, meta, now);
        self.field_plan =
            self.router
                .plan_field_carry(target, attempt_req, FieldSettlementMode::Settling, now);
    }

    /// Whether the previous attempt ended in a repair, which re-dispatches
    /// with no backoff sleep. Reading it re-arms the backoff.
    pub(super) const fn take_skip_backoff(&mut self) -> bool {
        std::mem::replace(&mut self.skip_replay_backoff, false)
    }

    /// Open a failed attempt: classify the error NATIVELY (the replay carry
    /// lifts a proven replay rejection only when artifacts were carried).
    ///
    /// Also ends the canary's lossy repair record: it is settled only by the
    /// success of ITS re-dispatch, so any failure after it (including a
    /// second rejection a later repair then fixes) leaves it unsucceeded.
    pub(super) fn classify(
        &mut self,
        err: &Error,
        provider_kind: Option<&str>,
    ) -> ClassifiedFailure {
        self.canary_lossy_repair = None;
        match self.replay_plan.as_ref() {
            Some(plan) => classify_with_attempt(err, provider_kind, plan.attempt()),
            None => classify(err, provider_kind),
        }
    }

    /// Run the fixed repair branches -- replay strip, envelope field, then
    /// the canary's repaired retry -- for a rejection that already passed
    /// auth recovery. Each fires at most once per target and never nests
    /// inside the per-target retry or the fallback walk, so the call count
    /// stays additive.
    ///
    /// The replay branch reads the effective (remapped) class; the field and
    /// canary branches read the NATIVE class, because an operator
    /// `[class_overrides]` entry says how a status should be ROUTED, not what
    /// the upstream said -- reading it would let an override turn a 429 into a
    /// field repair and a permanent verdict.
    ///
    /// On `Retry` after the replay repair the running error is the
    /// rejection's body-free form, so a re-gate refusal cannot surface the
    /// reasoning blob a replay rejection may echo; the field and canary
    /// repairs keep the raw rejection.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_rejection(
        &mut self,
        err: &Error,
        cf: &ClassifiedFailure,
        native_class: &FailureClass,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> FailureDecision {
        if self.try_replay_repair(err, &cf.class, target, attempt_req, meta, budget) {
            return FailureDecision::Retry {
                last_err_body_free: replay_rejection_body_free(
                    err,
                    &cf.class,
                    &target.provider_name,
                ),
            };
        }
        if self.try_field_repair(err, native_class, target, attempt_req, meta, budget)
            || self.try_canary_repair(err, native_class, target, attempt_req, meta, budget)
        {
            return FailureDecision::Retry {
                last_err_body_free: None,
            };
        }
        FailureDecision::Proceed
    }

    /// Reasoning-replay strip repair: when the optimistically carried variant
    /// drew the proven replay rejection, switch to the pre-stripped variant.
    /// It never re-attempts the carried variant; the held guards settle at
    /// success (commit) or on any later exit (release / drop, learning
    /// nothing).
    ///
    /// `budget.draw()` is the LAST condition: it mutates, so drawing before
    /// the class check would charge requests that never repair. An exhausted
    /// budget leaves the rejection on the ordinary error path.
    fn try_replay_repair(
        &mut self,
        err: &Error,
        class: &FailureClass,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> bool {
        if self.replay_repair_attempted {
            return false;
        }
        let Some(plan) = self.replay_plan.as_ref() else {
            return false;
        };
        if !Router::is_replay_rejection_class(class) || !budget.draw() {
            return false;
        }
        self.replay_repair_attempted = true;
        self.replay_reject_status = upstream_facts(err).status.unwrap_or(0);
        let lane = plan.lane();
        meta.replay_degradation = Some(ReplayDegradation {
            action: REPLAY_ACTION_STRIP_REPAIR,
            target_lane: lane,
            state_key: sanitize_for_log(&target.state_key),
            source_schemes: plan.source_schemes().to_vec(),
            reason: REPLAY_REASON_UPSTREAM_REJECTION,
            artifact_count: plan.artifact_count(),
            repair_attempted: true,
            repair_succeeded: false,
            learned: false,
        });
        strip_replay_artifacts_recalibrating(attempt_req, lane, meta);
        self.skip_replay_backoff = true;
        true
    }

    /// Envelope-field L0 repair: on a rejection naming a carried field, drop
    /// (or rewrite) that field from `attempt_req`.
    ///
    /// `apply` owns the budget draw together with the mutation, so a drop
    /// that removes nothing charges nothing and every repaired-state flag is
    /// set only on its `Some`. It also selects WHICH admitted row to repair --
    /// the one the rejection names -- and releases every other candidate's
    /// guard.
    fn try_field_repair(
        &mut self,
        err: &Error,
        native_class: &FailureClass,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> bool {
        if self.reactive_repair.is_some() {
            return false;
        }
        let Some(plan) = self.field_plan.as_mut() else {
            return false;
        };
        let Some(status) = plan.apply(
            attempt_req,
            meta,
            budget,
            native_class,
            err,
            target.provider_kind.unwrap_or(""),
        ) else {
            return false;
        };
        self.field_reject_status = status;
        self.reactive_repair = Some(self.router.note_field_repair(meta, &target.state_key, plan));
        self.skip_replay_backoff = true;
        true
    }

    /// The canary's same-field repaired retry. Its position before the
    /// walk's terminal handling is the whole point: a 400 is terminal and
    /// non-fallbackable, so a retry placed after it would never run and the
    /// rejection the canary provoked -- by restoring a field routectl had
    /// been dropping -- would reach the client.
    ///
    /// No unrelated 4xx gains an exemption: the branch fires only when the
    /// rejection names the SAME closed-table surface the canary restored. The
    /// retry draws the shared request allowance.
    fn try_canary_repair(
        &mut self,
        err: &Error,
        native_class: &FailureClass,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> bool {
        if !self.canary_enabled || self.canary_repair_attempted {
            return false;
        }
        let Some(repair) = self.router.canary_repaired_retry(
            &mut self.preflight,
            attempt_req,
            meta,
            budget,
            native_class,
            err,
            target.provider_kind.unwrap_or(""),
        ) else {
            return false;
        };
        self.canary_repair_attempted = true;
        self.canary_reject_status = repair.status;
        if let Some(path) = repair.lossy_path {
            self.canary_lossy_repair =
                Some(
                    self.router
                        .note_field_repair_at(meta, &target.state_key, path, true),
                );
        }
        self.skip_replay_backoff = true;
        true
    }

    /// The generic learned-capability observation for a rejection that took
    /// no repair, settling this target's re-probes against it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn observe_generic(
        &mut self,
        err: &Error,
        cf: &ClassifiedFailure,
        remapped: bool,
        target: &DispatchTarget,
        req: &ChatRequest,
        untranslated: &UntranslatedCapabilities,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
    ) {
        self.router.observe_for_learning(
            err,
            cf,
            remapped,
            target,
            req,
            untranslated,
            dedupe,
            meta,
            &mut self.learned_probe_guard,
        );
    }

    /// Settle every learner on a successful attempt: reported betas and the
    /// held re-probes first, then the replay carry, the field carry, and (on
    /// the complete surface) the canary. Each settlement rides its learned or
    /// cleared rows out on `meta`, so the ledger records them and a warm
    /// rebuild does not resurrect a negative this request disproved.
    pub(super) fn on_success(
        &mut self,
        beta_report: &BetaRepairReport,
        untranslated: &UntranslatedCapabilities,
        target: &DispatchTarget,
        req: &ChatRequest,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
    ) {
        self.router.settle_attempt_success(
            beta_report,
            untranslated,
            target,
            req,
            dedupe,
            meta,
            &mut self.learned_probe_guard,
        );
        self.settle_replay_success(req, meta);
        self.settle_field_success(req, meta);
        if self.canary_enabled {
            self.settle_canary_success(req, meta);
        }
    }

    /// A stripped repair that reached success confirms the negative
    /// (commit); a carried variant that succeeded outright proves the pair
    /// works, so any resident (lapsed) negative clears.
    fn settle_replay_success(&mut self, req: &ChatRequest, meta: &mut DispatchMeta) {
        let Some(plan) = self.replay_plan.take() else {
            return;
        };
        if self.replay_repair_attempted {
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
            if let Some(deg) = meta.replay_degradation.as_mut() {
                deg.repair_succeeded = true;
                deg.learned = true;
            }
        } else {
            meta.cleared_capabilities.extend(plan.settle_success());
        }
    }

    /// A repaired retry that reached success confirms the verdict (commit);
    /// an UNREPAIRED success proves the field is accepted, so any resident
    /// verdict clears.
    fn settle_field_success(&mut self, req: &ChatRequest, meta: &mut DispatchMeta) {
        let Some(plan) = self.field_plan.take() else {
            return;
        };
        if let Some(repair) = self.reactive_repair {
            let features =
                super::field_repair::request_feature_keys(self.router.field_repair_table(), req);
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
    fn settle_canary_success(&mut self, req: &ChatRequest, meta: &mut DispatchMeta) {
        if let Some(repair) = self.canary_lossy_repair {
            self.router.note_field_repair_succeeded(meta, repair, false);
        }
        let Some(plan) = std::mem::take(&mut self.preflight).into_canary() else {
            return;
        };
        if self.canary_repair_attempted {
            let features =
                super::field_repair::request_feature_keys(self.router.field_repair_table(), req);
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
