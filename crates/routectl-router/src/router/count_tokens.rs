//! The token-counting surface of the shared dispatch walk
//! ([`Router::run_walk`]): no reducer, no cache, independent of the
//! would-trim seam. [`CountAdapter`] supplies what a count does differently
//! at the walk's seams.
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

use std::sync::Arc;
use std::time::Instant;

use routectl_core::provider_kind::config as config_kind;
use routectl_core::wire_check::WireTap;
use routectl_core::{ChatRequest, Error, Provider, Result, TokenCount, sanitize_for_log};
use routectl_providers::anthropic_family::{AnthropicFamily, anthropic_family};

use crate::config::RetryPolicy;

use super::capability_observe::UntranslatedCapabilities;
use super::class_observe::{DispatchSurface, class_label, matched_by_label, upstream_facts};
use super::dispatch::{complete_timed_out, is_capability_error};
use super::dispatch_loop::{
    CommitCtx, Deadline, ErrorDisposition, Exhaustion, FailureObservation, SurfaceAdapter,
};
use super::field_repair::FieldSettlementMode;
use super::learn_engine::RejectionSite;
use super::repair_budget::RepairBudget;
use super::runtime_gate::ProbeAdmission;
use super::wire_check::SampleCharge;
use super::{CountedTokens, DispatchMeta, DispatchTarget, Router, RouterOptions};

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

/// The token-count surface: one `count_tokens()` call per attempt on a seat
/// [`seat_can_count_tokens`] admits, with the field settlement mode of the
/// entry point that started the walk.
struct CountAdapter {
    mode: FieldSettlementMode,
}

impl SurfaceAdapter for CountAdapter {
    type Output = TokenCount;
    const SURFACE: DispatchSurface = DispatchSurface::CountTokens;

    fn admits(&self, target: &DispatchTarget) -> bool {
        let capable = seat_can_count_tokens(target.provider_kind, &target.upstream);
        if !capable {
            tracing::debug!(
                provider = %sanitize_for_log(&target.provider_name),
                kind = target.provider_kind.unwrap_or("unknown"),
                model = %sanitize_for_log(target.nickname.as_deref().unwrap_or("")),
                "provider skipped: seat cannot count_tokens",
            );
        }
        capable
    }

    /// The composed `request_timeout_ms`, expiring with a completion's text.
    /// An expired count walks to the next capable seat.
    fn deadline(&self, attempt_policy: &RetryPolicy) -> Option<Deadline> {
        attempt_policy
            .request_timeout_ms
            .map(|ms| Deadline::new(ms, complete_timed_out))
    }

    async fn invoke(
        &self,
        _target: &DispatchTarget,
        provider: Arc<dyn Provider>,
        req: ChatRequest,
    ) -> Result<TokenCount> {
        provider.count_tokens(req).await
    }

    fn on_commit(&self, ctx: CommitCtx<'_, '_>, count: TokenCount) -> TokenCount {
        let CommitCtx {
            target,
            client_req,
            attempt_req,
            translation_report,
            learning,
            learn_dedupe,
            meta,
            ..
        } = ctx;
        // Settle every carry the engine holds against this count. Read from
        // the CLIENT request, as the messages surfaces do, so a repaired
        // commit keys on what the caller asked for rather than the repaired
        // body. A token count carries no response evidence, so the count
        // surface never yields a scope.
        let _no_evidence = learning.on_success(
            &UntranslatedCapabilities::take_from(translation_report),
            target,
            client_req,
            attempt_req,
            learn_dedupe,
            meta,
        );
        count
    }

    fn exhausted(&self, exhaustion: Exhaustion<'_>) -> Error {
        // The walk holds only a timed-out attempt's error or a parked seat's
        // gate refusal (a timeout is kept over a parked seat either way
        // round), and either outranks both 501 shapes below.
        if let Some(parked) = exhaustion.last {
            return parked;
        }
        // Two distinct terminal shapes, both mapping to a 501 at the
        // handler: no capable seat existed at all, versus capable seats
        // existed but every one returned a capability error.
        let detail = if exhaustion.any_admitted {
            "count_tokens: all capable providers returned a capability error (cannot count)"
        } else {
            tracing::warn!(
                alias = %sanitize_for_log(exhaustion.alias),
                "alias chain has no count_tokens-capable provider; \
                 no target in chain can count tokens for its upstream model",
            );
            "count_tokens: no count_tokens-capable provider in chain"
        };
        Error::NotImplemented(exhaustion.alias.to_string(), detail.into())
    }

    /// A token count is not a messages-capability test, so a re-probe the
    /// filter admitted settles OtherError before the walk: release the
    /// in_flight slot and leave the entry expired for the next real request
    /// to re-probe, never latching it in flight. The walk then holds none,
    /// and every seat's engine holds an inert re-probe guard.
    fn settle_probe_admissions(
        &self,
        router: &Router,
        admissions: Vec<ProbeAdmission>,
    ) -> Vec<ProbeAdmission> {
        let now = Instant::now();
        for admission in admissions {
            if matches!(
                router
                    .learned_capabilities
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
        Vec::new()
    }

    fn field_settlement_mode(&self) -> FieldSettlementMode {
        self.mode
    }

    /// A bare tap and no sampler charge: see
    /// [`Router::install_count_wire_tap`].
    fn install_wire(
        &self,
        _router: &Router,
        _target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
    ) -> (Option<WireTap>, SampleCharge) {
        (
            Router::install_count_wire_tap(attempt_req),
            SampleCharge::none(),
        )
    }

    /// Reads the rejection site only; nothing is compared and no rejection
    /// is held on the meta (see [`Router::read_count_rejection_site`]).
    fn read_failed_wire(
        &self,
        router: &Router,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        tap: Option<&WireTap>,
        _charge: SampleCharge,
        err: &Error,
        _meta: &mut DispatchMeta,
    ) -> Option<RejectionSite> {
        router.read_count_rejection_site(target, attempt_req, tap, err)
    }

    /// A CAPABILITY error (`is_capability_error`: local `NotImplemented`, or
    /// a wire 501 from an admitted seat whose upstream cannot count) walks
    /// to the next capable seat with no breaker debit, before the engine or
    /// the health settle reads it: a count-only capability signal must never
    /// be recorded as health on the per-seat breaker that completions gate
    /// on.
    fn disposition(&self, target: &DispatchTarget, err: &Error) -> ErrorDisposition {
        if !is_capability_error(err) {
            return ErrorDisposition::Common;
        }
        if let Error::Upstream { status: 501, .. } = err {
            // DEBUG, not WARN: this is the steady-state path (every count
            // for a passthrough alias 501s here and walks), so at WARN it
            // would flood the log on every client poll.
            tracing::debug!(
                provider = %sanitize_for_log(&target.provider_name),
                state_key = %sanitize_for_log(&target.state_key),
                status = 501,
                "count_tokens got wire-501 from admitted target; \
                 treating as capability, not debiting breaker",
            );
        }
        ErrorDisposition::WalkWithoutDebit
    }

    /// One INFO event at the settle point makes a count breaker debit or
    /// park triageable. Safe dimensions only, never a body or prompt.
    fn observe_failure(&self, router: &Router, failure: &FailureObservation<'_>) {
        router.count_refusal_error(failure.ingress, failure.target, failure.err);
        let facts = upstream_facts(failure.err);
        tracing::info!(
            event = "count_tokens",
            state_key = %sanitize_for_log(&failure.target.state_key),
            provider = %sanitize_for_log(&failure.target.provider_name),
            status = facts.status.unwrap_or(0),
            upstream_type = facts.upstream_type.unwrap_or(""),
            upstream_code = facts.upstream_code.unwrap_or(""),
            effective_class = class_label(&failure.cf.class),
            matched_by = matched_by_label(failure.cf.matched_by),
            remapped = failure.remapped,
            debit = failure.debit,
            "count_tokens seat terminal; resilience class policy applied",
        );
    }
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
    /// - An attempt that outlives the composed `request_timeout_ms` -> the
    ///   status-0 timeout error a completion gives, debited like a 5xx, then
    ///   advance to the NEXT capable seat. The walk keys on its own deadline
    ///   expiring, never on the status-0 class a refused connection shares.
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
    /// serves a count, the last timed-out attempt's error is returned, else a
    /// parked seat's refusal; failing both (none capable, or every capable
    /// seat returned a capability error), this returns
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
        // threaded by `&mut` through the walk. Constructing it per seat would
        // be the per-seat reset the shared ceiling exists to remove; holding
        // it here reads its spend once on every exit of the walk.
        let mut repair_budget = RepairBudget::per_request();
        let result = Box::pin(self.run_walk(
            CountAdapter { mode },
            req,
            RouterOptions::default(),
            &mut meta,
            &mut repair_budget,
        ))
        .await;
        self.finish_dispatch(&meta, &repair_budget);
        CountedTokens { meta, result }
    }
}

#[cfg(test)]
#[path = "count_tokens_tests.rs"]
mod count_tokens_tests;

#[cfg(test)]
#[path = "count_tokens_wire_tests.rs"]
mod count_tokens_wire_tests;
