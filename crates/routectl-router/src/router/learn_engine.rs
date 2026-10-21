//! Per-target learning state for one chain iteration of a dispatch walk.
//!
//! [`AttemptLearning`] owns every learner handle a chain target holds while
//! its attempts run: the reasoning-replay carry plan, the envelope-field
//! carry plan, the field pre-flight plan (with its canary on the complete
//! surface), the learned re-probe settle guard, the beta flags a repair
//! withheld, and the repaired-state flags each repair branch sets. One value
//! lives for the whole chain iteration -- across same-provider retries -- and
//! a fallback hop replaces it by building the next target's.
//!
//! The request-scoped learner state (the learn dedupe set and the repair
//! budget) is NOT owned here: it outlives every target, so the walk keeps it
//! above its chain loop and lends it to each call.
//!
//! The engine never re-dispatches. [`AttemptLearning::on_rejection`] asks
//! every proposer what the rejection supports, arbitrates in the one fixed
//! order ([`ARBITRATION_ORDER`]), applies the first repair that takes against
//! the walk's `attempt_req` in place, and returns a [`FailureDecision`]; the
//! walk re-gates on `Retry` and continues its ordinary error path on
//! `Proceed`. A `Proceed` keeps the generic capability proposal for
//! [`AttemptLearning::observe_generic`], which applies it later in the error
//! path, after the class observability.

use std::collections::HashSet;
use std::time::Instant;

use routectl_core::capability::{FailurePhase, SignalTier};
use routectl_core::failure_class::{
    ClassifiedFailure, FailureClass, classify, classify_with_attempt,
};
use routectl_core::wire_check::{EditSite, RejectionProvenance};
use routectl_core::{BetaAssembly, ChatRequest, Error, sanitize_for_log};

use super::capability_learn::LearnDedupeKey;
use super::capability_observe::UntranslatedCapabilities;
use super::class_observe::{DispatchSurface, upstream_facts};
use super::dispatch::{
    REPLAY_ACTION_STRIP_REPAIR, REPLAY_REASON_UPSTREAM_REJECTION, replay_rejection_body_free,
};
use super::field_preflight::FieldPreflightPlan;
use super::field_repair::{
    FieldRepairHandle, FieldRepairPlan, FieldRepairRow, FieldRepairTable, FieldSettlementMode,
};
use super::learn_proposers::{BetaProposer, CapabilityProposer, FieldRowProposer, ReplayProposer};
use super::learn_revert::{MatchedEdit, SentSlot, TargetRevert, answer_declared_edit};
use super::rejection_parser::{NamedRejection, RawPath, RejectionKind, RejectionValue};
use super::repair_budget::RepairBudget;
use super::replay_repair::{ReplayCarryPlan, strip_replay_artifacts_recalibrating};
use super::runtime_gate::LearnedProbeGuard;
use super::{DispatchMeta, DispatchTarget, ReplayDegradation, Router};

/// A generic capability attribution: the canonical capability key, the
/// signal tier, and the phase the resolver read it at.
pub(super) type ResolvedCapability = (String, SignalTier, FailurePhase);

/// Where on the attempt's wire a rejection points, as settled from the
/// attempt's record before any repair touched the request.
#[derive(Clone, Debug)]
pub(super) struct RejectionSite {
    /// Where the named path came from, as the wire check records it.
    pub(super) provenance: RejectionProvenance,
    /// Whether the body that went out carries the named path. `None` when
    /// that cannot be told: no deposit, an unparseable body, a rejection
    /// naming no exact path, or a path whose shape the body does not share.
    pub(super) in_egress: Option<bool>,
    /// The attempt's declared request edits that put the named path on the
    /// wire. Only a path the body that went out carries, on a fidelity lane,
    /// is matched.
    pub(super) matched_edit: MatchedEdit,
    /// The named location, unmasked, when the rejection named an exact one.
    /// Read to undo an edit there; never rendered.
    pub(super) locator: Option<RawPath>,
    /// What the body that went out held at `locator`.
    pub(super) sent: SentSlot,
    /// The rejection's position among the request's pending rejections.
    pub(super) pending: usize,
}

impl RejectionSite {
    /// Whether the rejection names a path the body that went out did not
    /// carry. Such a rejection cannot be about anything routectl sent there,
    /// so no path-keyed proposal may act on it.
    fn names_absent_path(site: Option<&Self>) -> bool {
        site.and_then(|site| site.in_egress) == Some(false)
    }

    /// Whether a declared edit that could not be undone still leaves the
    /// rejection to the proposers: only one routectl-written required field
    /// does, since the upstream may refuse routectl's value and a learned
    /// repair can supply another. A path with no closed-table row stays
    /// inert, as no proposer names it.
    fn unreverted_reaches_proposers(site: Option<&Self>) -> bool {
        matches!(
            site.map(|site| &site.matched_edit),
            Some(MatchedEdit::One(edit)) if edit.site() == EditSite::RequiredFieldDefault
        )
    }
}

/// Everything a proposer may read about one rejection, borrowed read-only.
///
/// Holds no `&mut` state, no repair budget and no registry handle: a
/// proposer only says what the rejection supports, and the arbitration alone
/// decides which proposal acts.
pub(super) struct RejectionInput<'r> {
    pub(super) err: &'r Error,
    /// The effective class, after any operator `[class_overrides]` remap.
    pub(super) class: &'r ClassifiedFailure,
    /// What the upstream actually said. The path-keyed proposers read this,
    /// never `class`: an override says how a status is ROUTED, and reading it
    /// would let a remapped 429 drop a field and mint a verdict.
    pub(super) native_class: &'r FailureClass,
    pub(super) target: &'r DispatchTarget,
    /// Whether this target's attempt carried non-portable replay artifacts.
    pub(super) replay_carried: bool,
    pub(super) table: FieldRepairTable,
    pub(super) site: Option<RejectionSite>,
    /// The `anthropic-beta` flags the client sent, before any operator
    /// overlay merged its own pins in.
    pub(super) client_betas: &'r [String],
    /// The flags the attempt already withheld from this lane.
    pub(super) withheld_betas: &'r [String],
    /// The beta floor and body-implied set the attempt's egress recorded;
    /// `None` when it recorded none.
    pub(super) beta_assembly: Option<&'r BetaAssembly>,
}

/// What one proposer says a rejection supports. Closed: a new learner is a
/// new variant here and a new arm in [`arbitrate`], never an open registry.
#[derive(Clone, Debug)]
pub(super) enum Proposal {
    /// Re-send the attempt with its reasoning-replay artifacts stripped.
    ReplayStrip,
    /// Edit the closed-table row the rejection named; the canary's repaired
    /// retry answers the same proposal when the reactive repair does not.
    FieldRow {
        row: &'static FieldRepairRow,
        kind: RejectionKind,
        tier: SignalTier,
        value: Option<RejectionValue>,
    },
    /// Re-send the attempt withholding the named client `anthropic-beta`
    /// flags; a success then learns a `beta:<flag>` negative for each.
    Beta { flags: Vec<String> },
    /// Learn a generic capability negative for the attributed capability.
    Capability(ResolvedCapability),
}

/// One learner's reading of a rejection.
pub(super) trait Proposer {
    fn propose(&self, input: &RejectionInput<'_>) -> Option<Proposal>;
}

/// One slot of the arbitration order. An enum rather than a trait object so
/// the order is a plain static array dispatched by `match`.
#[derive(Clone, Copy, Debug)]
pub(super) enum ProposerSlot {
    Replay(ReplayProposer),
    FieldRow(FieldRowProposer),
    Beta(BetaProposer),
    Capability(CapabilityProposer),
}

impl ProposerSlot {
    /// Whether this slot's proposal is keyed on the path the rejection
    /// names. The canary answers the field-row proposal, so it is keyed on
    /// the same path.
    const fn is_path_keyed(self) -> bool {
        matches!(self, Self::FieldRow(_))
    }
}

impl Proposer for ProposerSlot {
    fn propose(&self, input: &RejectionInput<'_>) -> Option<Proposal> {
        match self {
            Self::Replay(proposer) => proposer.propose(input),
            Self::FieldRow(proposer) => proposer.propose(input),
            Self::Beta(proposer) => proposer.propose(input),
            Self::Capability(proposer) => proposer.propose(input),
        }
    }
}

/// THE precedence among the learners: the first slot whose proposal takes
/// decides the rejection.
pub(super) const ARBITRATION_ORDER: [ProposerSlot; 4] = [
    ProposerSlot::Replay(ReplayProposer),
    ProposerSlot::FieldRow(FieldRowProposer),
    ProposerSlot::Beta(BetaProposer),
    ProposerSlot::Capability(CapabilityProposer),
];

/// The effects a winning repair proposal has on the attempt. Each returns
/// whether the repair took; `false` means nothing was mutated or charged.
pub(super) trait RepairSink {
    fn strip_replay(&mut self) -> bool;
    fn repair_field(&mut self, named: NamedRejection) -> bool;
    fn canary_retry(&mut self, named: NamedRejection) -> bool;
    fn withhold_betas(&mut self, flags: Vec<String>) -> bool;
}

/// What the arbitration decided for one rejection.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Arbitration {
    ReplayStrip,
    FieldRepair,
    CanaryRetry,
    BetaRepair,
    /// No repair took. Carries the generic capability proposal, if any, for
    /// the generic learner to apply.
    Proceed(Option<ResolvedCapability>),
}

/// Walk [`ARBITRATION_ORDER`] and settle the rejection on the first proposal
/// that takes. A repair proposal whose effect does not take (already spent,
/// no plan, budget exhausted) falls through to the next slot; a capability
/// proposal always takes, as the generic learner's claim.
///
/// A rejection naming a path the sent body did not carry skips every
/// path-keyed slot: nothing is repaired or learned for that path. The
/// generic capability slot still runs, behind its own request-membership
/// gate, since a capability name is not a body path.
pub(super) fn arbitrate(input: &RejectionInput<'_>, sink: &mut impl RepairSink) -> Arbitration {
    let skip_path_keyed = RejectionSite::names_absent_path(input.site.as_ref());
    for slot in ARBITRATION_ORDER {
        if skip_path_keyed && slot.is_path_keyed() {
            continue;
        }
        if let Some(proposal) = slot.propose(input)
            && let Some(decided) = take_proposal(proposal, sink)
        {
            return decided;
        }
    }
    Arbitration::Proceed(None)
}

/// The decision `proposal` settles the rejection on, or `None` when its
/// repair effect did not take.
fn take_proposal(proposal: Proposal, sink: &mut impl RepairSink) -> Option<Arbitration> {
    match proposal {
        Proposal::ReplayStrip => sink.strip_replay().then_some(Arbitration::ReplayStrip),
        Proposal::FieldRow {
            row,
            kind,
            tier,
            value,
        } => {
            let named = NamedRejection {
                row,
                tier,
                kind,
                value,
            };
            if sink.repair_field(named) {
                Some(Arbitration::FieldRepair)
            } else {
                sink.canary_retry(named).then_some(Arbitration::CanaryRetry)
            }
        }
        Proposal::Beta { flags } => sink
            .withhold_betas(flags)
            .then_some(Arbitration::BetaRepair),
        Proposal::Capability(resolved) => Some(Arbitration::Proceed(Some(resolved))),
    }
}

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

/// One rejection as the walk hands it to [`AttemptLearning::on_rejection`],
/// borrowed read-only.
pub(super) struct Rejection<'r> {
    pub(super) err: &'r Error,
    /// The effective class, after any operator `[class_overrides]` remap.
    pub(super) class: &'r ClassifiedFailure,
    /// What the upstream actually said.
    pub(super) native_class: &'r FailureClass,
    pub(super) target: &'r DispatchTarget,
    /// The client's canonical request, which a revert restores values from.
    pub(super) client_req: &'r ChatRequest,
    /// What the attempt's settlement read off the wire.
    pub(super) site: Option<RejectionSite>,
    /// Whether the rejection is laid to the request's defective transcript,
    /// decided once per attempt by
    /// [`is_transcript_rejection`](super::transcript_defect::is_transcript_rejection).
    pub(super) transcript_rejection: bool,
    /// The beta floor and body-implied set the attempt's egress recorded.
    pub(super) beta_assembly: Option<&'r BetaAssembly>,
}

/// The learner state one chain target holds across its attempts.
pub(super) struct AttemptLearning<'a> {
    router: &'a Router,
    /// Only the complete surface claims a field canary, so only it runs the
    /// canary's repaired retry and settlement.
    canary_enabled: bool,
    /// The count surface mints no generic verdict and settles its replay
    /// carry by drop: a token count validates only what its own endpoint
    /// projects, which is narrower than what a message dispatch sends.
    count_surface: bool,
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
    /// The client beta flags this target's beta repair withheld; empty until
    /// one takes. Settled as `beta:` negatives only by a later success.
    repaired_betas: Vec<String>,
    skip_replay_backoff: bool,
    /// The generic capability proposal of the last rejection that took no
    /// repair, held until [`Self::observe_generic`] applies it.
    pending_capability: Option<ResolvedCapability>,
    /// The revert of a declared routectl edit a rejection named.
    revert: TargetRevert,
    /// Whether the last rejection was laid to the request's defective
    /// transcript, which no learner may read.
    transcript_rejection: bool,
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
            count_surface: surface == DispatchSurface::CountTokens,
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
            repaired_betas: Vec::new(),
            skip_replay_backoff: false,
            pending_capability: None,
            revert: TargetRevert::default(),
            transcript_rejection: false,
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
    /// A `NonSettling` field admission claims no single-flight slot.
    pub(super) fn admit_carries(
        &mut self,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        mode: FieldSettlementMode,
        now: Instant,
    ) {
        self.replay_plan = self
            .router
            .plan_replay_carry(target, attempt_req, meta, now);
        self.field_plan = self.router.plan_field_carry(target, attempt_req, mode, now);
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
        self.revert.on_failure();
        match self.replay_plan.as_ref() {
            Some(plan) => classify_with_attempt(err, provider_kind, plan.attempt()),
            None => classify(err, provider_kind),
        }
    }

    /// Arbitrate a rejection that already passed auth recovery: every
    /// proposer reads it, and the first proposal in [`ARBITRATION_ORDER`] that
    /// takes decides it -- replay strip, envelope field, the canary's repaired
    /// retry, then the generic capability learner. Each repair fires at most
    /// once per target and never nests inside the per-target retry or the
    /// fallback walk, so the call count stays additive.
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
    /// reasoning blob a replay rejection may echo; the field and canary
    /// repairs keep the raw rejection.
    ///
    /// A rejection laid to the request's defective transcript (see
    /// [`Rejection::transcript_rejection`]) proceeds without consulting any
    /// proposer, after the declared-edit undo, and is held for
    /// [`Self::observe_generic`] so the generic learner skips it too.
    pub(super) fn on_rejection(
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
        self.arbitrate_proposers(rejection, attempt_req, meta, budget)
    }

    /// The decision for a rejection laid to declared routectl edits: a retry
    /// when the one edit was undone, otherwise `Proceed` with no proposer
    /// consulted. `None` when no declared edit explains the rejection, or
    /// when the one edit not undone is a required-field default, which the
    /// proposers then answer.
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
            if RejectionSite::unreverted_reaches_proposers(rejection.site.as_ref()) {
                return None;
            }
            return Some(FailureDecision::Proceed);
        }
        self.skip_replay_backoff = true;
        Some(FailureDecision::Retry {
            last_err_body_free: None,
        })
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

    /// Reasoning-replay strip repair: when the optimistically carried variant
    /// drew the proven replay rejection, switch to the pre-stripped variant.
    /// It never re-attempts the carried variant; the held guards settle at
    /// success (commit) or on any later exit (release / drop, learning
    /// nothing).
    ///
    /// `budget.draw()` is the LAST condition: it mutates, so drawing before
    /// the proposer's class check would charge requests that never repair. An
    /// exhausted budget leaves the rejection on the ordinary error path.
    fn try_replay_repair(
        &mut self,
        err: &Error,
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
        if !budget.draw() {
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
        named: NamedRejection,
        err: &Error,
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
        let Some(status) = plan.apply_named(named, attempt_req, meta, budget, err) else {
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
        named: NamedRejection,
        err: &Error,
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
            named,
            err,
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
    /// no repair, settling this target's re-probes against it. Applies only
    /// the capability proposal [`Self::on_rejection`] held; it resolves
    /// nothing itself. A rejection laid to the transcript learns nothing and
    /// counts only as one the field parser did not localize; on the count
    /// surface no rejection learns, and one only counts when unlocalized.
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
        if self.transcript_rejection {
            self.router
                .observe_transcript_rejection(err, target, dedupe);
            return;
        }
        if self.count_surface {
            self.pending_capability = None;
            self.router
                .observe_unlocalized_field_rejection(err, cf, remapped, target, dedupe);
            return;
        }
        self.router.observe_for_learning(
            err,
            cf,
            remapped,
            target,
            req,
            untranslated,
            self.pending_capability.take(),
            dedupe,
            meta,
            &mut self.learned_probe_guard,
        );
    }

    /// Settle every learner on a successful attempt: the beta flags a repair
    /// withheld and the held re-probes first, then the replay carry, the
    /// field carry, and (on the complete surface) the canary. The count
    /// surface's beta repair served its answer and settles nothing. Each settlement rides its learned or
    /// cleared rows out on `meta`, so the ledger records them and a warm
    /// rebuild does not resurrect a negative this request disproved.
    pub(super) fn on_success(
        &mut self,
        untranslated: &UntranslatedCapabilities,
        target: &DispatchTarget,
        req: &ChatRequest,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
    ) {
        let repaired = if self.count_surface {
            &[][..]
        } else {
            self.repaired_betas.as_slice()
        };
        self.router.settle_attempt_success(
            repaired,
            untranslated,
            target,
            req,
            dedupe,
            meta,
            &mut self.learned_probe_guard,
        );
        self.revert.on_success(meta);
        self.settle_replay_success(req, meta);
        self.settle_field_success(req, meta);
        if self.canary_enabled {
            self.settle_canary_success(req, meta);
        }
    }
}

#[path = "learn_settle.rs"]
mod learn_settle;

#[path = "learn_beta_repair.rs"]
mod learn_beta_repair;
use learn_beta_repair::EngineSink;

#[cfg(test)]
#[path = "learn_engine_tests.rs"]
mod learn_engine_tests;

#[cfg(test)]
#[path = "learn_engine_location_tests.rs"]
mod learn_engine_location_tests;
