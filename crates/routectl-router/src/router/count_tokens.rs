//! Token-counting dispatch path (no reducer/cache; independent of the would-trim seam).
//!
//! # Learning on this walk
//!
//! Each capable seat holds one [`AttemptLearning`] for the count surface, the
//! same engine the two messages walks hold per target, and every repair and
//! settlement here is the engine's: the walk only classifies through it, asks
//! [`AttemptLearning::on_rejection`] after auth recovery whether a repair
//! re-dispatches the seat, hands a rejection that took none to
//! [`AttemptLearning::observe_generic`], and settles a count through
//! [`AttemptLearning::on_success`]. Three things the engine does differently
//! on this surface, all decided by the surface it was built for:
//!
//! - **No generic verdict is minted.** A token count validates only what the
//!   count endpoint projects, which is narrower than what a message dispatch
//!   sends, so a count rejection only counts as one the field parser did not
//!   localize; it never teaches a capability negative.
//! - **The replay carry settles by drop.** A repaired replay carry releases
//!   its slots and learns nothing, since a count success is not the
//!   optimistic probe the two-phase learn settles. The degradation summary
//!   records that the repair served without claiming anything was learned.
//! - **Field settlement follows the entry point.** Through
//!   [`Router::count_tokens_with_meta`] the field carry runs the full
//!   two-phase settlement and both a committed and a cleared verdict ride out
//!   on the returned [`DispatchMeta`]; through the result-only
//!   [`Router::count_tokens`] it repairs for the answer it returns and
//!   settles nothing, because that caller keeps no meta and a settlement
//!   whose event row never reached the ledger would leave the shared registry
//!   mutated with no record of it -- the state a warm rebuild resurrects a
//!   verdict from.
//!
//! Each provider call carries a bare wire tap when the request holds the
//! client's body, so a rejection can be laid to a declared routectl edit
//! and reverted; the tap is never admitted to the wire check.
//!
//! A count admits no untranslated-capability report, and the filter's
//! re-probe admissions are settled before the walk, so the engine holds an
//! inert re-probe guard and settles successes against an empty report. A
//! named beta rejection is repaired for the count it returns and never
//! learned. A rejection laid to a defective transcript is never repaired:
//! the engine proceeds without consulting a proposer.

use std::collections::HashSet;
use std::time::Instant;

use routectl_core::provider_kind::config as config_kind;
use routectl_core::{ChatRequest, Error, Result, TokenCount, sanitize_for_log};

use super::beta_report_learn::install_beta_assembly;
use super::capability_learn::LearnDedupeKey;
use super::capability_observe::UntranslatedCapabilities;
use super::class_observe::{DispatchSurface, class_label, matched_by_label, upstream_facts};
use super::credential_park::{log_gate_refusal, merge_gate_refusal};
use super::dispatch::{
    apply_remap, emit_replay_degradation, forwarded_terminal_status, is_capability_error,
    log_forwarded_auth_terminal, missing_forwarded_bearer_error, replay_rejection_body_free,
    upstream_status_for_remap,
};
use super::field_preflight::emit_field_preflight;
use super::field_repair::{FieldSettlementMode, emit_field_repair};
use super::learn_engine::{AttemptLearning, FailureDecision, Rejection};
use super::local_refusal::ingress_dialect_of;
use super::outcome_rule::{BreakerOutcome, breaker_outcome, rate_limit_reset_hint};
use super::repair_budget::RepairBudget;
use super::runtime_gate::{LearnedProbeGuard, is_probe_request};
use super::transcript_defect::{is_transcript_rejection, record_transcript_defect};
use super::{
    CountedTokens, DispatchMeta, DispatchTarget, Router, StripDecision, apply_layered_overlays,
};
use routectl_providers::anthropic_family::{AnthropicFamily, anthropic_family};

/// Whether one dispatch seat can serve a token count, decided from its
/// egress kind together with the upstream model id that kind would be
/// asked to count for. Kinds are the `kind = "..."` discriminant from
/// `ProviderEntry::kind_str`.
///
/// Every seat this admits shares the SAME Anthropic tokenizer family,
/// and that is what makes the walk in [`Router::count_tokens`] safe:
/// `anthropic-api` is Claude-only, and a `bedrock` seat is admitted
/// only when its upstream model id is provably an Anthropic-family id.
/// A count produced by a different tokenizer than the one the caller's
/// request bills against is a wrong number delivered as a success, with
/// no error anywhere -- so the family check happens on the seat, before
/// dispatch, and never on the answer.
///
/// A model id that proves neither family (an inference-profile ARN,
/// which may carry no vendor token) is refused for the same reason: the
/// ARN says nothing about the tokenizer behind it, and callers size
/// context windows with this number, so a clean 501 is the better
/// answer than a plausible wrong count.
///
/// The `bedrock` arm needs no feature gate: its kind constant exists in every
/// build, and a build without that egress never produces a seat reaching it.
pub(super) fn seat_can_count_tokens(kind: Option<&str>, upstream: &str) -> bool {
    match kind {
        Some(config_kind::ANTHROPIC_API) => true,
        Some(config_kind::BEDROCK) => matches!(anthropic_family(upstream), AnthropicFamily::Yes),
        _ => false,
    }
}

/// Outcome of dispatching `count_tokens` to one capable seat, driving
/// the walk in [`Router::count_tokens`].
pub(super) enum CountSeatOutcome {
    /// The seat returned a token count; return it to the caller.
    Count(TokenCount),
    /// A definitive result for this request -- return the error verbatim.
    /// Covers a settled health error (breaker already debited/parked), a
    /// non-fallbackable 4xx, a breaker/RPM gate block, or an auth-refresh failure.
    Terminal(Error),
    /// The seat's credential is parked; it was skipped without a dial.
    Parked(Error),
    /// The seat was admitted as capable but its upstream cannot count (local
    /// `NotImplemented` or a wire 501). The probe slot was released
    /// without a breaker debit; advance to the next capable seat.
    Capability,
}

impl Router {
    /// Probe call: route a request to a count_tokens-CAPABLE provider in
    /// the dispatch chain and call `Provider::count_tokens`. Used by
    /// claude-code's context-budget display via the
    /// `/v1/messages/count_tokens` endpoint.
    ///
    /// Capability walk (not a try-and-fallback over health): the chain is
    /// scanned for targets `seat_can_count_tokens` admits. Incapable
    /// targets are skipped BEFORE dispatch (DEBUG log, no upstream call,
    /// no breaker account); a capability skip is operator-known config,
    /// not upstream health, so it must not touch the breaker. This
    /// mirrors `filter_chain_by_features` discipline.
    ///
    /// Why walking is safe for tokenizer correctness: every seat the
    /// predicate admits is provably Anthropic-family, so every capable
    /// target the walk can select uses the SAME tokenizer family.
    /// Walking therefore never reintroduces the wrong-tokenizer hazard
    /// -- it only steps over seats that cannot count, or cannot be
    /// proven to count in the caller's tokenizer.
    ///
    /// Once a CAPABLE target is selected, the outcome decides the walk:
    ///
    /// - `Ok` -> return the count.
    /// - A CAPABILITY error (`is_capability_error`: local
    ///   `NotImplemented`, or a WIRE 501 from an admitted seat whose
    ///   upstream cannot count) -> release the probe slot WITHOUT debiting
    ///   the breaker, then advance to the NEXT capable seat in the
    ///   already-resolved chain. This is the incident fix: a
    ///   count_tokens-only capability signal must never be recorded as
    ///   health on the per-seat breaker that completions gate on.
    /// - A 401 -> single-flight `on_auth_failure` refresh + one retry of
    ///   the SAME seat.
    /// - Any OTHER fallbackable HEALTH error (429 / 5xx / status-0) ->
    ///   debit-or-park and propagate (NO walk -- health fallback stays
    ///   reserved for the messages path).
    /// - A non-fallbackable 4xx -> release the probe slot and propagate.
    ///
    /// The walk is bounded and single-visit: each seat is dispatched to at
    /// most once, plus at most one 401 auth-retry of that same seat, plus
    /// the learning engine's repair re-dispatches of that same seat. The
    /// auth retry is per SEAT, while every repair draws a per-REQUEST ceiling
    /// shared with the two messages walks, so total upstream calls never exceed
    /// `2 * chain.len() + REPAIRS_PER_REQUEST` -- the repair term is added
    /// once for the whole walk, not once per seat. When no capable seat
    /// serves a count (none capable, or every capable seat
    /// returned a capability error), this returns
    /// `Error::NotImplemented` naming the alias -- the handler maps that to
    /// a stable 501, and the last upstream's raw 501 body is never leaked
    /// to the client.
    ///
    /// count_tokens calls consume the same RPM bucket and honor the same
    /// circuit breaker as messages calls: the gate runs on EACH seat
    /// before its upstream is touched, so a walk cannot fan across seats
    /// to bypass an operator rate limit or an open breaker.
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        // NON-SETTLING deliberately, and this is a behavioral difference from
        // `count_tokens_with_meta`, not a convenience wrapper over it. The
        // caller of this signature keeps no `DispatchMeta`, so a settlement's
        // event row would be produced and dropped -- a persisted verdict with
        // no ledger record, which is what a warm rebuild resurrects from. The
        // walk still computes (and may still repair) for the count it returns;
        // it just cannot learn or clear. A caller that wants the verdict
        // lifecycle uses `count_tokens_with_meta` and drains its rows.
        self.count_tokens_settling(req, FieldSettlementMode::NonSettling)
            .await
            .result
    }

    /// Route a token count and return the result paired with its
    /// router-scoped [`DispatchMeta`] -- the SETTLING variant of this walk.
    /// [`Router::count_tokens`] is not a thin wrapper over it: that one is
    /// non-settling, which is a behavioral difference rather than a convenience.
    ///
    /// The meta is what carries this walk's reactive-repair settlement rows out
    /// to the capability-event ledger. `commit` / `clear` mutate the SHARED
    /// learned registry, so a caller that produced those rows and dropped them
    /// would leave a persisted verdict with no ledger record -- exactly the
    /// state a warm rebuild resurrects a cleared verdict from. A caller of THIS
    /// signature must therefore drain the meta, the same way the messages
    /// walks' callers drain theirs; a caller that will not is asking for
    /// [`Router::count_tokens`], which settles nothing by construction.
    #[must_use]
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn count_tokens_with_meta(&self, req: ChatRequest) -> CountedTokens {
        self.count_tokens_settling(req, FieldSettlementMode::Settling)
            .await
    }

    /// The walk both public entry points share, parameterized by whether the
    /// caller can drain a settlement row.
    ///
    /// The mode is threaded to the admission rather than checked at the
    /// settlement: a non-settling walk then never claims a single-flight slot
    /// at all, so it cannot take one from a sibling request that could have
    /// settled it, and cannot learn by forgetting to drop a plan.
    async fn count_tokens_settling(
        &self,
        req: ChatRequest,
        mode: FieldSettlementMode,
    ) -> CountedTokens {
        let mut meta = DispatchMeta::for_alias(&req.model);
        // Reactive-repair ceiling for THIS client request, declared above the
        // per-seat walk exactly as `complete_with_options` declares it, and
        // threaded by `&mut` into every seat. Constructing it inside
        // `count_tokens_try_seat` would be the per-seat reset the shared
        // ceiling exists to remove; holding it here reads its spend once on
        // every exit of the walk.
        let mut repair_budget = RepairBudget::per_request();
        let result = self
            .count_tokens_inner(req, mode, &mut meta, &mut repair_budget)
            .await;
        self.metrics
            .record_repairs_per_request(repair_budget.spent());
        emit_replay_degradation(&meta);
        emit_field_repair(&meta);
        emit_field_preflight(&meta);
        CountedTokens { meta, result }
    }

    /// The walk body for [`Router::count_tokens_with_meta`]. Mutates `meta` as
    /// the seats are visited, so the caller's `meta` is correct at every
    /// return -- including the terminal 501 path.
    async fn count_tokens_inner(
        &self,
        mut req: ChatRequest,
        mode: FieldSettlementMode,
        meta: &mut DispatchMeta,
        repair_budget: &mut RepairBudget,
    ) -> Result<TokenCount> {
        // No seat this walk admits is a Gemini egress, so a defect is only
        // recorded and the transcript forwarded.
        record_transcript_defect(&mut req);
        let (chain, probe_admissions) =
            self.dispatch_chain_for_request(&req, DispatchSurface::CountTokens)?;
        // A token-count is not a messages-capability test, so a re-probe the
        // filter admitted here settles OtherError: release the in_flight slot
        // and leave the entry expired for the next real request to re-probe,
        // never latching it in flight.
        let now = Instant::now();
        for admission in probe_admissions {
            if matches!(
                self.learned_capabilities
                    .record_probe_outcome_in_generation(
                        // The generation the FILTER granted this admission
                        // under. Sampling here instead would read after
                        // `dispatch_chain_for_request`, which a boundary can
                        // span.
                        admission.generation,
                        &admission.learned_key,
                        &admission.feature,
                        admission.provider_kind,
                        crate::learned_capability::ProbeOutcome::OtherError,
                        now,
                    ),
                crate::learned_capability::GenerationOutcome::Stale
            ) {
                tracing::debug!(
                    event = "probe_settlement_stale",
                    surface = "count_tokens",
                    state_key = %sanitize_for_log(&admission.state_key),
                    capability_key = %admission.feature,
                    attempted_outcome = "other_error",
                    "probe settlement refused: its admission predates the live \
                     capability generation"
                );
            }
        }
        let mut saw_capable = false;
        let outcome = self
            .count_tokens_walk(&req, chain, &mut saw_capable, repair_budget, mode, meta)
            .await;
        if let Some(result) = outcome {
            return result;
        }
        // Two distinct terminal shapes, both mapping to a 501 at the
        // handler: no capable seat existed at all, versus capable
        // seats existed but every one returned a capability error.
        let detail = if saw_capable {
            "count_tokens: all capable providers returned a capability error (cannot count)"
        } else {
            tracing::warn!(
                alias = %sanitize_for_log(&req.model),
                "alias chain has no count_tokens-capable provider; \
                 no target in chain can count tokens for its upstream model",
            );
            "count_tokens: no count_tokens-capable provider in chain"
        };
        Err(Error::NotImplemented(req.model.clone(), detail.into()))
    }

    /// The per-seat walk itself: `Some(result)` when a seat settled the
    /// request or was skipped as parked, else `None` (the caller's 501).
    ///
    /// Split out of [`Router::count_tokens`] so the aggregated degradation
    /// WARN fires on every exit of the walk rather than only the exhausted
    /// one -- the same reason `complete_with_options` wraps `complete_inner`.
    async fn count_tokens_walk(
        &self,
        req: &ChatRequest,
        chain: Vec<DispatchTarget>,
        saw_capable: &mut bool,
        repair_budget: &mut RepairBudget,
        mode: FieldSettlementMode,
        meta: &mut DispatchMeta,
    ) -> Option<Result<TokenCount>> {
        let mut parked: Option<Error> = None;
        let ingress = ingress_dialect_of(req);
        for candidate in chain {
            if !seat_can_count_tokens(candidate.provider_kind, &candidate.upstream) {
                tracing::debug!(
                    provider = %routectl_core::sanitize_for_log(&candidate.provider_name),
                    kind = candidate.provider_kind.unwrap_or("unknown"),
                    model = %routectl_core::sanitize_for_log(
                        candidate.nickname.as_deref().unwrap_or("")
                    ),
                    "provider skipped: seat cannot count_tokens",
                );
                continue;
            }
            *saw_capable = true;
            let egress = candidate.egress_kind;
            match self
                .count_tokens_try_seat(req, candidate, repair_budget, mode, meta)
                .await
            {
                CountSeatOutcome::Count(tc) => return Some(Ok(tc)),
                CountSeatOutcome::Terminal(e) => {
                    if let Some(reason) = e.refusal_reason() {
                        self.count_egress_refusal(ingress, egress, reason);
                    }
                    return Some(Err(e));
                }
                CountSeatOutcome::Parked(e) => merge_gate_refusal(&mut parked, e),
                // Capability error: the seat was admitted as capable but its
                // upstream cannot count. The slot was already released
                // without a breaker debit; advance to the next capable
                // seat in the already-resolved chain (single-visit,
                // never re-resolved or re-queued).
                CountSeatOutcome::Capability => continue,
            }
        }
        parked.map(Err)
    }

    /// Dispatch `count_tokens` to ONE already-selected capable seat and
    /// classify the outcome for the walk in [`Router::count_tokens`].
    ///
    /// PROBE-SLOT INVARIANT: on every exit the half-open slot this seat
    /// claimed at the gate is settled exactly once, and `probe_guard`
    /// is disarmed only AFTER that settle (never before, never instead).
    /// A `Capability` return releases the slot BEFORE returning, so the
    /// caller's next-seat gate can claim a fresh slot without contending
    /// with this seat's. `auth_retry_attempted` is a fresh per-seat local,
    /// so advancing to a new seat resets it -- safe because seats are
    /// single-visit.
    ///
    /// `repair_budget` is the opposite case and is therefore passed in: the
    /// reactive-repair ceiling is per REQUEST, so a fresh one per seat would
    /// let an N-seat walk pay N repairs for one logical token count.
    async fn count_tokens_try_seat(
        &self,
        req: &ChatRequest,
        target: DispatchTarget,
        repair_budget: &mut RepairBudget,
        mode: FieldSettlementMode,
        meta: &mut DispatchMeta,
    ) -> CountSeatOutcome {
        let provider = match target.provider.clone() {
            Some(p) => p,
            None => {
                return CountSeatOutcome::Terminal(Error::UnknownProvider(
                    target.provider_name.clone(),
                ));
            }
        };
        let provider_name = target.provider_name.as_str();
        let model_label = target.nickname.as_deref().unwrap_or("");

        // Missing-bearer terminal guard (see `complete_inner`): a
        // forwarded seat with NO captured client bearer must fail
        // cleanly before any upstream touch, never an ambiguous
        // upstream 401.
        if let Some(err) = missing_forwarded_bearer_error(&target, req) {
            return CountSeatOutcome::Terminal(err);
        }

        // Apply the same per-attempt overlays the messages path does so
        // header_extras / payload_extras are consistent -- notably the
        // `anthropic-beta` surface count_tokens must observe or the
        // upstream may reject a request /v1/messages would accept.
        let mut attempt_req = req.clone();
        // See `complete_inner`: a forwarded target keeps the client's
        // requested model verbatim instead of rewriting to `upstream`.
        if !target.use_forwarded_credential {
            attempt_req.model = target.upstream.clone();
        }
        apply_layered_overlays(&self.config, &target, &mut attempt_req);
        // INTERCEPTOR HOOK (see `complete_inner`): strip runs after layered
        // config compose so the estimated prefix matches the shipped
        // prefix. Strict refusal is a terminal 400; a rolled-back hazard
        // advances to the next capable seat (the count_tokens route-away).
        match self.apply_strip_interceptor(&target, &mut attempt_req) {
            StripDecision::Proceed => {}
            StripDecision::StrictReject(err) => return CountSeatOutcome::Terminal(err),
            StripDecision::RouteAway(err) => {
                self.count_refusal_error(ingress_dialect_of(req), &target, &err);
                return CountSeatOutcome::Capability;
            }
        }

        // Envelope-field pre-flight (see `complete_inner`): same seam,
        // `CountTokens` surface. `attempt_req` at this point carries only
        // this seat's overlay + strip, never a prior seat's clone or a
        // reactive repair, so it is the same "original for this target"
        // base the messages walks plan from. The returned request REPLACES
        // `attempt_req`, so from here on this seat has exactly one body:
        // the remote count dispatches it, and every local read of it
        // measures that same body rather than an unplanned one.
        // Captured before pre-flight -- see `complete_inner`.
        let probe_fields = super::probe_payload_capture::ProbeFieldCapture::of(
            self.field_repair_table(),
            &attempt_req,
        );
        // The filter's re-probe admissions were settled up front, so the
        // engine holds no re-probe guard of its own here.
        let mut learning = AttemptLearning::new(
            self,
            DispatchSurface::CountTokens,
            LearnedProbeGuard::inert(),
        );
        let (preflighted_req, preflight_records, field_preflight_plan) =
            self.plan_field_preflight(&attempt_req, &target, DispatchSurface::CountTokens);
        attempt_req = preflighted_req;
        meta.field_preflight.extend(preflight_records);
        learning.hold_preflight(field_preflight_plan);

        // Carry admission at the ANALOGOUS position the two messages walks
        // use: after every request-shaping step and before the seat's attempt
        // loop, so the gray-artifact count describes the exact carried bytes.
        // The replay admission may strip `attempt_req` proactively, in which
        // case the stripped variant is what this seat counts.
        learning.admit_carries(&target, &mut attempt_req, meta, mode, Instant::now());

        let mut auth_retry_attempted = false;
        let mut attempts_made: u32 = 0;
        // Per seat is per target: seats are single-visit, so this dedupes the
        // seat's own retries exactly as the messages walks' per-request set does.
        let mut learn_dedupe: HashSet<LearnDedupeKey> = HashSet::new();
        loop {
            // Per-attempt gate: rate limit + circuit breaker. Runs on THIS
            // seat before its upstream is touched (and again on the 401
            // retry), so a capability walk cannot fan across seats to
            // bypass an operator rate limit or an open breaker.
            //
            // The returned guard OWNS the half-open claim this admission took
            // (inert for a closed breaker or a refusal); it is the cancellation
            // backstop (see ProbeSlotGuard) and frees the slot if this future is
            // dropped before an outcome arm settles it.
            let (refusal, mut probe_guard) =
                self.admit_dispatch(&target.state_key, &target.provider_name);
            if let Some((gate_kind, gate_err)) = refusal {
                log_gate_refusal(
                    &target.provider_name,
                    model_label,
                    gate_kind,
                    &gate_err,
                    "count_tokens gate blocked",
                );
                return CountSeatOutcome::of_gate_refusal(gate_kind, gate_err);
            }

            // Lazy probe activation at the ADMITTED boundary, the same
            // position and for the same reasons as the two messages walks:
            // the gate has cleared, every request-shaping step has run, and
            // this seat is about to be dialed. A count_tokens request is
            // real admitted traffic on this lane, so a lane that only ever
            // sees counting still activates -- reading the pre-flight
            // capture and the per-target body actually being sent.
            self.on_admitted_request(&target, &probe_fields, &attempt_req);
            let beta_assembly = install_beta_assembly(&mut attempt_req);
            let wire_tap = Self::install_count_wire_tap(&mut attempt_req);
            let result = provider.count_tokens(attempt_req.clone()).await;
            attempts_made += 1;
            match result {
                Ok(tc) => {
                    self.settle_seat_success(&target.state_key, true, &probe_guard);
                    probe_guard.disarm();
                    // Settle every carry the engine holds against this
                    // count. Read from the CLIENT request, as the messages
                    // walks do, so a repaired commit keys on what the caller
                    // asked for rather than the repaired body.
                    // A token count carries no response evidence, so the
                    // count surface never yields a scope.
                    let _no_evidence = learning.on_success(
                        &UntranslatedCapabilities::default(),
                        &target,
                        req,
                        &mut learn_dedupe,
                        meta,
                    );
                    return CountSeatOutcome::Count(tc);
                }
                Err(mut e) => {
                    // Classified ONCE at the top of the arm (as both messages
                    // walks do), because the engine's repair position below
                    // and the health settle further down must read the SAME
                    // effective class. The carried-artifact signal comes from
                    // the engine's replay carry, so a proven replay rejection
                    // lifts here and nowhere else.
                    let policy = self.policy_for(&req.model);
                    // Read off the status before any lift or remap: see
                    // `is_transcript_rejection`.
                    let transcript_rejection =
                        is_transcript_rejection(&e, req.routectl_internal.transcript_defect);
                    let native_cf = learning.classify(&e, target.provider_kind);
                    // Retained before the remap: the engine's path-keyed
                    // proposers read what the UPSTREAM said, while routing
                    // reads the operator's override.
                    let original_class = native_cf.class.clone();
                    let (cf, remapped) = apply_remap(
                        native_cf,
                        upstream_status_for_remap(&e),
                        &target.class_overrides,
                    );
                    // A forwarded-credential 401/403/429 is TERMINAL
                    // -- bypass the on_auth_failure refresh (below) AND any
                    // health park/debit, and surface verbatim as a Terminal
                    // outcome (count_tokens never WALKS on health errors, so
                    // "no fallback" here means also no breaker debit/park).
                    // Keyed off the TARGET's `use_forwarded_credential`, not
                    // request-global bearer presence (see `complete_inner`
                    // for the full rationale). Release the half-open slot
                    // without a breaker debit.
                    if target.use_forwarded_credential
                        && let Some(status) = forwarded_terminal_status(&e)
                    {
                        log_forwarded_auth_terminal(
                            status,
                            req.routectl_internal.inbound_session_key.is_some(),
                        );
                        probe_guard.release();
                        return CountSeatOutcome::Terminal(e);
                    }
                    // Auth-401 single-flight refresh: rotate the token and
                    // retry the SAME seat exactly once. Release the slot
                    // this attempt claimed BEFORE the `continue` re-enters
                    // the loop and re-gates (or the re-gate sees
                    // half_open_in_flight and returns CircuitOpen, locking
                    // the breaker until restart).
                    if !auth_retry_attempted && matches!(&e, Error::Upstream { status: 401, .. }) {
                        auth_retry_attempted = true;
                        tracing::debug!(
                            provider = provider_name,
                            model = model_label,
                            attempt = attempts_made,
                            "count_tokens 401; refreshing auth and retrying once",
                        );
                        if let Err(refresh_err) = provider.on_auth_failure().await {
                            self.park_counted_seat(&target, req, &refresh_err, &probe_guard);
                            probe_guard.release();
                            return CountSeatOutcome::Terminal(refresh_err);
                        }
                        probe_guard.release();
                        continue;
                    }

                    // CAPABILITY error, checked BEFORE the engine and before
                    // the health settle so a wire-501 can never reach
                    // record_failure or a learner: the seat was admitted as
                    // capable but its upstream cannot count. Release the probe
                    // slot WITHOUT debiting the breaker, then let the caller
                    // walk to the next capable seat.
                    if is_capability_error(&e) {
                        if let Error::Upstream { status: 501, .. } = &e {
                            // DEBUG, not WARN: post-fix this is the
                            // steady-state happy path (every count_tokens
                            // for a passthrough alias 501s here and walks),
                            // so at WARN it would flood the log on every
                            // client poll and bury real warnings. The new
                            // count_tokens tests guard the regression.
                            tracing::debug!(
                                provider = provider_name,
                                state_key = %sanitize_for_log(&target.state_key),
                                status = 501,
                                "count_tokens got wire-501 from admitted target; \
                                 treating as capability, not debiting breaker",
                            );
                        }
                        probe_guard.release();
                        return CountSeatOutcome::Capability;
                    }

                    // The learning engine's repair position, after auth
                    // recovery and before the health settle, as on the two
                    // messages walks. A repair edits `attempt_req` in place,
                    // so the repaired body is the one sent upstream and the
                    // one whose count is returned. Slot handling mirrors the
                    // 401 recovery above: release the half-open probe slot
                    // before the `continue` re-gates, and never debit the
                    // breaker -- a repairable rejection is a caller-shaped
                    // fault, not this seat's health signal.
                    let site = self.read_count_rejection_site(
                        &target,
                        &attempt_req,
                        wire_tap.as_ref(),
                        &e,
                    );
                    let assembly = beta_assembly.take();
                    let rejection = Rejection {
                        err: &e,
                        class: &cf,
                        native_class: &original_class,
                        target: &target,
                        client_req: req,
                        site,
                        transcript_rejection,
                        beta_assembly: assembly.as_ref(),
                    };
                    if let FailureDecision::Retry { .. } =
                        learning.on_rejection(rejection, &mut attempt_req, meta, repair_budget)
                    {
                        probe_guard.release();
                        continue;
                    }
                    learning.observe_generic(
                        &e,
                        &cf,
                        remapped,
                        &target,
                        req,
                        &UntranslatedCapabilities::default(),
                        &mut learn_dedupe,
                        meta,
                    );
                    // No repair took, so this error can now reach the CALLER.
                    // A replay rejection's upstream body echoes the reasoning
                    // artifact it objected to, so rebuild it body-free first --
                    // the same helper and the same position the two messages
                    // walks use. A non-replay class is left untouched (no
                    // clone on the common path).
                    if let Some(body_free) =
                        replay_rejection_body_free(&e, &cf.class, provider_name)
                    {
                        e = body_free;
                    }

                    // Health error. Mirror `complete_with_options`: the
                    // breaker DEBIT keys off the failure CLASS, not the
                    // fallback decision. A transient-health class (429 / 5xx
                    // / status-0 / overload) debits or parks by the rule every
                    // surface shares (`breaker_outcome`); a probe's 429/529
                    // and a caller-shaped 4xx release the slot without a
                    // debit, so a repeated
                    // non-retryable 4xx here cannot trip the per-seat breaker
                    // that also gates completions and streams. Either way this
                    // propagates -- health fallback stays reserved for the
                    // messages path, so a 429 here does NOT walk.
                    let reset_hint = rate_limit_reset_hint(&e, &policy);
                    let is_probe = is_probe_request(req, &policy);
                    let outcome = breaker_outcome(&e, &cf.class, reset_hint, is_probe);
                    let debit = outcome != BreakerOutcome::None;
                    // The class/remap/debit decision on the token-count path was
                    // otherwise silent (unlike the messages path, which emits a
                    // class-decision event at every error arm). One INFO event at
                    // the settle point makes a count_tokens breaker debit / park
                    // triageable. Safe dimensions only -- NEVER a body or prompt.
                    let facts = upstream_facts(&e);
                    tracing::info!(
                        event = "count_tokens",
                        state_key = %sanitize_for_log(&target.state_key),
                        provider = provider_name,
                        status = facts.status.unwrap_or(0),
                        upstream_type = facts.upstream_type.unwrap_or(""),
                        upstream_code = facts.upstream_code.unwrap_or(""),
                        effective_class = class_label(&cf.class),
                        matched_by = matched_by_label(cf.matched_by),
                        remapped,
                        debit,
                        "count_tokens seat terminal; resilience class policy applied",
                    );
                    self.apply_breaker_outcome(&target.state_key, outcome, &cf.class);
                    probe_guard.settle(outcome);
                    self.park_counted_seat(&target, req, &e, &probe_guard);
                    return CountSeatOutcome::Terminal(e);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "count_tokens_tests.rs"]
mod count_tokens_tests;

#[cfg(test)]
#[path = "count_tokens_wire_tests.rs"]
mod count_tokens_wire_tests;
