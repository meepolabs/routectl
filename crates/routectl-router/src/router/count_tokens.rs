//! Token-counting dispatch path (no reducer/cache; independent of the would-trim seam).
//!
//! # The two reactive-repair kinds at this walk's repair position
//!
//! This walk carries the third reactive-repair position, wired at the same
//! seam the two messages walks use and drawing the same per-request ceiling.
//! The two repair kinds differ in whether a seat here can reach them, and the
//! difference is structural rather than a gap in wiring:
//!
//! - **Envelope-field repair is REACHABLE and behaviorally covered.** It acts
//!   on the `anthropic-api` lane, which is exactly the lane
//!   [`seat_can_count_tokens`] admits unconditionally, so a repair genuinely
//!   fires here. Whether it SETTLES depends on the entry point: through
//!   [`Router::count_tokens_with_meta`] the walk runs the full two-phase
//!   settlement and both a committed and a cleared verdict ride out on the
//!   returned [`DispatchMeta`]; through the result-only
//!   [`Router::count_tokens`] it repairs for the answer it returns and settles
//!   nothing, because that caller keeps no meta and a settlement whose event
//!   row never reached the ledger would leave the shared registry mutated with
//!   no record of it -- the state a warm rebuild resurrects a verdict from.
//! - **Reasoning-replay repair is UNREACHABLE from here.** The classifier's
//!   replay-rejection lift is closed over the provider kinds with a captured
//!   envelope -- today only `openai-responses` -- and that set is disjoint
//!   from the kinds this walk admits (both read the same
//!   `DispatchTarget::provider_kind`). So no seat this walk can dispatch to
//!   can produce the class its replay arm gates on. That arm therefore
//!   settles its carry by DROPPING the plan -- releasing the single-flight
//!   slots, learning nothing -- and the degradation summary stays honest by
//!   recording that the repair happened without claiming anything was
//!   learned. Do not "fix" that to match the field arm's settlement without
//!   first making the class reachable: a behavioral test of an unreachable
//!   arm passes with the arm deleted.

use std::time::Instant;

use routectl_core::failure_class::{LastOutcome, classify, classify_with_attempt};
use routectl_core::{ChatRequest, Error, Result, TokenCount, sanitize_for_log};

use super::class_observe::{DispatchSurface, class_label, matched_by_label, upstream_facts};
use super::dispatch::{
    REPLAY_ACTION_STRIP_REPAIR, REPLAY_REASON_UPSTREAM_REJECTION, apply_remap, class_debits,
    emit_replay_degradation, forwarded_terminal_status, is_capability_error,
    log_forwarded_auth_terminal, missing_forwarded_bearer_error, rate_limit_reset_hint,
    replay_rejection_body_free, upstream_status_for_remap,
};
use super::field_preflight::emit_field_preflight;
use super::field_repair::{FieldSettlementMode, emit_field_repair};
use super::repair_budget::RepairBudget;
use super::replay_repair::strip_replay_artifacts_recalibrating;
use super::{
    CountedTokens, DispatchMeta, DispatchTarget, ReplayDegradation, Router, StripDecision,
    apply_layered_overlays,
};
use crate::anthropic_family::{AnthropicFamily, anthropic_family};

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
/// The `bedrock` arm needs no feature gate: it matches a plain string
/// literal, so a build without that egress compiled in simply never
/// produces a seat that reaches it.
fn seat_can_count_tokens(kind: Option<&str>, upstream: &str) -> bool {
    match kind {
        Some("anthropic-api") => true,
        Some("bedrock") => matches!(anthropic_family(upstream), AnthropicFamily::Yes),
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
    /// non-fallbackable 4xx, a gate block, or an auth-refresh failure.
    Terminal(Error),
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
    /// most once, plus at most one 401 auth-retry of that same seat, plus at
    /// most one reactive repair re-dispatch of that same seat. The auth
    /// retry is per SEAT, while repairs draw a per-REQUEST ceiling shared
    /// with the two messages walks, so total upstream calls never exceed
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
        let result = self.count_tokens_inner(req, mode, &mut meta).await;
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
        req: ChatRequest,
        mode: FieldSettlementMode,
        meta: &mut DispatchMeta,
    ) -> Result<TokenCount> {
        let (chain, probe_admissions) = self.dispatch_chain_for_request(&req)?;
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
                        &admission.state_key,
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
                    state_key = %admission.state_key,
                    capability_key = %admission.feature,
                    attempted_outcome = "other_error",
                    "probe settlement refused: its admission predates the live \
                     capability generation"
                );
            }
        }
        let mut saw_capable = false;
        // Reactive-repair ceiling for THIS client request, declared above the
        // per-seat walk exactly as `complete_inner` declares it above its
        // chain loop, and threaded by `&mut` into every seat. Constructing it
        // inside `count_tokens_try_seat` would be the per-seat reset the shared
        // ceiling exists to remove.
        let mut repair_budget = RepairBudget::per_request();
        let outcome = self
            .count_tokens_walk(
                &req,
                chain,
                &mut saw_capable,
                &mut repair_budget,
                mode,
                meta,
            )
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
    /// request (a count or a terminal error), `None` when the walk exhausted
    /// without one and the caller must build the terminal 501.
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
            match self
                .count_tokens_try_seat(req, candidate, repair_budget, mode, meta)
                .await
            {
                CountSeatOutcome::Count(tc) => return Some(Ok(tc)),
                CountSeatOutcome::Terminal(e) => return Some(Err(e)),
                // Capability error: the seat was admitted as capable but its
                // upstream cannot count. The slot was already released
                // without a breaker debit; advance to the next capable
                // seat in the already-resolved chain (single-visit,
                // never re-resolved or re-queued).
                CountSeatOutcome::Capability => continue,
            }
        }
        None
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
            StripDecision::RouteAway(_) => return CountSeatOutcome::Capability,
        }

        // Envelope-field pre-flight (see `complete_inner`): same seam,
        // `CountTokens` surface. `attempt_req` at this point carries only
        // this seat's overlay + strip, never a prior seat's clone or a
        // reactive repair, so it is the same "original for this target"
        // base the messages walks plan from. The returned request REPLACES
        // `attempt_req`, so from here on this seat has exactly one body:
        // the remote count dispatches it, and every local read of it (the
        // calibration re-stamp on the replay-strip branch below) measures
        // that same body rather than an unplanned one.
        // The plan is held rather than discarded so the modified-request
        // accounting guard lives for this seat's whole attempt loop; a token
        // count never claims a canary (the surface is excluded upstream of the
        // cadence tick), so there is no settlement to owe here.
        let (preflighted_req, preflight_records, _field_preflight_plan) =
            self.plan_field_preflight(&attempt_req, &target, DispatchSurface::CountTokens);
        attempt_req = preflighted_req;
        meta.field_preflight.extend(preflight_records);

        // Reasoning-replay carry admission at the ANALOGOUS position the two
        // messages walks use: after every request-shaping step and before the
        // seat's attempt loop, so the gray-artifact count describes the exact
        // carried bytes. `None` either found nothing to repair or already
        // stripped `attempt_req` proactively (an acting negative or a peer
        // probe), in which case the stripped variant is what this seat counts.
        let now_admit = Instant::now();
        let mut replay_plan = self.plan_replay_carry(&target, &mut attempt_req, meta, now_admit);
        let mut replay_repair_attempted = false;
        // Envelope-field carry admission at the SAME position: the closed
        // table plus this attempt's own fields decide the identity, so the
        // guard is claimed before dispatch and both settlements stay reachable
        // (commit on a repaired success, clear when the field is accepted).
        // Nothing is mutated here -- an acting verdict simply refuses the slot
        // and the attempt dispatches unchanged.
        let mut field_plan = self.plan_field_carry(&target, &attempt_req, mode, now_admit);
        let mut field_repair_attempted = false;
        let mut field_reject_status: u16 = 0;

        let mut auth_retry_attempted = false;
        let mut attempts_made: u32 = 0;
        loop {
            // Per-attempt gate: rate limit + circuit breaker. Runs on THIS
            // seat before its upstream is touched (and again on the 401
            // retry), so a capability walk cannot fan across seats to
            // bypass an operator rate limit or an open breaker.
            if let Some((gate_kind, gate_err)) =
                self.gate_check(&target.state_key, &target.provider_name)
            {
                tracing::warn!(
                    provider = %routectl_core::sanitize_for_log(&target.provider_name),
                    model = %routectl_core::sanitize_for_log(model_label),
                    gate_kind,
                    error = ?gate_err,
                    "count_tokens gate blocked",
                );
                return CountSeatOutcome::Terminal(gate_err);
            }

            // Cancellation backstop (see ProbeSlotGuard): free the
            // half-open probe slot if this future is dropped before an
            // outcome arm settles it.
            let mut probe_guard = self.probe_slot_guard(&target.state_key);

            // Lazy probe activation at the ADMITTED boundary, the same
            // position and for the same reasons as the two messages walks:
            // the gate has cleared, every request-shaping step has run, and
            // this seat is about to be dialed. A count_tokens request is
            // real admitted traffic on this lane, so a lane that only ever
            // sees counting still activates -- and it reads `attempt_req`,
            // the per-target post-pre-flight body actually being sent.
            self.on_admitted_request(&target, &attempt_req);
            let result = provider.count_tokens(attempt_req.clone()).await;
            attempts_made += 1;
            match result {
                Ok(tc) => {
                    self.record_success(&target.state_key);
                    probe_guard.disarm();
                    // Settle the replay carry WITHOUT persisting or emitting:
                    // dropping the plan releases every single-flight slot it
                    // holds and leaves any resident entry exactly as it was.
                    //
                    // Deliberately NOT the two-phase settle the messages walks
                    // run, and the reason is REACHABILITY, not a missing sink: a
                    // settling caller of this walk does drain its meta (the
                    // field carry below settles through exactly that path). What
                    // stops this one is that no seat this walk admits can
                    // produce the replay-rejection class at all, so the carry
                    // here is never the optimistic probe the two-phase learn
                    // exists to settle -- committing off it would persist a
                    // conclusion nothing tested. The degradation summary stays
                    // honest by recording that the repair happened and claiming
                    // nothing was learned.
                    drop(replay_plan.take());
                    if replay_repair_attempted && let Some(deg) = meta.replay_degradation.as_mut() {
                        deg.repair_succeeded = true;
                    }
                    // Settle the envelope-field carry through the two-phase
                    // guard. Unlike the replay carry above, this class IS
                    // reachable from a capable seat, so the carry really is the
                    // optimistic probe the two-phase learn settles. Both
                    // settlements are no-ops unless the plan holds a guard,
                    // which only a SETTLING entry point admits -- so a
                    // result-only caller reaches this code and persists nothing,
                    // while a with-meta caller's row reaches the
                    // capability-event ledger and a warm rebuild sees the same
                    // outcome this process reached:
                    //
                    // - repaired success -> commit the verdict (the rejection
                    //   is confirmed as a real field incompatibility);
                    // - unrepaired success -> the field is ACCEPTED, so clear
                    //   any resident verdict and ride the clear out, or a warm
                    //   rebuild would resurrect a verdict this request
                    //   disproved.
                    if let Some(plan) = field_plan.take() {
                        if field_repair_attempted {
                            let features = super::field_repair::request_feature_keys(req);
                            // The `learned` claim is derived from what the commit
                            // actually PERSISTED, never asserted alongside it: a
                            // non-settling plan commits nothing, and a summary that
                            // claimed otherwise would be a false persistence claim on
                            // the surface built to make such claims trustworthy.
                            let learned =
                                plan.commit(field_reject_status, features, Instant::now());
                            let persisted = learned.is_some();
                            meta.learned_capabilities.extend(learned);
                            self.note_field_repair_succeeded(meta, persisted);
                        } else {
                            meta.cleared_capabilities.extend(plan.settle_success());
                        }
                    }
                    return CountSeatOutcome::Count(tc);
                }
                Err(mut e) => {
                    // Classified ONCE at the top of the arm (as both messages
                    // walks do), because the repair arm below and the health
                    // settle further down must read the SAME effective class.
                    // The carried-artifact signal comes from the plan, so a
                    // proven replay rejection lifts here and nowhere else.
                    let policy = self.policy_for(&req.model);
                    let native_cf = match replay_plan.as_ref() {
                        Some(plan) => {
                            classify_with_attempt(&e, target.provider_kind, plan.attempt())
                        }
                        None => classify(&e, target.provider_kind),
                    };
                    // Retained before the remap: the field-repair arm below
                    // reads what the UPSTREAM said, while routing reads the
                    // operator's override (see that arm).
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
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
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
                            self.release_probe_slot(&target.state_key);
                            probe_guard.disarm();
                            return CountSeatOutcome::Terminal(refresh_err);
                        }
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                        continue;
                    }

                    // Reasoning-replay strip repair, the count_tokens twin of
                    // the messages arms and placed at the analogous position:
                    // after auth recovery, before the capability and health
                    // settles. On the proven replay rejection, switch THIS
                    // seat's attempt request to the pre-stripped variant and
                    // re-dispatch it once; `strip_replay_artifacts_recalibrating`
                    // re-stamps the calibration estimate, and because the loop
                    // dispatches `attempt_req`, the repaired body is the one
                    // sent upstream and the one whose count is returned.
                    //
                    // Slot handling mirrors the 401 recovery above: release the
                    // half-open probe slot before the `continue` re-gates, and
                    // never debit the breaker -- a repairable rejection is not
                    // this seat's health signal. The per-seat
                    // `replay_repair_attempted` flag keeps it at most once per
                    // seat; `repair_budget.draw()` is the LAST condition, so a
                    // request that never repairs is never charged and an
                    // exhausted request budget leaves the rejection on the
                    // ordinary settle path below.
                    if !replay_repair_attempted
                        && let Some(plan) = replay_plan.as_ref()
                        && Self::is_replay_rejection_class(&cf.class)
                        && repair_budget.draw()
                    {
                        replay_repair_attempted = true;
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
                        strip_replay_artifacts_recalibrating(&mut attempt_req, lane, meta);
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                        continue;
                    }

                    // Envelope-field L0 repair, at the SAME dispatch position
                    // the replay repair occupies: after auth recovery, before
                    // the capability and health settles. On a rejection naming
                    // the carried field, DROP that field from this seat's
                    // attempt request and re-dispatch it once. Because the loop
                    // dispatches `attempt_req`, the repaired body is the one
                    // sent upstream and the one whose count is returned.
                    //
                    // The class read here is the NATIVE one, never the remapped
                    // `cf.class`: an operator `[class_overrides]` entry says how
                    // a status should be ROUTED, not what the upstream said, so
                    // reading it would let an override turn a 429 into a field
                    // repair. Routing below still reads the override.
                    //
                    // `apply` owns the budget draw together with the mutation,
                    // so a drop that removes nothing charges nothing; every
                    // repaired-state flag is set only on its `Some`. It also
                    // selects WHICH admitted row to repair -- the one the
                    // rejection names -- and releases every other candidate's
                    // guard. Slot handling mirrors the arm above: release the
                    // half-open probe slot before the `continue` re-gates, and
                    // never debit the breaker -- a repairable envelope
                    // rejection is a caller-shaped fault, not this seat's
                    // health signal.
                    if !field_repair_attempted
                        && let Some(plan) = field_plan.as_mut()
                        && let Some(status) = plan.apply(
                            &mut attempt_req,
                            meta,
                            repair_budget,
                            &original_class,
                            &e,
                            target.provider_kind.unwrap_or(""),
                        )
                    {
                        field_repair_attempted = true;
                        field_reject_status = status;
                        let repaired_path = plan
                            .repaired_path()
                            .expect("a reported repair names the row it dropped");
                        self.note_field_repair(meta, &target.state_key, repaired_path);
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                        continue;
                    }
                    // The repair arm declined (already repaired this seat, not
                    // a replay rejection, or the request's budget is spent), so
                    // this error can now reach the CALLER. A replay rejection's
                    // upstream body echoes the reasoning artifact it objected
                    // to, so rebuild it body-free first -- the same helper and
                    // the same position the two messages walks use, before
                    // their generic logging and their returns. Without it the
                    // token-count walk is the one surface that hands a client
                    // the reasoning blob verbatim. A non-replay class is left
                    // untouched (no clone on the common path).
                    if let Some(body_free) =
                        replay_rejection_body_free(&e, &cf.class, provider_name)
                    {
                        e = body_free;
                    }

                    // CAPABILITY error, checked BEFORE should_fallback so a
                    // wire-501 can never reach record_failure: the seat was
                    // admitted as capable but its upstream cannot count. Release
                    // the probe slot WITHOUT debiting the breaker, then let
                    // the caller walk to the next capable seat.
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
                                state_key = %target.state_key,
                                status = 501,
                                "count_tokens got wire-501 from admitted target; \
                                 treating as capability, not debiting breaker",
                            );
                        }
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                        return CountSeatOutcome::Capability;
                    }

                    // Health error. Mirror `complete_with_options`: the
                    // breaker DEBIT keys off the failure CLASS, not the
                    // fallback decision. A transient-health class (429 / 5xx
                    // / status-0 / overload) debits (an honored reset hint
                    // parks instead); a caller-shaped 4xx releases the slot
                    // without a debit, so a repeated non-retryable 4xx here
                    // cannot trip the per-seat breaker that also gates
                    // completions and streams. Either way this propagates --
                    // health fallback stays reserved for the messages path,
                    // so a 429 here does NOT walk.
                    let reset_hint = rate_limit_reset_hint(&e, &policy);
                    let debit = class_debits(&cf.class);
                    // The class/remap/debit decision on the token-count path was
                    // otherwise silent (unlike the messages path, which emits a
                    // class-decision event at every error arm). One INFO event at
                    // the settle point makes a count_tokens breaker debit / park
                    // triageable. Safe dimensions only -- NEVER a body or prompt.
                    let facts = upstream_facts(&e);
                    tracing::info!(
                        event = "count_tokens",
                        state_key = %target.state_key,
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
                    if debit {
                        match reset_hint {
                            Some(h) => self.park_provider(&target.state_key, h),
                            None => self.record_failure(
                                &target.state_key,
                                LastOutcome::from_failure_class(&cf.class),
                            ),
                        }
                        probe_guard.disarm();
                    } else {
                        self.release_probe_slot(&target.state_key);
                        probe_guard.disarm();
                    }
                    return CountSeatOutcome::Terminal(e);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "count_tokens_tests.rs"]
mod count_tokens_tests;
