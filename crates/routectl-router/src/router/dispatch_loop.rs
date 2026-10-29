//! The one dispatch walk every surface runs. [`Router::run_walk`] owns the
//! lifecycle: chain resolution and probe admissions, the per-target guards
//! and request shaping, the gate, the attempt loop with its per-attempt
//! deadline, auth refresh, the learner's repair branch, classification and
//! remap, breaker outcome, credential park, the same-provider retry decision,
//! fallback, the last error, and `DispatchMeta` advancement. A
//! [`SurfaceAdapter`] supplies what differs in type between surfaces: the
//! provider call, what a committed attempt becomes, the exhaustion error, and
//! the typed hooks at the walk's seams (re-probe settlement, field settlement
//! mode, per-attempt wire handling, error disposition, failure observation).
//! Differences that are booleans are rows on [`DispatchSurface`].

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_core::context_reduction::{ReductionDelta, ReductionOutcome, apply_json_minify};
use routectl_core::failure_class::{ClassifiedFailure, FailureClass};
use routectl_core::wire_check::{IngressDialect, WireTap};
use routectl_core::{
    ChatRequest, Error, Provider, RefusalReason, Result, TranslationReport, sanitize_for_log,
};

use crate::config::RetryPolicy;

use super::beta_report_learn::install_beta_assembly;
use super::cache_plan::{AutoCacheRequestPlan, apply_auto_cache_placement, cache_target_gates};
use super::capability_learn::LearnDedupeKey;
use super::capability_observe::{UntranslatedCapabilities, install_translation_report};
use super::class_observe::{
    DispatchSurface, apply_remap, upstream_facts, upstream_status_for_remap,
};
use super::credential_park::{CREDENTIAL_PARKED_GATE_KIND, log_gate_refusal, merge_gate_refusal};
use super::edit_declare::{
    declare_cache_placement, declare_model_rename, declare_reduction, fidelity_dialect,
};
use super::feature_filter::StripDecision;
use super::field_preflight::emit_field_preflight;
use super::field_repair::{FieldSettlementMode, emit_field_repair};
use super::learn_engine::{AttemptLearning, FailureDecision, Rejection, RejectionSite};
use super::local_refusal::ingress_dialect_of;
use super::outcome_rule::{
    SameProvider, breaker_outcome, class_debits, probe_fast_fail, rate_limit_reset_hint,
    same_provider_retry, should_fallback,
};
use super::overlays::apply_layered_overlays;
use super::probe_payload_capture::ProbeFieldCapture;
use super::repair_budget::RepairBudget;
use super::replay_repair::{emit_replay_degradation, replay_rejection_body_free};
use super::runtime_gate::{
    LearnedProbeGuard, ProbeAdmission, ProbeAdmissionSet, is_probe_request, log_probe_fast_fail,
};
use super::transcript_defect::{
    egress_pairing_refusal, is_transcript_rejection, record_transcript_defect,
};
use super::wire_check::SampleCharge;
use super::{DispatchMeta, DispatchTarget, Router, RouterOptions};

/// A per-attempt deadline a surface asks the walk to enforce around its
/// provider call, and the message its expiry carries.
#[derive(Clone, Copy)]
pub(super) struct Deadline {
    ms: u64,
    describe: fn(u64) -> String,
}

impl Deadline {
    pub(super) const fn new(ms: u64, describe: fn(u64) -> String) -> Self {
        Self { ms, describe }
    }

    pub(super) const fn after(self) -> Duration {
        Duration::from_millis(self.ms)
    }
}

/// An attempt's deadline expired before its provider call resolved.
struct DeadlineFired(Deadline);

impl DeadlineFired {
    /// The status-0 upstream error the walk records for the expired attempt,
    /// attributed to the config-key provider name the chain walks so it lines
    /// up with gate errors and `routectl_provider`, never `provider.id()`.
    fn error(self, provider_name: &str) -> Error {
        Error::upstream(provider_name, 0, (self.0.describe)(self.0.ms))
    }
}

/// The expiry text of a `complete()` attempt deadline, shared by the token
/// count so the two surfaces time out with the same error.
pub(super) fn complete_timed_out(ms: u64) -> String {
    format!("request timed out after {ms}ms")
}

/// What a committed attempt hands its surface: the target that served, the
/// requests as the client sent them and as they went upstream, the attempt's
/// wire tap and translation report, and the per-request learner state.
pub(super) struct CommitCtx<'w, 'm> {
    pub(super) router: &'w Router,
    pub(super) target: &'w DispatchTarget,
    pub(super) client_req: &'w ChatRequest,
    pub(super) attempt_req: &'w ChatRequest,
    pub(super) wire_tap: Option<&'w WireTap>,
    pub(super) translation_report: &'w TranslationReport,
    pub(super) learning: &'w mut AttemptLearning<'m>,
    pub(super) learn_dedupe: &'w mut HashSet<LearnDedupeKey>,
    pub(super) meta: &'w mut DispatchMeta,
}

/// How the walk treats a failed attempt before its common error handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ErrorDisposition {
    /// Learner repair, breaker outcome, retry, fallback: the common path.
    Common,
    /// The target cannot serve this surface after all: free its probe slot
    /// without a breaker debit, record no last error, and walk on.
    WalkWithoutDebit,
}

/// What the walk knows when no attempt committed.
pub(super) struct Exhaustion<'a> {
    /// The last error the walk recorded, if any.
    pub(super) last: Option<Error>,
    /// The client's requested alias.
    pub(super) alias: &'a str,
    /// Whether any chain target passed [`SurfaceAdapter::admits`].
    pub(super) any_admitted: bool,
}

/// The settled facts of one failed attempt, for its observability.
pub(super) struct FailureObservation<'a> {
    pub(super) err: &'a Error,
    pub(super) cf: &'a ClassifiedFailure,
    pub(super) original_class: &'a FailureClass,
    pub(super) remapped: bool,
    pub(super) remap_status: Option<u16>,
    pub(super) target: &'a DispatchTarget,
    pub(super) fallback: bool,
    pub(super) policy: &'a RetryPolicy,
    pub(super) debit: bool,
    pub(super) is_probe: bool,
    pub(super) is_forwarded: bool,
    pub(super) ingress: Option<IngressDialect>,
}

/// One dispatch surface's part of the walk. Implementations are few,
/// closed and statically dispatched; anything a surface decides with a
/// boolean belongs on [`DispatchSurface`] instead. The adapter value is
/// built per walk and owned by it, so it is the walk's adapter state.
pub(super) trait SurfaceAdapter: Send + Sync {
    /// What a committed attempt produces for the caller.
    type Output: Send;
    /// The surface token every shared rule and observability field reads.
    const SURFACE: DispatchSurface;

    /// Whether `target` can serve this surface at all. A target that cannot
    /// is skipped without being marked, gated, or counted.
    fn admits(&self, target: &DispatchTarget) -> bool;

    /// The per-attempt deadline under the composed attempt policy, if any.
    fn deadline(&self, attempt_policy: &RetryPolicy) -> Option<Deadline>;

    /// The provider call for one attempt. `Ok` means committed: nothing
    /// after it may walk to another target.
    fn invoke(
        &self,
        target: &DispatchTarget,
        provider: Arc<dyn Provider>,
        req: ChatRequest,
    ) -> impl Future<Output = Result<Self::Output>> + Send;

    /// Finish a committed attempt after the walk has settled the seat's
    /// success, the sample charge and the request wire check.
    fn on_commit(&self, ctx: CommitCtx<'_, '_>, out: Self::Output) -> Self::Output;

    /// The error a walk that committed nothing returns.
    fn exhausted(&self, exhaustion: Exhaustion<'_>) -> Error;

    /// Settle, before the walk, the re-probe admissions the chain filter
    /// granted, returning those the walk settles on reaching their target.
    fn settle_probe_admissions(
        &self,
        _router: &Router,
        admissions: Vec<ProbeAdmission>,
    ) -> Vec<ProbeAdmission> {
        admissions
    }

    /// Whether the envelope-field carry may settle a verdict.
    fn field_settlement_mode(&self) -> FieldSettlementMode {
        FieldSettlementMode::Settling
    }

    /// Give one provider call its wire tap and sampler charge.
    fn install_wire(
        &self,
        router: &Router,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
    ) -> (Option<WireTap>, SampleCharge) {
        router.install_wire_tap(target, attempt_req)
    }

    /// Settle the wire of a failed attempt and return where on the wire its
    /// rejection points.
    #[allow(clippy::too_many_arguments)]
    fn read_failed_wire(
        &self,
        router: &Router,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        tap: Option<&WireTap>,
        charge: SampleCharge,
        err: &Error,
        meta: &mut DispatchMeta,
    ) -> Option<RejectionSite> {
        router.settle_failed_attempt(target, attempt_req, tap, charge, err, meta)
    }

    /// How a failed attempt on `target` is handled, decided after auth
    /// recovery and before any learner or breaker reads it.
    fn disposition(&self, _target: &DispatchTarget, _err: &Error) -> ErrorDisposition {
        ErrorDisposition::Common
    }

    /// Observe a failed attempt the walk has settled.
    fn observe_failure(&self, router: &Router, failure: &FailureObservation<'_>) {
        router.emit_class_observability(
            failure.err,
            failure.cf,
            failure.original_class,
            failure.remapped,
            failure.remap_status,
            Self::SURFACE,
            failure.target.provider_name.as_str(),
            failure.target,
            failure.fallback,
            failure.policy,
            failure.debit,
            failure.is_probe,
            failure.is_forwarded,
            failure.ingress,
        );
    }
}

impl Router {
    /// Close a resolved walk: count the reactive repairs it drew and report
    /// its replay degradations, field repairs, wire rejections and field
    /// pre-flight decisions. Called exactly once per public entry point,
    /// after [`Router::run_walk`] returns on any exit, when every repair
    /// record on `meta` is final.
    ///
    /// This is where a complete or a token count finishes as a request. A
    /// stream's walk also ends here, at its commit or exhaustion, but the
    /// request it serves ends later: at the end of the cli's `drive_stream`
    /// loop (`routectl-cli` `handlers/ingress_handle.rs`), which finalizes
    /// the stream's usage capture on every exit.
    ///
    /// A token count queues no wire rejection on `meta` (see
    /// [`Router::read_count_rejection_site`]), so reporting them is a no-op
    /// there.
    pub(crate) fn finish_dispatch(&self, meta: &DispatchMeta, repair_budget: &RepairBudget) {
        self.metrics
            .record_repairs_per_request(repair_budget.spent());
        emit_replay_degradation(meta);
        emit_field_repair(meta);
        self.emit_wire_rejections(meta);
        emit_field_preflight(meta);
    }

    /// Walk the resolved chain for `req` on `adapter`'s surface. Mutates
    /// `meta` as the chain is walked -- `attempt_count` on every upstream
    /// attempt, `fallback_count` on every hop to a later entry, and the
    /// served_* fields on each dispatched target -- so the caller's `meta`
    /// is correct at every early return, including the all-failed Err path.
    pub(super) async fn run_walk<A: SurfaceAdapter>(
        &self,
        adapter: A,
        mut req: ChatRequest,
        opts: RouterOptions,
        meta: &mut DispatchMeta,
        repair_budget: &mut RepairBudget,
    ) -> Result<A::Output> {
        let pairing = record_transcript_defect(&mut req);
        let (chain, probe_admissions) = self.dispatch_chain_for_request(&req, A::SURFACE)?;
        let probe_admissions = adapter.settle_probe_admissions(self, probe_admissions);
        // Re-probes the chain filter admitted, owned by a request-scoped set
        // that settles them on transfer or on drop. `take(state_key)` moves a
        // target's admissions into its `LearnedProbeGuard` when the loop
        // reaches the target; any admission still held when the set drops (an
        // earlier target succeeded, a terminal error, a `break 'chain`, a
        // client disconnect) was never reached and settles as `OtherError`, so
        // its `in_flight` slot never latches.
        let mut probe_set = ProbeAdmissionSet::new(
            self.learned_capabilities.clone(),
            probe_admissions,
            A::SURFACE.as_str(),
        );
        // Whether the CLIENT'S request carried a captured forwarded
        // bearer, computed ONCE before the chain loop (mirroring
        // `is_probe` below). This is a request-global observability
        // dimension ONLY (fed to `emit_class_observability`) -- the
        // per-target `use_forwarded_credential` flag, not this, drives
        // the model-rewrite bypass and the 401/403/429 terminal re-key,
        // so a coexisting own-credential Anthropic target in the same
        // chain keeps its normal auth recovery even on a request that
        // also carries a forwarded bearer.
        let is_forwarded = req.routectl_internal.forwarded_bearer.is_some();
        let ingress = ingress_dialect_of(&req);
        let chain_len = chain.len();
        let policy = self.policy_for(&req.model);
        // Availability-probe detection, computed ONCE: `max_tokens` is
        // stable across the chain (overlays never touch it). Claude
        // Code sends max_tokens=1 probes whose output is unread; on a
        // 429/529 these fast-fail instead of spraying retry+fallback
        // across the all-Anthropic chain. See `should_fallback`.
        let is_probe = is_probe_request(&req, &policy);
        // Auto-cache decision inputs: computed ONCE off the ORIGINAL req
        // (above the chain loop) so they are identical across every retry
        // and fallback target. The per-target capability + provider switch
        // are looked up inside the loop; the request-level facts here do
        // not vary by target.
        let auto_cache_plan = A::SURFACE.shapes_message().then(|| {
            let plan =
                AutoCacheRequestPlan::build(&req, self.config.cache.auto_emit_top_level_breakpoint);
            if plan.has_caller_breakpoints {
                self.warn_volatile_in_caller_prefix(&req);
            }
            plan
        });
        // Prefix-rewrite detector: ONCE per client request, off the ORIGINAL
        // req and above the chain loop for the same reason the auto-cache plan
        // is -- it observes the CLIENT's bytes, which do not vary by target,
        // and its output must never reach the per-target emission path.
        if A::SURFACE.shapes_message() {
            self.observe_prefix_epoch(&req, meta);
        }
        let mut last_err: Option<Error> = None;
        let mut any_admitted = false;
        // One learned-capability observation per request per
        // (state_key, feature): the error arm fires per attempt, so this
        // set stops a same-request retry from manufacturing a second
        // observation. See `observe_for_learning`.
        let mut learn_dedupe: HashSet<LearnDedupeKey> = HashSet::new();
        // One reasoning-drop fidelity WARN per client request: the emit sits
        // at the dispatch point (per-target, post-overlay) so it is
        // target-accurate, and this flag stops a same-provider retry or a
        // later fallback hop from repeating it.
        let mut reasoning_drop_warned = false;
        // `repair_budget` is the reactive-repair ceiling for THIS client
        // request, declared by the caller above the chain loop: each repair
        // kind gates itself to at most once per target, and this bounds their
        // SUM across every target the fallback walk reaches. Never declared
        // inside the loop -- a per-target budget lets an N-target chain pay N
        // repairs for one logical request. Every surface adapter draws from
        // this one request-scoped `repair_budget`.

        'chain: for (chain_idx, target) in chain.iter().enumerate() {
            let provider_name = target.provider_name.as_str();
            let state_key = target.state_key.as_str();
            let model = target.upstream.as_str();
            if !adapter.admits(target) {
                continue;
            }
            any_admitted = true;
            // Learned re-probe settle guard for THIS target, scoped to the
            // whole chain-iteration (persists across same-provider retries,
            // drops -> OtherError on any exit). Inert unless the filter
            // admitted at least one probe for this state_key; the move takes
            // ownership out of the set so the set's drop cannot settle it again.
            let learned_probe_guard = match probe_set.take(state_key) {
                Some(admissions) => LearnedProbeGuard::armed(
                    self.learned_capabilities.clone(),
                    admissions,
                    probe_set.surface,
                ),
                None => LearnedProbeGuard::inert(),
            };
            let mut learning = AttemptLearning::new(self, A::SURFACE, learned_probe_guard);
            let Some(provider) = target.provider.clone() else {
                let err = Error::UnknownProvider(provider_name.to_string());
                if !A::SURFACE.unknown_provider_walks() {
                    return Err(err);
                }
                last_err = Some(err);
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            };
            if !A::SURFACE.dedupes_learning_per_request() {
                learn_dedupe.clear();
            }
            // This entry is being dispatched: record it as the terminal
            // target (overwritten by a later entry on fallback) and count
            // the hop. `chain_idx` is the index into the resolved chain;
            // a non-zero index means at least one earlier entry was tried
            // or skipped, i.e. we fell back to reach here.
            meta.mark_target(target, &req.model);
            meta.fallback_count = chain_idx as u32;

            // Missing-bearer terminal guard: a forwarded target with NO
            // captured client bearer must fail cleanly BEFORE egress --
            // never an ambiguous upstream 401. Per-target (checked here,
            // inside the loop), so a mixed chain that never reaches this
            // target is unaffected.
            // Terminal on purpose, unlike the walking refusals below: a
            // client that sent no credential for a forwarded target must not
            // be served on a later target's own key instead.
            if let Some(err) = missing_forwarded_bearer_error(target, &req) {
                self.count_refusal_error(ingress, target, &err);
                return Err(err);
            }
            // A Gemini egress cannot translate a defective tool pairing; the
            // next target may forward it.
            if A::SURFACE.shapes_message()
                && let Some(err) = egress_pairing_refusal(target, pairing.as_ref())
            {
                self.count_refusal_error(ingress, target, &err);
                last_err = Some(err);
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            }

            let mut attempt_req = req.clone();
            // A forwarded target keeps the client's requested model
            // verbatim (model transparency); an own target rewrites to
            // this target's `upstream` as before.
            if !target.use_forwarded_credential {
                attempt_req.model = model.to_string();
                if A::SURFACE.shapes_message() {
                    declare_model_rename(&mut attempt_req, target, &req.model);
                }
            }
            // v0.6: layered config compose. The provider's
            // header_extras + payload_extras are looked up by
            // provider_name; the model's contribution lives on the
            // dispatch target.
            apply_layered_overlays(&self.config, target, &mut attempt_req);
            // INTERCEPTOR HOOK: strip runs after layered config compose,
            // before auto-cache / context reduction. Any future interceptor
            // over the per-attempt canonical clone goes here, ordered by
            // data-dependency. A strict-mode refusal leaves this target
            // unsent and walks on, since the next target may carry the
            // capability; a rolled-back post-strip hazard routes away the
            // same way.
            match self.apply_strip_interceptor(target, &mut attempt_req) {
                StripDecision::Proceed => {}
                StripDecision::StrictReject(err) => {
                    self.count_refusal_error(ingress, target, &err);
                    if !A::SURFACE.strict_reject_walks() {
                        return Err(err);
                    }
                    last_err = Some(err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue;
                }
                StripDecision::RouteAway(err) => {
                    self.count_refusal_error(ingress, target, &err);
                    if A::SURFACE.holds_recovered_errors() {
                        last_err = Some(err);
                    }
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue;
                }
            }
            // Envelope-field pre-flight: after overlays and the strip
            // interceptor, before provider translation. Plans from THIS
            // target's already-overlaid, already-stripped clone -- no
            // reactive repair has touched it yet at this point in the
            // loop, so it is exactly the base every fallback target
            // plans from independently.
            // Captured BEFORE pre-flight: an acting negative is what makes
            // pre-flight drop the field, and it is that field a re-check asks
            // about.
            let probe_fields = ProbeFieldCapture::of(self.field_repair_table(), &attempt_req);
            let (preflighted_req, preflight_records, field_canary) =
                self.plan_field_preflight(&attempt_req, target, A::SURFACE);
            attempt_req = preflighted_req;
            meta.field_preflight.extend(preflight_records);
            // The canary plan and the modified-request accounting guard live
            // for the whole chain ITERATION, not the attempt: a same-provider
            // retry is still the same request riding the same decision, and a
            // fallback hop replaces both by re-planning for the next target.
            // Held by the target's learner state so every exit path -- an
            // early return, a `break 'chain`, a dropped future -- releases the
            // claim and the in-flight count through RAII rather than through a
            // settlement site somebody has to remember.
            learning.hold_preflight(field_canary);
            if let Some(plan) = &auto_cache_plan {
                self.shape_message_attempt(target, &req, plan, &mut attempt_req, meta);
            }

            // Reasoning-replay carry admission: claim the single-flight slot
            // for every non-portable artifact scheme this request carries
            // toward the target lane. `Some` holds the guards and leaves the
            // carried variant intact; `None` either found nothing to repair or
            // already stripped `attempt_req` proactively (an acting negative or
            // a peer probe). Runs after every request-shaping step so the
            // gray-artifact count reflects the exact carried bytes. A proactive
            // strip re-stamps the calibration estimate, so the evidence
            // numerator describes the stripped payload the upstream will price.
            //
            // Envelope-field carry admission, at the SAME position and for the
            // same reason: the closed table plus this attempt's own fields
            // decide the identity, so the guard is claimed before dispatch and
            // BOTH settlements stay reachable (commit on a repaired success,
            // clear when the field is accepted). Nothing is mutated by it -- an
            // acting verdict simply refuses the slot and the attempt
            // dispatches unchanged.
            learning.admit_carries(
                target,
                &mut attempt_req,
                meta,
                adapter.field_settlement_mode(),
                Instant::now(),
            );

            let mut backoff = Duration::from_millis(policy.initial_backoff_ms);
            let mut attempts_made: u32 = 0;
            // Per-chain-entry one-shot auth-recovery flag. Set after a
            // 401 triggers `provider.on_auth_failure()`; ensures we
            // retry the SAME chain entry with the freshly-rotated
            // token at most once. Reset implicitly when the outer
            // 'chain loop moves to the next target. Note: if the
            // operator has the same provider in the chain twice
            // (different model overrides), each entry gets its own
            // flag -- the per-provider single-flight in OAuthStore
            // (double-check on access_token equality) is the safety
            // net that prevents redundant rotations across entries.
            let mut auth_retry_attempted = false;

            loop {
                // Per-attempt gate: rate limit + circuit breaker.
                // Charges one RPM token and (when half-open) claims the
                // probe slot. If the gate refuses, treat as a fallback
                // event for THIS provider and move to the next chain
                // entry -- retrying the same provider would just hit
                // the gate again.
                //
                // The returned guard OWNS the half-open claim this admission
                // took (inert for a closed breaker or a refusal), so the
                // cancellation backstop below is armed from the critical
                // section that acquired the slot and never from a re-read of
                // the shared half-open bit.
                let mut probe_guard = {
                    let (refusal, guard) = self.admit_dispatch(state_key, provider_name);
                    if let Some((gate_kind, gate_err)) = refusal {
                        log_gate_refusal(
                            provider_name,
                            target.nickname.as_deref().unwrap_or(""),
                            gate_kind,
                            &gate_err,
                            A::SURFACE.gate_blocked_message(),
                        );
                        if gate_kind != CREDENTIAL_PARKED_GATE_KIND
                            && !A::SURFACE.gate_refusal_walks()
                        {
                            return Err(gate_err);
                        }
                        // Keep the FIRST real error: a synthetic gate error
                        // (status 0 "circuit breaker open" / RPM) on a later
                        // chain entry must not overwrite an earlier entry's
                        // genuine upstream failure, or the client sees the
                        // synthetic error instead of the real 503/timeout.
                        // Between two parked seats the earlier deadline wins, and
                        // any other gate refusal replaces a parked one.
                        merge_gate_refusal(&mut last_err, gate_err);
                        if opts.disable_fallbacks {
                            break 'chain;
                        }
                        continue 'chain;
                    }
                    guard
                };
                // Read before any settle site disarms the guard: only a probe's
                // commit closes the breaker on a surface that does not record
                // success at commit.
                let was_half_open_probe = probe_guard.owns_slot();

                // `probe_guard` is the cancellation backstop (see
                // ProbeSlotGuard): if the gate admitted THIS dispatch as the
                // half-open probe, it frees the slot should the future be
                // dropped before an outcome arm settles it. Disarmed at each
                // settle below; inert + a no-op otherwise.

                // A repair is a fixed correctness branch, not a retry policy,
                // so its re-dispatch takes no backoff sleep.
                let skip_backoff = learning.take_skip_backoff();
                if attempts_made > 0 && !skip_backoff && A::SURFACE.sleeps_between_attempts() {
                    let jittered = add_jitter(backoff, policy.jitter_ms);
                    tokio::time::sleep(jittered).await;
                    backoff = mul_duration(backoff, policy.backoff_multiplier);
                }

                let attempt_policy = self.compose_attempt_policy(
                    &policy,
                    provider_name,
                    target.stream_first_byte_timeout_ms,
                );
                // Fidelity WARN at the dispatch point: reads the per-target
                // clone about to go upstream, at most once per request.
                if A::SURFACE.shapes_message() {
                    Self::warn_dropped_reasoning_dialect(
                        provider_name,
                        target.provider_kind,
                        &attempt_req,
                        &mut reasoning_drop_warned,
                    );
                }
                // Lazy probe activation at the ADMITTED boundary: the gate
                // has cleared and this attempt is about to go upstream, so the
                // lane is provably in use -- while a gate-refused or
                // breaker-parked request, which never reaches here, is not.
                // Deliberately NOT the success arm: a lane whose upstream is
                // erroring is exactly the one a probe should investigate, so
                // requiring a 2xx would keep the scheduler dark on the traffic
                // that most needs it. The fields come from the pre-flight
                // capture, so a field pre-flight dropped on an acting negative
                // is still the one re-checked; the beta context comes from
                // `attempt_req`, the per-target body actually being sent. Pure
                // bookkeeping: it enqueues at most one bounded job per lane and
                // capability and returns without dialing or awaiting, so this
                // request is not delayed and its outcome does not depend on it.
                self.on_admitted_request(target, &probe_fields, &attempt_req);
                // A surface that does not shape a message keeps the report
                // off the request, so its egress records nothing and the
                // attempt reads as having withheld nothing.
                let translation_report = if A::SURFACE.shapes_message() {
                    install_translation_report(&mut attempt_req)
                } else {
                    TranslationReport::default()
                };
                let beta_assembly = install_beta_assembly(&mut attempt_req);
                let (wire_tap, sample_charge) =
                    adapter.install_wire(self, target, &mut attempt_req);
                let invoked = adapter.invoke(target, provider.clone(), attempt_req.clone());
                let (result, deadline_fired) = match adapter.deadline(&attempt_policy) {
                    Some(deadline) => match tokio::time::timeout(deadline.after(), invoked).await {
                        Ok(resolved) => (resolved, false),
                        Err(_) => (Err(DeadlineFired(deadline).error(provider_name)), true),
                    },
                    None => (invoked.await, false),
                };
                attempts_made += 1;
                meta.attempt_count += 1;

                match result {
                    Ok(out) => {
                        self.settle_seat_success(
                            state_key,
                            A::SURFACE.records_success_on_commit() || was_half_open_probe,
                            &probe_guard,
                        );
                        probe_guard.disarm();
                        sample_charge.keep();
                        self.check_request_wire(target, &attempt_req, wire_tap.as_ref());
                        let ctx = CommitCtx {
                            router: self,
                            target,
                            client_req: &req,
                            attempt_req: &attempt_req,
                            wire_tap: wire_tap.as_ref(),
                            translation_report: &translation_report,
                            learning: &mut learning,
                            learn_dedupe: &mut learn_dedupe,
                            meta,
                        };
                        return Ok(adapter.on_commit(ctx, out));
                    }
                    Err(mut e) => {
                        let rejection_site = adapter.read_failed_wire(
                            self,
                            target,
                            &attempt_req,
                            wire_tap.as_ref(),
                            sample_charge,
                            &e,
                            meta,
                        );
                        // Read off the status before any lift or remap: see
                        // `is_transcript_rejection`.
                        let transcript_rejection =
                            is_transcript_rejection(&e, req.routectl_internal.transcript_defect);
                        let native_cf = learning.classify(&e, target.provider_kind);
                        let original_class = native_cf.class.clone();
                        let remap_candidate_status = upstream_status_for_remap(&e);
                        let (cf, remapped) =
                            apply_remap(native_cf, remap_candidate_status, &target.class_overrides);
                        let remap_status = if remapped {
                            remap_candidate_status
                        } else {
                            None
                        };
                        // A forwarded-credential TARGET that drew
                        // an upstream 401/403/429 is TERMINAL -- bypass BOTH
                        // the on_auth_failure refresh-and-retry (below) AND
                        // the fallback-chain hop, and surface the status
                        // verbatim. A request-scoped forwarded token has no
                        // refresh path and no credential to fall back to, so
                        // both recoveries are useless and wrong; the client
                        // owns its own retry/backoff. Keyed off the CURRENT
                        // target's `use_forwarded_credential`, not
                        // request-global bearer presence, so a coexisting
                        // own-creds Anthropic target in the same chain keeps
                        // normal auth recovery. Checked FIRST so it
                        // precedes auth-retry, same-provider retry, and
                        // fallback. Releases the half-open probe slot WITHOUT
                        // a breaker debit (mirrors the terminal path below):
                        // a forwarded-token failure is not this seat's health
                        // signal, so it must not trip routectl's breaker.
                        // Transport retries (5xx/network) are NOT in this set
                        // and fall through unchanged.
                        if target.use_forwarded_credential
                            && let Some(status) = forwarded_terminal_status(&e)
                        {
                            log_forwarded_auth_terminal(
                                status,
                                req.routectl_internal.inbound_session_key.is_some(),
                            );
                            probe_guard.release();
                            return Err(e);
                        }
                        // Auth-401 single-flight refresh: when the
                        // upstream rejects the token (typically a 401
                        // on a stale oauth:// credential that slipped
                        // past the near-expiry check), ask the provider
                        // to rotate via on_auth_failure, then retry
                        // exactly once. The OAuth store's per-provider
                        // mutex collapses concurrent 401-storms to a
                        // single token-endpoint POST. A non-Ok return
                        // means the OAuth identity is dead (revoked,
                        // network-failure to token endpoint); surface
                        // that immediately rather than walk the
                        // fallback chain over a known-broken
                        // credential.
                        if !auth_retry_attempted
                            && matches!(&e, Error::Upstream { status: 401, .. })
                        {
                            auth_retry_attempted = true;
                            tracing::debug!(
                                provider = provider_name,
                                model = %routectl_core::sanitize_for_log(target.nickname.as_deref().unwrap_or("")),
                                attempt = attempts_made,
                                "upstream 401; refreshing auth and retrying once",
                            );
                            // A refresh failure means the OAuth identity is
                            // dead; surface it without walking the chain. But
                            // first release any half-open probe slot this
                            // attempt claimed via the gate, or the breaker
                            // stays locked open until restart.
                            if let Err(refresh_err) = provider.on_auth_failure().await {
                                self.park_credential_on_rejection(
                                    state_key,
                                    &refresh_err,
                                    is_probe,
                                    probe_guard.credential_generation(),
                                );
                                probe_guard.release();
                                return Err(refresh_err);
                            }
                            // Refresh succeeded. Release the half-open probe
                            // slot this attempt claimed at the gate BEFORE the
                            // `continue` re-enters the loop and re-runs
                            // the gate. While this caller still holds the
                            // slot, the in-loop re-gate's `try_dispatch` sees
                            // `half_open_in_flight` and returns CircuitOpen,
                            // which would leave the breaker locked open until
                            // restart. Releasing here lets the re-gate claim a
                            // fresh slot (the per-attempt accounting the
                            // in-loop gate promises).
                            probe_guard.release();
                            // Preserve the genuine upstream 401 as last_err
                            // before re-gating. If the re-gate then refuses
                            // (CircuitOpen / RPM), the `last_err.is_none()`
                            // guard above keeps this real error rather than
                            // overwriting it with the synthetic status-0 gate
                            // error, so the client sees the true 401.
                            if A::SURFACE.holds_recovered_errors() {
                                last_err = Some(e);
                            }
                            continue;
                        }
                        if adapter.disposition(target, &e) == ErrorDisposition::WalkWithoutDebit {
                            probe_guard.release();
                            if opts.disable_fallbacks {
                                break 'chain;
                            }
                            continue 'chain;
                        }
                        // The learner repair branches: replay strip, then
                        // envelope field, then the canary's same-field retry,
                        // then the named beta withhold.
                        // A FIXED correctness branch, not a retry policy: each
                        // fires at most once per target, re-dispatches this
                        // same target with no backoff, and draws the shared
                        // per-request repair ceiling. They sit after auth
                        // recovery and BEFORE the terminal and fallback
                        // handling below, which a canary's provoked 400 would
                        // otherwise reach.
                        let untranslated = UntranslatedCapabilities::take_from(&translation_report);
                        let assembly = beta_assembly.take();
                        let rejection = Rejection {
                            err: &e,
                            class: &cf,
                            native_class: &original_class,
                            target,
                            client_req: &req,
                            site: rejection_site,
                            transcript_rejection,
                            beta_assembly: assembly.as_ref(),
                        };
                        if let FailureDecision::Retry { last_err_body_free } =
                            learning.on_rejection(rejection, &mut attempt_req, meta, repair_budget)
                        {
                            probe_guard.release();
                            // Preserve the genuine rejection as last_err before
                            // re-gating the repaired attempt: if the re-gate
                            // refuses (CircuitOpen / RPM), the
                            // `last_err.is_none()` guard keeps the real upstream
                            // rejection rather than surfacing the synthetic
                            // status-0 gate error.
                            if A::SURFACE.holds_recovered_errors() {
                                last_err = Some(last_err_body_free.unwrap_or(e));
                            }
                            continue;
                        }

                        if let Some(body_free) =
                            replay_rejection_body_free(&e, &cf.class, provider_name)
                        {
                            e = body_free;
                        }
                        let do_fallback =
                            should_fallback(&e, &cf.class, &policy, is_probe, transcript_rejection);
                        // The honored upstream reset for THIS error (clamped
                        // to the ceiling), computed once for both the park
                        // decision below and the in-loop sleep bump in the
                        // retry branch. `None` for every non-rate-limit error.
                        let reset_hint = rate_limit_reset_hint(&e, &policy);
                        let outcome = breaker_outcome(&e, &cf.class, reset_hint, is_probe);
                        // Probe fast-fail: a probe (max_tokens <=
                        // probe_max_tokens) that hit a rate-limit/overload
                        // (429/529) returns the status immediately via an
                        // explicit early return -- no retry, no fallback.
                        // The shared rule gives it no breaker debit, so it
                        // only frees the half-open slot it may have claimed
                        // at the gate.
                        if let Some(status) = probe_fast_fail(&e, is_probe) {
                            log_probe_fast_fail(
                                provider_name,
                                target.nickname.as_deref().unwrap_or(""),
                                status,
                                req.max_tokens,
                            );
                            self.apply_breaker_outcome(state_key, outcome, &cf.class);
                            probe_guard.settle(outcome);
                            // A probe fast-fails ACROSS distinct chain targets
                            // (walking an all-Anthropic chain is futile -- every
                            // hop shares the limit), but a rate-limited SEAT does
                            // not mean the pooled model is out of quota: hop to
                            // the next sibling seat of the SAME pool when one
                            // exists (and fallbacks are enabled). Carry the
                            // genuine 429/529 as last_err so a later synthetic
                            // gate error cannot mask it; fast-fail once the
                            // pool's seats are exhausted.
                            if !opts.disable_fallbacks
                                && A::SURFACE.health_error_walks()
                                && next_is_sibling_seat(&chain, chain_idx, target)
                            {
                                last_err = Some(e);
                                continue 'chain;
                            }
                            return Err(e);
                        }
                        // The breaker DEBIT keys off the failure CLASS, not
                        // the chain-hop decision: a seat looks unhealthy when
                        // it emits a transient-health failure (429 / 5xx /
                        // status-0 / overload), independent of whether the
                        // operator routes past that status. A non-debiting
                        // class (4xx caller error, capability, auth) that
                        // still falls back releases its slot without a debit
                        // in the fallback branch below.
                        let debits = class_debits(&cf.class);
                        let (_opened, breaker_effect) =
                            self.apply_breaker_outcome(state_key, outcome, &cf.class);
                        if debits {
                            probe_guard.disarm();
                        }
                        // Outside the debit gate: a credential rejection
                        // classifies Auth, which never debits.
                        self.park_credential_on_rejection(
                            state_key,
                            &e,
                            is_probe,
                            probe_guard.credential_generation(),
                        );
                        adapter.observe_failure(
                            self,
                            &FailureObservation {
                                err: &e,
                                cf: &cf,
                                original_class: &original_class,
                                remapped,
                                remap_status,
                                target,
                                fallback: do_fallback,
                                policy: &policy,
                                debit: debits,
                                is_probe,
                                is_forwarded,
                                ingress,
                            },
                        );
                        learning.observe_generic(
                            &e,
                            &cf,
                            remapped,
                            target,
                            &req,
                            &untranslated,
                            &mut learn_dedupe,
                            meta,
                        );
                        if opts.disable_fallbacks {
                            // Terminal error exit: free any half-open probe
                            // slot this attempt claimed. A no-op when a
                            // debiting class already routed through
                            // record_failure (which clears the slot).
                            probe_guard.release();
                            return Err(e);
                        }
                        let retry = same_provider_retry(
                            A::SURFACE,
                            &e,
                            &cf.class,
                            &policy,
                            attempts_made,
                            is_probe,
                            transcript_rejection,
                            outcome,
                            reset_hint,
                        );
                        let can_retry_here = matches!(retry, SameProvider::Retry { .. });
                        let facts = upstream_facts(&e);
                        tracing::debug!(
                            provider = provider_name,
                            state_key = %sanitize_for_log(state_key),
                            surface = A::SURFACE.as_str(),
                            attempt = attempts_made,
                            status = ?facts.status,
                            upstream_type = ?facts.upstream_type,
                            retry_after_ms = ?reset_hint.map(|d| d.as_millis()),
                            breaker_effect,
                            same_provider_retry = can_retry_here,
                            preserved_upstream_error = facts.status.is_some(),
                            "retry decision",
                        );
                        if can_retry_here {
                            tracing::debug!(
                                provider = provider_name,
                                model = %routectl_core::sanitize_for_log(target.nickname.as_deref().unwrap_or("")),
                                attempt = attempts_made,
                                error = ?e,
                                "retrying same provider",
                            );
                            // Keep the genuine upstream error as the running
                            // last_err before re-probing. If the re-gate then
                            // refuses (CircuitOpen / RPM), the `last_err.is_none()`
                            // guard above leaves this real error in place rather
                            // than overwriting it with the synthetic status-0
                            // gate error, so the client sees the true failure.
                            last_err = Some(e);
                            // Honor a SMALL non-probe upstream reset as the
                            // next in-loop sleep: bump `backoff` so the
                            // loop-top sleep waits at least the reset before
                            // re-probing the SAME provider.
                            if let SameProvider::Retry {
                                sleep_floor: Some(h),
                            } = retry
                            {
                                backoff = backoff.max(h);
                            }
                            // Free the half-open probe slot this attempt
                            // claimed before re-probing: the in-loop re-gate
                            // re-runs `try_dispatch`, which would otherwise see
                            // this caller's still-held slot as
                            // `half_open_in_flight` and return CircuitOpen,
                            // locking the breaker open forever (mirrors the
                            // auth-retry Ok path).
                            probe_guard.release();
                            continue;
                        }
                        // Done with this provider. Decide fallback vs propagate.
                        // An expired deadline walks on every surface: keyed on
                        // the walk's own deadline, never on the status-0 class
                        // it shares with a refused connection.
                        if do_fallback && (deadline_fired || A::SURFACE.health_error_walks()) {
                            let has_next = chain_idx + 1 < chain_len;
                            if has_next {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %routectl_core::sanitize_for_log(target.nickname.as_deref().unwrap_or("")),
                                    state_key = %sanitize_for_log(state_key),
                                    error = ?e,
                                    "fallback to next",
                                );
                            } else {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %routectl_core::sanitize_for_log(target.nickname.as_deref().unwrap_or("")),
                                    state_key = %sanitize_for_log(state_key),
                                    error = ?e,
                                    "chain exhausted; no fallback target available; request will fail",
                                );
                            }
                            // A fallbackable error whose class did NOT debit
                            // the breaker leaves the half-open probe slot
                            // armed; release it without a debit before the hop
                            // so every settle path frees the slot exactly once.
                            // A debiting class already settled + disarmed above.
                            if !debits {
                                probe_guard.release();
                            }
                            last_err = Some(e);
                            continue 'chain;
                        }
                        // Terminal non-fallbackable error. Free any half-open
                        // probe slot this attempt claimed so the breaker is
                        // not left locked open.
                        probe_guard.release();
                        return Err(e);
                    }
                }
            }
        }

        Err(adapter.exhausted(Exhaustion {
            last: last_err,
            alias: &req.model,
            any_admitted,
        }))
    }

    /// Shape one target's attempt as a message dispatch: context reduction,
    /// auto-cache placement and its observability, and the would-trim
    /// advisory. Runs once per chain entry, outside the attempt loop, so
    /// every retry on the target reuses identical bytes.
    fn shape_message_attempt(
        &self,
        target: &DispatchTarget,
        req: &ChatRequest,
        auto_cache_plan: &AutoCacheRequestPlan,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
    ) {
        let provider_name = target.provider_name.as_str();
        let model = target.upstream.as_str();
        let provider_cfg = self.config.providers.get(provider_name);
        apply_context_reduction(
            &self.config,
            provider_cfg,
            provider_name,
            model,
            fidelity_dialect(target, attempt_req),
            attempt_req,
            meta,
        );
        // Auto-cache: maybe inject the front + terminal cache markers
        // on THIS per-attempt clone, after overlays (the last
        // dispatch-time touch of any cache marker) and before the retry
        // loop, so every retry on this target reuses identical bytes.
        // Transactional: both markers validate as one candidate and
        // commit or discard together. Never mutates the original `req`;
        // any doubt sends un-injected.
        //
        // The K-gated verdict is consulted HERE, once per chain entry, so
        // it is constant across this target's retries and re-derived for
        // the next fallback target against its own row and triple.
        let k_suppressed = self.k_emission_suppressed(
            auto_cache_plan,
            target,
            req.routectl_internal.inbound_session_key.as_deref(),
        );
        let cache_decision = apply_auto_cache_placement(
            attempt_req,
            auto_cache_plan,
            cache_target_gates(provider_cfg),
            k_suppressed,
        );
        declare_cache_placement(
            attempt_req,
            target,
            cache_decision,
            auto_cache_plan.front_slot,
        );
        // Cache observability: stamp the per-marker decision tokens so
        // the outcome log and the usage ledger can see what was decided,
        // and emit the per-request decision at debug. No bodies /
        // secrets: only provider name, model id, and stable tokens.
        let front_token = cache_decision.front.strategy_str();
        let terminal_token = cache_decision.terminal.strategy_str();
        meta.cache_strategy = Some(terminal_token);
        meta.cache_front_decision = Some(front_token);
        meta.cache_terminal_decision = Some(terminal_token);
        tracing::debug!(
            provider = %routectl_core::sanitize_for_log(provider_name),
            model = %routectl_core::sanitize_for_log(model),
            strategy = terminal_token,
            front_decision = front_token,
            terminal_decision = terminal_token,
            "cache_auto_decision",
        );

        // Steady-state would-trim advisory: NON-MUTATING. Reads the
        // dispatched clone, prices the would-cut candidate, and records it
        // onto `meta`. Never touches `attempt_req` (no `apply_trim_plan`),
        // so what is sent upstream is byte-identical with or without this.
        self.record_would_trim(
            attempt_req,
            target.provider_kind,
            target.nickname.as_deref().unwrap_or(model),
            &target.model.effective_row,
            meta,
        );
    }
}

/// Run context reduction for one dispatch attempt: decide effectiveness,
/// minify, stamp `meta.reduction_strategy`, accumulate the counter ledger,
/// and emit the `context_reduction` debug log. Shared by the completion and
/// streaming dispatch paths so both stay byte-identical and observe
/// identically.
///
/// Must be called AFTER overlays and strictly BEFORE auto-cache so any
/// auto-emitted breakpoint covers the REDUCED bytes. `apply_json_minify`
/// computes the caller frozen floor itself (off this pre-auto-emit clone) and
/// only touches the mutable tail; it fails closed (no mutation on any doubt).
/// Effective only when the global switch is on AND the provider did not
/// explicitly opt out (`None` inherits the global).
///
/// Call site invariant: ONCE per chain-entry preparation, OUTSIDE the
/// same-target retry loop. A retry re-sends the request this call prepared,
/// so counting inside that loop would bill the same removed bytes twice;
/// walking to the next fallback entry prepares a fresh clone, which is a
/// genuinely new measurement and aggregates.
pub(super) fn apply_context_reduction(
    config: &crate::config::Config,
    provider_cfg: Option<&crate::config::ProviderEntry>,
    provider_name: &str,
    model: &str,
    wire: Option<IngressDialect>,
    attempt_req: &mut ChatRequest,
    meta: &mut DispatchMeta,
) {
    let effective = config.reduction.enabled
        && provider_cfg.and_then(crate::config::ProviderEntry::reduction_enabled) != Some(false);
    let before = wire.map(|_| Arc::clone(&attempt_req.messages));
    let outcome = if effective {
        Some(apply_json_minify(attempt_req))
    } else {
        None
    };
    if let (Some(ReductionOutcome::Applied(_)), Some(before)) = (&outcome, &before) {
        declare_reduction(attempt_req, wire, before);
    }
    let token = reduction_strategy_token(effective, outcome.as_ref());
    meta.reduction_strategy = Some(token);
    // Both payload-carrying outcomes describe a pass that actually examined
    // the tail, so both feed the ledger: `NothingToStrip` is a measured zero
    // for `strings_minified` / `bytes_saved` while still accounting for every
    // skipped and rejected target. `NoMutableTail` examined nothing and a
    // disabled pass never ran, so neither contributes -- the counters stay
    // `None` unless some dispatched target reduced.
    if let Some(ReductionOutcome::Applied(delta) | ReductionOutcome::NothingToStrip(delta)) =
        &outcome
    {
        accumulate_reduction_counters(meta, delta);
    }
    if let Some(ReductionOutcome::Applied(delta)) = &outcome {
        tracing::debug!(
            provider = %routectl_core::sanitize_for_log(provider_name),
            model = %routectl_core::sanitize_for_log(model),
            strategy = token,
            strings_minified = delta.strings_minified,
            bytes_saved = delta.bytes_saved,
            est_tokens_saved = delta.est_tokens_saved,
            "context_reduction",
        );
    }
}

/// Fold one reduction pass's ledger into `meta`'s four counters.
///
/// Additive across the fallback-entry preparations of one chain walk: a
/// `None` counter initializes from zero (so the first contributing entry
/// leaves a measured value, including a measured `Some(0)`), and later
/// entries saturate on top. Saturation over wrap keeps a counter monotone
/// under any pathological chain length -- a pinned ceiling is readable
/// evidence, a wrapped count is a lie.
fn accumulate_reduction_counters(meta: &mut DispatchMeta, delta: &ReductionDelta) {
    fn add(slot: &mut Option<u64>, n: usize) {
        *slot = Some(slot.unwrap_or(0).saturating_add(n as u64));
    }
    add(
        &mut meta.reduction_strings_compressed,
        delta.strings_minified,
    );
    add(&mut meta.reduction_strings_skipped, delta.strings_skipped);
    add(&mut meta.reduction_strings_rejected, delta.strings_rejected);
    add(&mut meta.reduction_bytes_saved, delta.bytes_saved);
}

/// Map a context-reduction decision to its stable operator-facing token.
/// These tokens are a CONTRACT (recorded in the usage DB and emitted in the
/// `context_reduction` log): do not rename or repurpose them, only add new
/// ones. The `skipped:` prefix groups the variants where reduction did not
/// mutate the request.
///
/// | condition                          | token                     |
/// |------------------------------------|---------------------------|
/// | reduction not effective            | `skipped:disabled`        |
/// | `ReductionOutcome::Applied`        | `applied`                 |
/// | `ReductionOutcome::NoMutableTail`  | `skipped:no-tail`         |
/// | `ReductionOutcome::NothingToStrip(_)` | `skipped:nothing-to-strip`|
/// | unrecognized future outcome        | `skipped:unknown`         |
///
/// `effective == false` short-circuits to `skipped:disabled` WITHOUT calling
/// `apply_json_minify`, so `outcome` is only consulted when reduction ran.
pub(super) const fn reduction_strategy_token(
    effective: bool,
    outcome: Option<&ReductionOutcome>,
) -> &'static str {
    if !effective {
        return "skipped:disabled";
    }
    match outcome {
        Some(ReductionOutcome::Applied(_)) => "applied",
        Some(ReductionOutcome::NoMutableTail) => "skipped:no-tail",
        Some(ReductionOutcome::NothingToStrip(_)) => "skipped:nothing-to-strip",
        // `ReductionOutcome` is `#[non_exhaustive]`; a future variant we do
        // not yet map means reduction RAN but produced an outcome this build
        // does not recognize -- which is distinct from "disabled", so record
        // it as its own token. `None` only happens when `effective == false`,
        // already handled above.
        _ => "skipped:unknown",
    }
}

/// The upstream status of a forwarded-credential auth/rate failure that
/// is TERMINAL for a forwarded (pure-passthrough) request: 401
/// (unauthorized), 403 (forbidden), 429 (rate limited). `None` for every
/// other error class.
///
/// A request-scoped forwarded token has no refresh path (routectl cannot
/// rotate a credential it does not own) and no sibling credential to fall
/// back to, so on such a status the router must bypass BOTH the
/// `on_auth_failure` refresh-and-retry AND the fallback-chain hop, and
/// surface the upstream response verbatim -- Claude Code owns its own
/// retry/backoff. Scoped to a forwarded TARGET ONLY: the caller gates
/// this on the current dispatch target's `use_forwarded_credential`
/// flag, so a coexisting own-credential target in the same chain keeps
/// the existing one-shot auth-refresh + fallback behavior unchanged.
/// Same-request TRANSPORT retries (5xx / network / status-0, which
/// reuse the same token) are NOT in this set, so they fall through to
/// the normal predicates untouched. Pattern-mirrors
/// `probe_fast_fail_status`: a pure decision helper; the caller owns
/// the `return` that short-circuits.
pub(super) const fn forwarded_terminal_status(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream {
            status: s @ (401 | 403 | 429),
            ..
        } => Some(*s),
        _ => None,
    }
}

/// WARN when a forwarded-token upstream auth/rate failure is surfaced
/// verbatim (terminal: no refresh-retry, no fallback hop). SAFE
/// dimensions ONLY: the upstream `status`, a fixed `credential_source`
/// token, and a boolean derived from whether an inbound session key was
/// captured -- NEVER the forwarded token itself, in a field or the
/// message. Mirrors `log_probe_fast_fail`: log-only, the caller owns the
/// short-circuiting `return`.
pub(super) fn log_forwarded_auth_terminal(status: u16, has_client_session_id: bool) {
    tracing::warn!(
        status,
        credential_source = "forwarded",
        has_client_session_id,
        "forwarded-token upstream auth failure surfaced verbatim; \
         bypassing on_auth_failure refresh and provider fallback",
    );
}

/// Missing-bearer terminal guard. `None` when `target` does not use a
/// forwarded credential, or the client's bearer WAS captured; `Some`
/// carries a clean terminal [`Error::Validation`] for the caller to
/// return immediately -- BEFORE any upstream touch of this target.
///
/// A forwarded target with no captured bearer (seam header absent,
/// capture gates closed, or the client simply never sent an
/// `Authorization` header) has no credential to authenticate with; an
/// upstream dispatch would either fail with a confusing anonymous
/// request or -- worse -- silently succeed against the wrong identity
/// if the egress falls back to some other implicit auth. Refusing here
/// gives the client an unambiguous 4xx instead of a passed-through
/// upstream 401. Per-target (the caller checks this inside the chain
/// loop, once per target about to be dispatched), so a mixed chain that
/// never reaches a forwarded target is unaffected. Emits a WARN with
/// SAFE dimensions only (reason, credential_source, provider_kind --
/// never the token, host, or provider name), mirroring the shape of the
/// deleted whole-chain gate's refusal log.
pub(super) fn missing_forwarded_bearer_error(
    target: &DispatchTarget,
    req: &ChatRequest,
) -> Option<Error> {
    if !target.use_forwarded_credential || req.routectl_internal.forwarded_bearer.is_some() {
        return None;
    }
    tracing::warn!(
        reason = "missing_forwarded_bearer",
        credential_source = "forwarded",
        provider_kind = target.provider_kind.unwrap_or("unknown"),
        "forwarded target has no captured client bearer; refusing before egress",
    );
    Some(Error::Validation(
        "forwarded target has no captured client bearer to authenticate this request \
         (reason=missing_forwarded_bearer)"
            .to_string(),
        RefusalReason::ForwardedBearerMissing,
    ))
}

pub(super) fn add_jitter(base: Duration, jitter_ms: u64) -> Duration {
    if jitter_ms == 0 {
        return base;
    }
    // Non-cryptographic jitter from the wall clock's sub-millisecond
    // bits. Suitable for retry-spreading; not for anything else.
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as u64);
    base.saturating_add(Duration::from_millis(nanos % jitter_ms))
}

pub(super) fn mul_duration(d: Duration, factor: f64) -> Duration {
    let nanos = d.as_nanos() as f64 * factor;
    Duration::from_nanos(nanos.min(u64::MAX as f64) as u64)
}

/// Whether the chain entry AFTER `chain_idx` is another seat of the SAME
/// pooled model as `target` -- i.e. a sibling seat a probe may fail over to
/// before fast-failing. All seats of one pool share the model `Arc`
/// (`dispatch_target_for_seat` clones the same `Arc<ResolvedModel>`), while
/// distinct chain entries carry distinct model Arcs, so pointer identity is
/// the exact same-pool discriminant.
pub(super) fn next_is_sibling_seat(
    chain: &[DispatchTarget],
    chain_idx: usize,
    target: &DispatchTarget,
) -> bool {
    chain
        .get(chain_idx + 1)
        .is_some_and(|next| std::sync::Arc::ptr_eq(&next.model, &target.model))
}
