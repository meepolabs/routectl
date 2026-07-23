//! Token-counting dispatch path (no reducer/cache; independent of the would-trim seam).

use std::time::Instant;

use routectl_core::failure_class::{LastOutcome, classify};
use routectl_core::{ChatRequest, Error, Result, TokenCount, sanitize_for_log};

use super::dispatch::{
    COUNT_TOKENS_CAPABLE_KIND, CountSeatOutcome, apply_remap, class_debits, class_label,
    forwarded_terminal_status, is_capability_error, log_forwarded_auth_terminal, matched_by_label,
    missing_forwarded_bearer_error, rate_limit_reset_hint, upstream_facts,
    upstream_status_for_remap,
};
use super::{DispatchTarget, Router, StripDecision, apply_layered_overlays};

impl Router {
    /// Probe call: route a request to a count_tokens-CAPABLE provider in
    /// the dispatch chain and call `Provider::count_tokens`. Used by
    /// claude-code's context-budget display via the
    /// `/v1/messages/count_tokens` endpoint.
    ///
    /// Capability walk (not a try-and-fallback over health): the chain is
    /// scanned for targets whose `provider_kind == "anthropic-api"` -- the
    /// only count_tokens-capable egress kind (it is the only kind that
    /// overrides `Provider::count_tokens`; every other kind uses the trait
    /// default that 501s). Incapable-by-kind targets are skipped BEFORE
    /// dispatch (DEBUG log, no upstream call, no breaker account); a
    /// kind-skip is operator-known config, not upstream health, so it must
    /// not touch the breaker. This mirrors `filter_chain_by_features`
    /// discipline.
    ///
    /// Why walking is safe for tokenizer correctness: `anthropic-api` is
    /// Claude-only, so every capable target the walk can select uses the
    /// SAME Anthropic tokenizer family. Walking therefore never
    /// reintroduces the wrong-tokenizer hazard -- it only steps over kinds
    /// or seats that cannot count.
    ///
    /// Once a CAPABLE target is selected, the outcome decides the walk:
    ///
    /// - `Ok` -> return the count.
    /// - A CAPABILITY error (`is_capability_error`: local
    ///   `NotImplemented`, or a WIRE 501 from a capable-by-kind seat whose
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
    /// most once (plus at most one 401 auth-retry of that same seat), so
    /// total upstream calls never exceed `2 * chain.len()`. When no
    /// capable seat serves a count (none capable, or every capable seat
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
        let (chain, probe_admissions) = self.dispatch_chain_for_request(&req)?;
        // A token-count is not a messages-capability test, so a re-probe the
        // filter admitted here settles OtherError: release the in_flight slot
        // and leave the entry expired for the next real request to re-probe,
        // never latching it in flight.
        let now = Instant::now();
        for admission in probe_admissions {
            self.learned_capabilities.record_probe_outcome(
                &admission.state_key,
                &admission.feature,
                admission.provider_kind,
                crate::learned_capability::ProbeOutcome::OtherError,
                now,
            );
        }
        let mut saw_capable = false;
        for candidate in chain {
            if candidate.provider_kind != Some(COUNT_TOKENS_CAPABLE_KIND) {
                tracing::debug!(
                    provider = %candidate.provider_name,
                    kind = candidate.provider_kind.unwrap_or("unknown"),
                    model = %candidate.nickname.as_deref().unwrap_or(""),
                    "provider skipped: kind cannot count_tokens",
                );
                continue;
            }
            saw_capable = true;
            match self.count_tokens_try_seat(&req, candidate).await {
                CountSeatOutcome::Count(tc) => return Ok(tc),
                CountSeatOutcome::Terminal(e) => return Err(e),
                // Capability error: the seat is capable-by-kind but its
                // upstream cannot count. The slot was already released
                // without a breaker debit; advance to the next capable
                // seat in the already-resolved chain (single-visit,
                // never re-resolved or re-queued).
                CountSeatOutcome::Capability => continue,
            }
        }
        // Two distinct terminal shapes, both mapping to a 501 at the
        // handler: no capable-by-kind seat existed at all, versus capable
        // seats existed but every one returned a capability error.
        let detail = if saw_capable {
            "count_tokens: all capable providers returned a capability error (cannot count)"
        } else {
            tracing::warn!(
                alias = %req.model,
                "alias chain has no count_tokens-capable provider; \
                 no target in chain overrides count_tokens",
            );
            "count_tokens: no count_tokens-capable provider in chain"
        };
        Err(Error::NotImplemented(req.model.clone(), detail.into()))
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
    async fn count_tokens_try_seat(
        &self,
        req: &ChatRequest,
        target: DispatchTarget,
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
                    provider = %target.provider_name,
                    model = %model_label,
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

            let result = provider.count_tokens(attempt_req.clone()).await;
            attempts_made += 1;
            match result {
                Ok(tc) => {
                    self.record_success(&target.state_key);
                    probe_guard.disarm();
                    return CountSeatOutcome::Count(tc);
                }
                Err(e) => {
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

                    // CAPABILITY error, checked BEFORE should_fallback so a
                    // wire-501 can never reach record_failure: the seat is
                    // capable-by-kind but its upstream cannot count. Release
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
                                "count_tokens got wire-501 from capable-by-kind target; \
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
                    let policy = self.policy_for(&req.model);
                    let native_cf = classify(&e, target.provider_kind);
                    let (cf, remapped) = apply_remap(
                        native_cf,
                        upstream_status_for_remap(&e),
                        &target.class_overrides,
                    );
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
