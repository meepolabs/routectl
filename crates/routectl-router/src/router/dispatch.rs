//! Dispatch retry state machine. Walks the resolved fallback chain
//! attempting each provider until one succeeds or all are exhausted,
//! retrying within a provider per `RetryPolicy` with exponential
//! backoff. Owns failure-class remap/fallback/retry decisions, breaker
//! accounting, class-decision observability, the would-trim recording +
//! K-floor, context-reduction + auto-cache injection, and forwarded-
//! credential handling for both `complete` and `stream`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::stream::{BoxStream, StreamExt};
use routectl_core::wire_check::IngressDialect;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, ReplayScheme, Result,
    failure_class::{ClassifiedFailure, FailureClass, MatchedBy},
    sanitize_for_log, scan_caller_prefix_advisory,
};
use serde_json::Value;

use crate::catalog::{CatalogRow, EffectiveRow};
use crate::config::RetryPolicy;
use crate::context_trim::{
    SteadyStateTrimParams, collect_near_lossless_marks, estimate_total_tokens,
    near_lossless_candidate, propose_steady_state_trim, trimmed_prefix_fingerprint,
};
use crate::cost_gate::{break_even_k, emission_break_even_k};

use super::beta_report_learn::install_beta_assembly;
use super::cache_plan::{AutoCacheRequestPlan, apply_auto_cache_placement, cache_target_gates};
use super::capability_learn::LearnDedupeKey;
use super::capability_observe::{
    EvidencePolicy, UntranslatedCapabilities, install_translation_report,
};
use super::class_observe::{
    DispatchSurface, UpstreamFacts, apply_remap, class_label, matched_by_label, upstream_facts,
    upstream_status_for_remap,
};
use super::credential_park::{log_gate_refusal, merge_gate_refusal};
use super::dispatch_loop::{
    CommitCtx, Deadline, SurfaceAdapter, apply_context_reduction, forwarded_terminal_status,
    log_forwarded_auth_terminal, missing_forwarded_bearer_error, next_is_sibling_seat,
};
use super::edit_declare::{declare_cache_placement, declare_model_rename, fidelity_dialect};
use super::feature_filter::{StripDecision, emit_feature_unsupported};
use super::field_preflight::emit_field_preflight;
use super::field_repair::emit_field_repair;
use super::learn_engine::{AttemptLearning, FailureDecision, Rejection};
use super::local_refusal::ingress_dialect_of;
use super::outcome_rule::{
    breaker_outcome, class_debits, probe_fast_fail, rate_limit_reset_hint, retry_cap_for,
    should_fallback,
};
use super::overlays::apply_layered_overlays;
use super::probe_payload_capture::ProbeFieldCapture;
use super::repair_budget::RepairBudget;
use super::replay_repair::replay_rejection_body_free;
use super::runtime_gate::{
    LearnedProbeGuard, ProbeAdmissionSet, is_probe_request, log_probe_fast_fail,
};
use super::stream_commit::{try_stream_with_first_content, wrap_with_breaker_accounting};
use super::stream_evidence::StreamEvidence;
use super::transcript_defect::{
    egress_pairing_refusal, is_transcript_rejection, record_transcript_defect,
};
use super::wire_response::{declare_model_relabel, declare_provider_attribution};
use super::{DispatchMeta, DispatchTarget, Dispatched, DispatchedStream, Router, RouterOptions};

/// Message/event name of the reasoning-replay degradation WARN. Stable,
/// greppable, closed-set.
const REPLAY_DEGRADE_EVENT: &str = "reasoning_replay_degraded";
/// Action token: the fixed strip-repair correctness branch stripped the
/// carried reasoning artifacts and re-dispatched the same target once.
pub(super) const REPLAY_ACTION_STRIP_REPAIR: &str = "strip_repair";
/// Reason token: the carried variant drew the proven upstream replay
/// rejection.
pub(super) const REPLAY_REASON_UPSTREAM_REJECTION: &str = "upstream_replay_rejection";

/// The one provider kind whose egress can represent the
/// OpenAI-Responses-dialect `reasoning.context` / `reasoning.mode`
/// sub-keys. Matches `ProviderEntry::kind_str`, so it round-trips with the
/// `kind = "..."` discriminant in the operator's provider table.
const RESPONSES_PROVIDER_KIND: &str = routectl_core::provider_kind::config::OPENAI_RESPONSES;

/// Whether `req` carries a Responses-dialect `reasoning.context` or
/// `reasoning.mode` under `provider_extras`. `summary` is deliberately
/// excluded -- its loss is a soft downgrade of summary verbosity, not a
/// semantic gap.
fn carries_responses_reasoning_dialect(req: &ChatRequest) -> bool {
    req.provider_extras
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("reasoning"))
        .and_then(|v| v.as_object())
        .is_some_and(|m| m.contains_key("context") || m.contains_key("mode"))
}

/// Whether the target egress DROPS the Responses-dialect reasoning
/// sub-keys. An unknown / unresolved provider kind (the legacy direct
/// dispatch path) is treated as dropping, so the fidelity loss warns
/// rather than passing silently.
fn target_drops_responses_reasoning(provider_kind: Option<&str>) -> bool {
    provider_kind != Some(RESPONSES_PROVIDER_KIND)
}

/// Emit the reasoning-replay degradation WARN for a resolved request: one
/// line per strip-repair that fired anywhere in the chain walk, in the order
/// they fired, and none otherwise. The request's repair ceiling bounds the
/// line count. Every line carries `repair_count`, the request's total, so a
/// reader of any one line knows whether siblings exist. Carries closed-set
/// tokens and counts only -- never the artifact bytes, a reasoning item id,
/// a hash, the session key, or the upstream body. The request span already
/// supplies `request_id` correlation across the retry and fallback hops.
pub(super) fn emit_replay_degradation(meta: &DispatchMeta) {
    let repair_count = meta.replay_degradations.len();
    for deg in &meta.replay_degradations {
        tracing::warn!(
            action = deg.action,
            target_lane = deg.target_lane.as_str(),
            state_key = %sanitize_for_log(&deg.state_key),
            source_schemes = %join_schemes(deg.source_schemes.as_slice()),
            reason = deg.reason,
            artifact_count = deg.artifact_count,
            repair_attempted = deg.repair_attempted,
            repair_succeeded = deg.repair_succeeded,
            learned = deg.learned,
            repair_count,
            "{REPLAY_DEGRADE_EVENT}",
        );
    }
}

/// Join replay scheme tokens into a stable, comma-separated closed-set
/// string for the degradation WARN's `source_schemes` field.
fn join_schemes(schemes: &[ReplayScheme]) -> String {
    schemes
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Bring `meta.calib_estimated_tokens` back in line with `attempt_req` after
/// a dispatch-path mutation shrank the payload.
///
/// `record_would_trim` stamps the estimate ONCE, before the retry loop, from
/// the request as it stood then. A reactive repair -- dropping a rejected
/// envelope field, stripping a rejected artifact -- makes the dispatched
/// payload smaller than that stamp describes, while the provider's reported
/// prompt total reflects the smaller payload. Left uncorrected the evidence
/// ratio comes out too low, and a low correction factor shrinks a corrected
/// estimate until the window gate admits requests the static estimate had
/// correctly judged too large.
///
/// THE single re-stamp entry point for every such mutation, so a new repair
/// class cannot land on this path without going through it.
pub(super) fn restamp_calibration_estimate(attempt_req: &ChatRequest, meta: &mut DispatchMeta) {
    meta.calib_estimated_tokens = Some(estimate_total_tokens(attempt_req));
}

/// The non-streaming surface: one `complete()` call per attempt, bounded by
/// the composed `request_timeout_ms`, committed when the response arrives.
struct CompleteAdapter;

/// The expiry text of a `complete()` attempt deadline.
fn complete_timed_out(ms: u64) -> String {
    format!("request timed out after {ms}ms")
}

impl SurfaceAdapter for CompleteAdapter {
    type Output = ChatResponse;
    const SURFACE: DispatchSurface = DispatchSurface::Complete;

    fn admits(&self, _target: &DispatchTarget) -> bool {
        true
    }

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
    ) -> Result<ChatResponse> {
        provider.complete(req).await
    }

    fn on_commit(&self, ctx: CommitCtx<'_, '_>, mut resp: ChatResponse) -> ChatResponse {
        let CommitCtx {
            router,
            target,
            client_req,
            attempt_req,
            wire_tap,
            translation_report,
            learning,
            learn_dedupe,
            meta,
        } = ctx;
        // Stamp the served upstream provider name only when the served
        // target opts in (`visible_routectl_provider`, default true). When
        // suppressed, leave the field None so serde's skip_serializing_if
        // drops it from the response. Internal accounting keys off
        // `DispatchMeta.served_provider` / `served_upstream`, not this
        // client-visible field, so suppression does not affect usage capture.
        // Authoritative gate: every concrete provider pre-stamps
        // `routectl_provider` with its own id before returning, so
        // suppression MUST clear the field, not merely skip setting it --
        // otherwise the provider's value leaks through.
        if target.visible_routectl_provider {
            resp.routectl_provider = Some(target.provider_name.clone());
            declare_provider_attribution(wire_tap);
        } else {
            resp.routectl_provider = None;
        }
        // Client-visible label: the serving target's `reported_model`
        // override when set (and non-empty), else the client's requested
        // alias. Internal accounting keys off `DispatchMeta`, not this field.
        resp.model = resolve_reported_model(target, &client_req.model);
        declare_model_relabel(wire_tap);
        router.carry_response_check(target, attempt_req, wire_tap, meta);
        // A 2xx proves the capability is not rejected: clear any learned
        // negative this dispatch re-probed, and ride each clear out on the
        // meta so the ledger records it and a warm rebuild does not resurrect
        // the negative. Beta flags the engine's repair withheld settle first,
        // as rejections. A capability the egress withheld was never tested,
        // so it is not cleared.
        let untranslated = UntranslatedCapabilities::take_from(translation_report);
        // Then settle the replay carry, the envelope-field carry, and the
        // re-verification canary against this success.
        let evidence = learning.on_success(
            &untranslated,
            target,
            client_req,
            attempt_req,
            learn_dedupe,
            meta,
        );
        // The assembled response is complete evidence, so a missing shape
        // here is a real absence: suspects are admitted.
        if let Some(scope) = evidence {
            router.observe_response_evidence(
                scope,
                &resp,
                EvidencePolicy {
                    admit_suspect: true,
                },
                &mut meta.capability_observations,
            );
        }
        // Subscription-quota feed, the non-streaming half. The reading rides
        // on the assembled response, and a commit ends the walk, so the feed
        // fires exactly once per response. Keyed on the seat that ACTUALLY
        // served -- a fallback's reading describes the credential that
        // answered, not the home one. The streaming half is armed at the
        // stream arm.
        crate::quota::feed::feed_response(
            &router.quota_store,
            meta.served_seat
                .as_deref()
                .map(crate::quota::key::seat_key_for_served_identity)
                .as_ref(),
            &resp,
        );
        resp
    }

    fn exhausted(&self, last: Option<Error>, alias: &str) -> Error {
        last.unwrap_or_else(|| Error::UnknownAlias(alias.to_string()))
    }
}

impl Router {
    /// Advisory-only WARN when the region the CALLER marked cacheable carries
    /// per-request-volatile content (fresh ids/timestamps): such a prefix
    /// writes a fresh cache entry every request that is never re-read. Reads
    /// the ORIGINAL request (caller breakpoints only; auto-emit is routectl's
    /// own and exempt), mutates nothing, and never logs a raw value -- only
    /// the structural component, volatile kind, and breakpoint position.
    /// Edge-triggered: at most one WARN per process per (component, kind).
    pub(super) fn warn_volatile_in_caller_prefix(&self, req: &ChatRequest) {
        let advisory = scan_caller_prefix_advisory(req);
        if advisory.findings().is_empty() {
            return;
        }
        let mut warned = self.volatile_prefix_warned.lock();
        for finding in advisory.findings() {
            if warned.insert((finding.component(), finding.kind())) {
                tracing::warn!(
                    component = finding.component().as_str(),
                    volatile_kind = finding.kind().as_str(),
                    breakpoint_position = ?finding.breakpoint_position(),
                    "cache_volatile_in_caller_prefix",
                );
            }
        }
    }

    /// Fidelity WARN when the target this dispatch is about to hit cannot
    /// represent the Responses-dialect `reasoning.context` / `reasoning.mode`
    /// the request carries. Reads the per-target (post-overlay) clone at the
    /// dispatch point, so it is accurate for the target actually used: a
    /// Responses primary that fails over to a non-Responses fallback warns,
    /// a Responses-only success does not. `warned` is a stack-local owned by
    /// the chain loop, making this at most one WARN per client request across
    /// same-provider retries and fallback hops. Logs no field values.
    pub(super) fn warn_dropped_reasoning_dialect(
        provider_name: &str,
        provider_kind: Option<&str>,
        attempt_req: &ChatRequest,
        warned: &mut bool,
    ) {
        if *warned
            || !target_drops_responses_reasoning(provider_kind)
            || !carries_responses_reasoning_dialect(attempt_req)
        {
            return;
        }
        *warned = true;
        tracing::warn!(
            provider = %routectl_core::sanitize_for_log(provider_name),
            "reasoning context/mode dropped: representable only on the OpenAI Responses egress"
        );
    }

    /// Complete a non-streaming request with default options, returning
    /// only the dispatch result.
    pub async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.complete_with_options(req, RouterOptions::default())
            .await
            .result
    }

    /// Complete a non-streaming request, returning the result paired with
    /// its router-scoped dispatch metadata.
    #[must_use]
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn complete_with_options(&self, req: ChatRequest, opts: RouterOptions) -> Dispatched {
        let mut meta = DispatchMeta::for_alias(&req.model);
        // Reactive-repair ceiling for THIS client request, declared outside the
        // walk so the draws it spent are read once on every exit of it.
        let mut repair_budget = RepairBudget::per_request();
        let result =
            Box::pin(self.run_walk(CompleteAdapter, req, opts, &mut meta, &mut repair_budget))
                .await;
        self.metrics
            .record_repairs_per_request(repair_budget.spent());
        emit_replay_degradation(&meta);
        emit_field_repair(&meta);
        self.emit_wire_rejections(&meta);
        emit_field_preflight(&meta);
        Dispatched { meta, result }
    }

    /// Stream a request with default options, returning only the dispatch
    /// result.
    pub async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_with_options(req, RouterOptions::default())
            .await
            .result
    }

    /// Streaming counterpart. Fallback only happens BEFORE the first
    /// CONTENT chunk reaches the client; once the upstream has emitted
    /// content, mid-stream errors propagate. Leading content-free chunks
    /// (a `delta.role` opener, id/model metadata) are buffered and do not
    /// commit the provider. Gate checks (rate limit / breaker) run before
    /// the upstream is touched.
    #[must_use]
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
    pub async fn stream_with_options(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
    ) -> DispatchedStream {
        let mut meta = DispatchMeta::for_alias(&req.model);
        // See `complete_with_options`: the ceiling outlives the walk so its
        // spend is read once on every exit.
        let mut repair_budget = RepairBudget::per_request();
        let result = self
            .stream_inner(req, opts, &mut meta, &mut repair_budget)
            .await;
        self.metrics
            .record_repairs_per_request(repair_budget.spent());
        emit_replay_degradation(&meta);
        emit_field_repair(&meta);
        self.emit_wire_rejections(&meta);
        emit_field_preflight(&meta);
        DispatchedStream { meta, result }
    }

    /// Dispatch-loop body for `stream_with_options`. Mutates `meta` as
    /// the chain is walked. The served_* fields are captured at the
    /// `Ok(stream)` arm (the winning target is known synchronously,
    /// before any stream body is consumed).
    async fn stream_inner(
        &self,
        mut req: ChatRequest,
        opts: RouterOptions,
        meta: &mut DispatchMeta,
        repair_budget: &mut RepairBudget,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let pairing = record_transcript_defect(&mut req);
        let (chain, probe_admissions) =
            self.dispatch_chain_for_request(&req, DispatchSurface::Stream)?;
        // See `run_walk`: re-probes the filter admitted, owned by a
        // request-scoped set that settles each on transfer (`take`) or on drop
        // (`OtherError` for any admission the loop never reached).
        let mut probe_set = ProbeAdmissionSet::new(
            self.learned_capabilities.clone(),
            probe_admissions,
            DispatchSurface::Stream.as_str(),
        );
        // See `run_walk`: request-global observability dimension
        // ONLY, fed to `emit_class_observability`. The per-target
        // `use_forwarded_credential` flag drives the model-rewrite
        // bypass and the 401/403/429 terminal re-key.
        let is_forwarded = req.routectl_internal.forwarded_bearer.is_some();
        let ingress = ingress_dialect_of(&req);
        let chain_len = chain.len();
        let policy = self.policy_for(&req.model);
        // Availability-probe detection (see `complete_with_options`). A
        // streaming probe that 429/529s fast-fails the chain too.
        let is_probe = is_probe_request(&req, &policy);
        // Auto-cache decision inputs: computed ONCE off the ORIGINAL req
        // (see `run_walk`). Reused across retries and fallback
        // targets so the decision never drifts.
        let auto_cache_plan =
            AutoCacheRequestPlan::build(&req, self.config.cache.auto_emit_top_level_breakpoint);
        if auto_cache_plan.has_caller_breakpoints {
            self.warn_volatile_in_caller_prefix(&req);
        }
        // Prefix-rewrite detector -- see `run_walk`. Same once-per-client-
        // request placement above the chain loop, so the streaming path is not a
        // second site that mints an extra observation for the same turn.
        self.observe_prefix_epoch(&req, meta);
        let mut last_err: Option<Error> = None;
        // One learned-capability observation per request per
        // (state_key, feature): the error arm fires per attempt, so this
        // set stops a same-request retry from manufacturing a second
        // observation. See `observe_for_learning`.
        let mut learn_dedupe: HashSet<LearnDedupeKey> = HashSet::new();
        // One reasoning-drop fidelity WARN per client request -- see
        // `run_walk`. Same stack-local once-flag over the dispatch-point
        // emit, so the streaming path is not a second site that repeats it.
        let mut reasoning_drop_warned = false;
        // `repair_budget`: the reactive-repair ceiling for THIS client request
        // -- see `run_walk`. Same above-the-loop position and the same
        // shared ceiling, so the streaming path is not a second walk with its
        // own allowance.

        'chain: for (chain_idx, target) in chain.iter().enumerate() {
            let provider_name = target.provider_name.as_str();
            let state_key = target.state_key.as_str();
            let model = target.upstream.as_str();
            // See `run_walk`: learned re-probe settle guard, scoped to
            // the whole chain-iteration; holds every admission the target owns
            // and drops -> OtherError on any exit. The move takes ownership out
            // of the set so the set's drop cannot settle it again.
            let learned_probe_guard = match probe_set.take(state_key) {
                Some(admissions) => LearnedProbeGuard::armed(
                    self.learned_capabilities.clone(),
                    admissions,
                    probe_set.surface,
                ),
                None => LearnedProbeGuard::inert(),
            };
            let mut learning =
                AttemptLearning::new(self, DispatchSurface::Stream, learned_probe_guard);
            let Some(provider) = target.provider.clone() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            };
            // This entry is being dispatched: record it as the terminal
            // target (overwritten by a later entry on fallback) and count
            // the hop -- see `run_walk` for the `chain_idx` rationale.
            meta.mark_target(target, &req.model);
            meta.fallback_count = chain_idx as u32;

            // Missing-bearer terminal guard, terminal on purpose: see
            // `run_walk`.
            if let Some(err) = missing_forwarded_bearer_error(target, &req) {
                self.count_refusal_error(ingress, target, &err);
                return Err(err);
            }
            // Gemini pairing refusal: see `run_walk`.
            if let Some(err) = egress_pairing_refusal(target, pairing.as_ref()) {
                self.count_refusal_error(ingress, target, &err);
                last_err = Some(err);
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            }

            let mut attempt_req = req.clone();
            // See `run_walk`: a forwarded target keeps the client's
            // requested model verbatim instead of rewriting to `upstream`.
            if !target.use_forwarded_credential {
                attempt_req.model = model.to_string();
                declare_model_rename(&mut attempt_req, target, &req.model);
            }
            apply_layered_overlays(&self.config, target, &mut attempt_req);
            // INTERCEPTOR HOOK (see `run_walk`): strip runs after
            // layered config compose, before auto-cache / context
            // reduction. A strict refusal and a rolled-back hazard both
            // walk on to the next target.
            match self.apply_strip_interceptor(target, &mut attempt_req) {
                StripDecision::Proceed => {}
                StripDecision::StrictReject(err) => {
                    self.count_refusal_error(ingress, target, &err);
                    last_err = Some(err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue;
                }
                StripDecision::RouteAway(err) => {
                    self.count_refusal_error(ingress, target, &err);
                    last_err = Some(err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue;
                }
            }
            // Envelope-field pre-flight (see `run_walk`): same seam,
            // `Stream` surface. The returned plan is held rather than
            // discarded so the modified-request accounting guard lives for the
            // whole chain iteration; a stream never claims a canary (the
            // surface is excluded upstream of the cadence tick), so there is
            // no settlement to owe here.
            // Captured before pre-flight -- see `run_walk`.
            let probe_fields = ProbeFieldCapture::of(self.field_repair_table(), &attempt_req);
            let (preflighted_req, preflight_records, field_preflight_plan) =
                self.plan_field_preflight(&attempt_req, target, DispatchSurface::Stream);
            attempt_req = preflighted_req;
            meta.field_preflight.extend(preflight_records);
            learning.hold_preflight(field_preflight_plan);
            let provider_cfg = self.config.providers.get(provider_name);
            apply_context_reduction(
                &self.config,
                provider_cfg,
                provider_name,
                model,
                fidelity_dialect(target, &attempt_req),
                &mut attempt_req,
                meta,
            );
            // Auto-cache: see `run_walk`. Same once-vs-per-attempt
            // split and the same transactional commit-or-discard; injected
            // on this clone after overlays, before the inner loop. Original
            // `req` is never mutated. The K-gated verdict is consulted at the
            // same seam for the same per-target reason.
            let k_suppressed = self.k_emission_suppressed(
                &auto_cache_plan,
                target,
                req.routectl_internal.inbound_session_key.as_deref(),
            );
            let cache_decision = apply_auto_cache_placement(
                &mut attempt_req,
                &auto_cache_plan,
                cache_target_gates(provider_cfg),
                k_suppressed,
            );
            declare_cache_placement(
                &mut attempt_req,
                target,
                cache_decision,
                auto_cache_plan.front_slot,
            );
            // Cache observability: see `run_walk`. Stamp the per-marker
            // decision tokens and emit the per-request decision at debug. No
            // bodies / secrets: only provider name, model id, stable tokens.
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

            // Steady-state would-trim advisory: see `run_walk`. The same
            // NON-MUTATING shared helper runs on the streaming path so the two
            // dispatch sites never diverge. Never touches `attempt_req`.
            self.record_would_trim(
                &attempt_req,
                target.provider_kind,
                target.nickname.as_deref().unwrap_or(model),
                &target.model.effective_row,
                meta,
            );

            // Reasoning-replay carry admission (see `run_walk`): claim
            // the single-flight slot for each non-portable carried scheme, or
            // strip proactively when an acting negative / peer probe refuses.
            // Envelope-field carry admission -- see `run_walk`. Same
            // pre-dispatch position, same closed-table identity, so the
            // streaming walk is not a second site with its own rules.
            learning.admit_carries(
                target,
                &mut attempt_req,
                meta,
                super::field_repair::FieldSettlementMode::Settling,
                Instant::now(),
            );
            let attempt_policy = self.compose_attempt_policy(
                &policy,
                provider_name,
                target.stream_first_byte_timeout_ms,
            );
            // Per-target one-shot auth-recovery: a 401 from the
            // pre-content attempt triggers on_auth_failure (forced
            // refresh through the OAuth store's per-provider mutex)
            // and exactly one retry. Streams don't have their own
            // retry policy (mid-stream errors propagate), and this
            // recovery only covers the PRE-CONTENT window --
            // once `wrap_with_breaker_accounting` is wrapping a live
            // stream, a 401 surfacing as a mid-stream chunk error
            // propagates to the caller without auth-recovery (rare
            // for current upstreams that don't revalidate per-chunk;
            // documented gap if a future provider does). A refresh
            // failure propagates immediately rather than walking the
            // fallback chain over a dead OAuth identity.
            let mut auth_retry_attempted = false;
            let mut attempts_made: u32 = 0;
            loop {
                // Per-attempt gate: rate limit + circuit breaker. Lives
                // INSIDE the loop (mirroring `complete_with_options`) so
                // the auth-401 retry is gated + RPM-debited exactly like
                // the first attempt -- per-attempt accounting is uniform
                // across all three dispatch sites. Streams don't retry on
                // ordinary errors: a later iteration is the auth-recovery
                // retry or a pre-content repaired retry (field repairs may
                // chain, each a further iteration, within the request's
                // shared repair budget). The gate runs once per attempt, so
                // each attempt is debited exactly once.
                //
                // The returned guard OWNS the half-open claim this admission
                // took (see `run_walk`).
                let (refusal, mut probe_guard) = self.admit_dispatch(state_key, provider_name);
                if let Some((gate_kind, gate_err)) = refusal {
                    log_gate_refusal(
                        provider_name,
                        target.nickname.as_deref().unwrap_or(""),
                        gate_kind,
                        &gate_err,
                        "stream gate blocked",
                    );
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

                // Gate granted Allow. Capture NOW whether this dispatch
                // claimed the half-open probe slot: only a probe's first
                // CONTENT chunk should close + release the breaker at the Ok
                // arm below. `probe_guard` (the cancellation backstop, see
                // ProbeSlotGuard) frees the slot if this future is dropped
                // before an outcome arm settles it (e.g. consumer disconnect
                // during the pre-content wait against a hung upstream). It has
                // to be read before any settle site disarms it.
                let was_half_open_probe = probe_guard.owns_slot();

                // Fidelity WARN at the dispatch point -- see `run_walk`.
                Self::warn_dropped_reasoning_dialect(
                    provider_name,
                    target.provider_kind,
                    &attempt_req,
                    &mut reasoning_drop_warned,
                );
                // Lazy probe activation at the same ADMITTED boundary as the
                // complete walk (see there for why it is not the success arm),
                // reading the same pre-flight capture and per-target
                // `attempt_req`. Deduped against the other walks by the shared
                // lane/capability identity, so a session mixing surfaces still
                // queues one job.
                self.on_admitted_request(target, &probe_fields, &attempt_req);
                let translation_report = install_translation_report(&mut attempt_req);
                let beta_assembly = install_beta_assembly(&mut attempt_req);
                let (wire_tap, sample_charge) = self.install_wire_tap(target, &mut attempt_req);
                let r = try_stream_with_first_content(
                    provider_name,
                    model,
                    provider.clone(),
                    attempt_req.clone(),
                    attempt_policy.stream_first_byte_timeout_ms,
                )
                .await;
                attempts_made += 1;
                meta.attempt_count += 1;
                match r {
                    Ok(stream) => {
                        // A half-open PROBE that produced first CONTENT has
                        // proven the upstream live -- close the breaker NOW
                        // (release the single probe slot) rather than
                        // holding it for the whole stream duration, which
                        // would lock out all concurrent requests to this
                        // model until the stream ends. A leading content-free
                        // role chunk does NOT reach here: the Ok arm fires
                        // only once `try_stream_with_first_content` has seen
                        // content. Gate this on `was_half_open_probe`: for a
                        // HEALTHY (closed) breaker first content must NOT reset
                        // the failure counter, or mid-stream errors could never
                        // accumulate toward the threshold (each stream's
                        // first-content reset would zero the count).
                        //
                        // Closing here clears the half-open flag, so a
                        // mid-stream failure the wrap below debits is
                        // counted as a normal failure accumulating toward
                        // `circuit_failures` -- a probe that delivered
                        // content then errors does NOT get a special immediate
                        // re-trip (and a probe's 429/529 is not debited at
                        // all). With circuit_failures = 1 a single
                        // post-close mid-stream error re-quarantines at once
                        // (fast-flap); with >= 2 a still-degraded upstream
                        // may serve up to that many content-then-error
                        // streams before re-opening -- the throughput-vs-
                        // quarantine tradeoff of closing on first content
                        // (see runtime_state.rs).
                        let state = self.state.get(state_key).cloned();
                        // First content proves the credential works, whatever
                        // the breaker phase was.
                        self.settle_seat_success(state_key, was_half_open_probe, &probe_guard);
                        // The probe (if any) is settled; the wrapped stream's
                        // BreakerAccounting owns the tail. Disarm so a drop here
                        // does not free a slot a later probe may hold.
                        probe_guard.disarm();
                        sample_charge.keep();
                        self.check_request_wire(target, &attempt_req, wire_tap.as_ref());
                        // First content proves the capability is not rejected:
                        // clear any learned negative this dispatch re-probed,
                        // and ride each clear out on the meta so the ledger
                        // records it and a warm rebuild does not resurrect it.
                        // Beta flags the engine's repair withheld settle
                        // first, as rejections. A capability the egress
                        // withheld was never tested, so it is not cleared.
                        let untranslated = UntranslatedCapabilities::take_from(&translation_report);
                        // Then settle the replay carry and the envelope-field
                        // carry against this first content (see
                        // `run_walk`); a stream holds no canary.
                        // First content is not the end of the response, so the
                        // evidence scope rides out on the meta and the caller
                        // settles it once the stream has ended naturally.
                        meta.stream_evidence = learning
                            .on_success(
                                &untranslated,
                                target,
                                &req,
                                &attempt_req,
                                &mut learn_dedupe,
                                meta,
                            )
                            .map(StreamEvidence::new);
                        // Stamp the client-visible label on every Ok chunk
                        // (including the terminal / usage-only chunk) before
                        // the breaker wrap. The closure owns the label String
                        // (moved in via `move`) to satisfy the 'static
                        // BoxStream bound; `chunk.model` needs an owned String,
                        // so the label is cloned per Ok chunk. `Err` passes
                        // through byte-for-byte unchanged.
                        let label = resolve_reported_model(target, &req.model);
                        declare_model_relabel(wire_tap.as_ref());
                        self.carry_response_check(target, &attempt_req, wire_tap.as_ref(), meta);
                        let relabeled = stream
                            .map(move |item| match item {
                                Ok(mut chunk) => {
                                    chunk.model = label.clone();
                                    Ok(chunk)
                                }
                                Err(e) => Err(e),
                            })
                            .boxed();
                        let folded = match &meta.stream_evidence {
                            Some(evidence) => evidence.wrap(relabeled),
                            None => relabeled,
                        };
                        return Ok(wrap_with_breaker_accounting(
                            folded,
                            state,
                            target.provider_kind,
                            // Subscription-quota feed, the STREAMING half.
                            // Streaming quota rides the FIRST canonical chunk
                            // only, so it is lifted as the chunk passes through
                            // the wrap -- before the caller renders it -- rather
                            // than at end-of-stream, where it is already gone.
                            // The feed disarms itself on the first chunk
                            // carrying metadata, so it reads once per stream and
                            // never per chunk. Keyed on the seat that ACTUALLY
                            // served, exactly as the non-streaming arm is.
                            crate::quota::feed::FirstChunkFeed::armed(
                                self.quota_store.clone(),
                                meta.served_seat
                                    .as_deref()
                                    .map(crate::quota::key::seat_key_for_served_identity),
                            ),
                            is_probe,
                            target.class_overrides.clone(),
                            policy.max_honored_retry_after(),
                        ));
                    }
                    Err(mut e) => {
                        let rejection_site = self.settle_failed_attempt(
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
                        // A forwarded-credential TARGET that drew a
                        // 401/403/429 is TERMINAL -- bypass the
                        // on_auth_failure refresh (below) AND the fallback
                        // hop, surface verbatim. Keyed off the CURRENT
                        // target's `use_forwarded_credential`, not
                        // request-global bearer presence (see
                        // `run_walk` for the full rationale). This
                        // is the pre-content window; a mid-stream error
                        // never reaches here (it rides the wrapped stream).
                        // Release the half-open slot without a breaker debit.
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
                        // Auth-401 single-flight refresh + retry once
                        // (pre-content only). A refresh failure means
                        // the OAuth identity is dead; surface it without
                        // walking the chain, but first release any half-open
                        // probe slot this attempt claimed at the gate or the
                        // breaker stays locked open until restart.
                        if !auth_retry_attempted
                            && matches!(&e, Error::Upstream { status: 401, .. })
                        {
                            auth_retry_attempted = true;
                            tracing::debug!(
                                provider = provider_name,
                                model = %routectl_core::sanitize_for_log(target.nickname.as_deref().unwrap_or("")),
                                attempt = attempts_made,
                                "stream 401 pre-content; refreshing auth and retrying once",
                            );
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
                            // fresh slot.
                            probe_guard.release();
                            // Preserve the genuine upstream 401 as last_err
                            // before re-gating (see `run_walk`): if the
                            // re-gate refuses, the `last_err.is_none()` guard
                            // keeps the real 401 over the synthetic gate error.
                            last_err = Some(e);
                            continue;
                        }
                        // The learner repair branches (see `run_walk`):
                        // replay strip, envelope field, named beta, each a FIXED
                        // correctness branch that re-dispatches this target
                        // exactly ONCE. Streams take no in-loop backoff, so the
                        // retry is immediate; a stream holds no canary.
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
                            // re-gating the repaired attempt (see
                            // `run_walk`): if the re-gate refuses, the
                            // `last_err.is_none()` guard keeps the real
                            // rejection over the synthetic gate error.
                            last_err = Some(last_err_body_free.unwrap_or(e));
                            continue;
                        }
                        if let Some(body_free) =
                            replay_rejection_body_free(&e, &cf.class, provider_name)
                        {
                            e = body_free;
                        }
                        let do_fallback =
                            should_fallback(&e, &cf.class, &policy, is_probe, transcript_rejection);
                        // Stream dispatch never retries the same provider (no
                        // in-loop sleep), so a reset hint only decides park
                        // versus debit, by the rule every surface shares
                        // (`breaker_outcome`).
                        let reset_hint = rate_limit_reset_hint(&e, &policy);
                        let outcome = breaker_outcome(&e, &cf.class, reset_hint, is_probe);
                        // Probe fast-fail: a probe that hit a rate-limit/
                        // overload (429/529) returns the status immediately
                        // -- no fallback. The shared rule gives it no breaker
                        // debit, so it only frees the half-open slot it may
                        // have claimed at the gate. Streams never retry the
                        // same provider, so there is no can_retry_here to
                        // guard.
                        if let Some(status) = probe_fast_fail(&e, is_probe) {
                            log_probe_fast_fail(
                                provider_name,
                                target.nickname.as_deref().unwrap_or(""),
                                status,
                                req.max_tokens,
                            );
                            self.apply_breaker_outcome(state_key, outcome, &cf.class);
                            probe_guard.settle(outcome);
                            // Fail over to a sibling seat of the SAME pool
                            // before fast-failing (see `run_walk`): a
                            // rate-limited seat does not mean the pool is out of
                            // quota. Fast-fail across DISTINCT chain targets and
                            // once the pool's seats are exhausted.
                            if !opts.disable_fallbacks
                                && next_is_sibling_seat(&chain, chain_idx, target)
                            {
                                last_err = Some(e);
                                continue 'chain;
                            }
                            return Err(e);
                        }
                        // The DEBIT keys off the failure CLASS, not the
                        // chain-hop decision (see `run_walk`): a seat is
                        // unhealthy when it emits a transient-health failure,
                        // independent of routing. A non-debiting class that
                        // still falls back releases its slot without a debit
                        // in the fallback branch below.
                        let debits = class_debits(&cf.class);
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
                        self.emit_class_observability(
                            &e,
                            &cf,
                            &original_class,
                            remapped,
                            remap_status,
                            DispatchSurface::Stream,
                            provider_name,
                            target,
                            do_fallback,
                            &policy,
                            debits,
                            is_probe,
                            is_forwarded,
                            ingress,
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
                        if do_fallback {
                            let has_next = chain_idx + 1 < chain_len;
                            if has_next {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %routectl_core::sanitize_for_log(target.nickname.as_deref().unwrap_or("")),
                                    error = ?e,
                                    "stream fallback to next",
                                );
                            } else {
                                // The previous shape WARNed "fallback to next"
                                // with `provider=<self> model=<self>` because
                                // we always log the SOURCE of the hop, not the
                                // target. On a single-entry chain (or the
                                // final entry of a longer chain) there is no
                                // next target -- the loop exits and the
                                // request fails. Log accordingly so an
                                // operator triaging a misleading "fallback
                                // happened" line sees what actually
                                // happened.
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %routectl_core::sanitize_for_log(target.nickname.as_deref().unwrap_or("")),
                                    error = ?e,
                                    "stream chain exhausted; no fallback target available; request will fail",
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

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
    }

    /// Observability seam at the router's class-decision point. Bumps the
    /// fail-closed-unknown / feature-unsupported counters, emits the stable
    /// FeatureUnsupported event when applicable, and emits exactly one
    /// class-decision event (DEBUG, or WARN on an Unknown-classified
    /// upstream outcome). Called from BOTH dispatch error arms with only
    /// safe, structured facts -- never the error body / Display string.
    /// `cf` is the EFFECTIVE (post-remap) classification every other
    /// consumer already acted on; `original_class` is the classifier's
    /// pre-remap decision, carried through purely for provenance.
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    pub(super) fn emit_class_observability(
        &self,
        err: &Error,
        cf: &ClassifiedFailure,
        original_class: &FailureClass,
        remapped: bool,
        remap_status: Option<u16>,
        surface: DispatchSurface,
        provider: &str,
        target: &DispatchTarget,
        fallback: bool,
        policy: &RetryPolicy,
        debit: bool,
        is_probe: bool,
        is_forwarded: bool,
        ingress: Option<IngressDialect>,
    ) {
        if let Some(reason) = err.refusal_reason() {
            self.count_local_refusal(ingress, target, reason);
        }
        let facts = upstream_facts(err);
        let model = target.nickname.as_deref().unwrap_or("");
        match &cf.class {
            FailureClass::FeatureUnsupported { capability } => {
                self.metrics.incr_feature_unsupported();
                emit_feature_unsupported(
                    provider,
                    target.provider_kind,
                    model,
                    capability,
                    &facts,
                    cf.matched_by,
                    surface,
                    is_forwarded,
                    remapped,
                );
            }
            FailureClass::Unknown if facts.status.is_some() => {
                self.metrics.incr_unknown_failure_classification();
            }
            // The window gate runs during chain construction, strictly
            // before dispatch -- so every target reaching this arm cleared
            // the gate. Paired with the gate's own skip counter; see the
            // counter's doc comment for the remap-mixing caveat this arm
            // is subject to.
            FailureClass::ContextWindow => {
                self.metrics.incr_context_window_overflow();
            }
            _ => {}
        }
        emit_class_decision(&ClassDecisionObs {
            provider,
            model,
            surface,
            original_class,
            effective_class: &cf.class,
            matched_by: cf.matched_by,
            facts,
            fallback,
            retry_cap: retry_cap_for(&cf.class, policy),
            hard_retry_cap: policy.hard_retry_cap(),
            debit,
            is_probe,
            is_forwarded,
            remapped,
            remap_status,
        });
    }

    /// Resolve the retry policy for the wire `model` field. v0.6.0
    /// removed per-alias retry overrides; the only retry policy is
    /// the top-level `[retry]` table. Pre-v0.6.0 each `[aliases.X]`
    /// could carry a `[aliases.X.retry]` sub-table; that surface was
    /// dropped when `[aliases]` collapsed into a flat
    /// wire-string -> nickname-or-chain map. Operators wanting
    /// different retry behavior per target can split routes into
    /// distinct `[providers.X]` entries (timeouts) or use
    /// per-error-class caps in `[retry]`. The `model` argument is
    /// retained for forward-compat: a future per-model retry surface
    /// would key off this value.
    pub(super) fn policy_for(&self, _model: &str) -> RetryPolicy {
        self.config.retry.clone()
    }

    /// Overlay the target provider's timeout config onto the
    /// resolved `RetryPolicy`. Provider-level fills in only when the
    /// base left the field None. Both None falls through to reqwest's
    /// default.
    ///
    /// Resolution precedence for `stream_first_byte_timeout_ms`:
    /// per-model (`model_first_byte_timeout_override`) >
    /// per-provider (`ProviderRuntimePolicy.stream_first_byte_timeout_ms`) >
    /// global (`[retry].stream_first_byte_timeout_ms` baked into
    /// `base`). When the per-model override is `Some`, it wins
    /// unconditionally over the provider + global tiers.
    pub(super) fn compose_attempt_policy(
        &self,
        base: &RetryPolicy,
        provider_name: &str,
        model_first_byte_timeout_override: Option<u64>,
    ) -> RetryPolicy {
        let provider_runtime = self
            .config
            .providers
            .get(provider_name)
            .map(crate::config::ProviderEntry::runtime);
        let mut out = base.clone();
        if out.request_timeout_ms.is_none() {
            out.request_timeout_ms = provider_runtime.and_then(|p| p.request_timeout_ms);
        }
        // Per-model override wins unconditionally over the
        // provider + global resolution.
        if let Some(ms) = model_first_byte_timeout_override {
            out.stream_first_byte_timeout_ms = Some(ms);
        } else if out.stream_first_byte_timeout_ms.is_none() {
            out.stream_first_byte_timeout_ms =
                provider_runtime.and_then(|p| p.stream_first_byte_timeout_ms);
        }
        out
    }

    /// NON-MUTATING would-trim advisory recording, gated by ONE independent
    /// check: the request's estimated token count clears
    /// `params.trigger_tokens`. Below the trigger, records no would-trim
    /// advisory at all (every `would_trim_*` field stays `None`) and returns
    /// early. At or above it, runs TWO independent measurements against the
    /// same resolved `(provider_kind, model)` pricing row:
    ///
    /// 1. The shipped size-baseline plan (`propose_steady_state_trim`):
    ///    freed-token count `d`, break-even K*, `k_floor`, and the shadow
    ///    misfire monitor. Records nothing when `propose` finds no cut (its
    ///    own columns stay `None`).
    /// 2. The near-lossless pass (`record_near_lossless_marks`): dedup /
    ///    supersession attribution, path-extractability counts, the
    ///    recorder-version marker, the raw-marks blob, and the
    ///    context-fraction advisory. Runs INDEPENDENTLY of whether (1)
    ///    found a cut, so near-lossless opportunity is measured even where
    ///    the size baseline declines.
    ///
    /// `meta.calib_estimated_tokens` is the ONE exception to the trigger
    /// gate: it is stamped before the early return, for every dispatched
    /// attempt. It is calibration evidence, not a trim advisory -- gating it
    /// on size would train a correction factor on large requests only. This
    /// stamp is the estimate for the request as it stands HERE, before the
    /// retry loop; a later reasoning-replay strip shrinks the dispatched
    /// payload and re-stamps it (see
    /// `strip_replay_artifacts_recalibrating`), so the persisted value always
    /// describes the bytes that actually went upstream.
    ///
    /// CRITICAL: this NEVER mutates `attempt_req`. Both measurements only
    /// read the request and never call `apply_trim_plan`, so the bytes sent
    /// upstream are identical with or without this call. It is extracted
    /// into ONE helper -- invoked from both `run_walk` and
    /// `stream_inner` -- so the two byte-identical dispatch blocks cannot
    /// drift.
    ///
    /// An unknown / unresolved-catalog target (e.g. `provider_kind` is
    /// `None` for a legacy / direct-construction target, or the two-layer
    /// merge resolves `Disabled` / `Missing`) records freed-token /
    /// attribution counts but no break-even K*: a merge result other than
    /// `Present` carries no trusted multipliers, so a break-even number
    /// would be misleading. This matches the offline prompt-size advisory
    /// convention (compute `break_even_k` only when the merge resolved
    /// `Present`).
    ///
    /// Additionally, when the size-baseline plan exists, consults the
    /// in-process [`crate::k_estimator::KEstimator`] over the request's
    /// `inbound_session_key` and the SAME pricing row's `ttl_seconds` (which
    /// bounds the age of the samples the estimate may count).
    /// The estimate's `k_floor` is stamped onto `meta.would_trim_k_floor` only
    /// for a `Calibrated` confidence (the only bound the cost gate may
    /// consult to authorize a cut). The met/unmet/cold/unpriced verdict is
    /// DERIVED downstream from the numeric advisory columns
    /// (`would_trim_tokens`, `would_trim_break_even_k`, `would_trim_k_floor`);
    /// this function never overwrites `meta.reduction_strategy`, which
    /// remains owned by the reduction path. Advisory only -- the
    /// dispatched bytes never change.
    ///
    /// The model dimension is the served NICKNAME throughout. Every store this
    /// function touches is a K-side store -- the estimator's
    /// [`crate::k_estimator::KQuery`] and the shadow misfire monitor -- and
    /// [`Router::record_k_sample`] writes each per-session K window under the
    /// served nickname (`meta.served_model` == `target.nickname`), so keying on
    /// it is what makes the read triple byte-identical to the sample-write
    /// triple. Keying on the upstream wire id instead silently misses the store,
    /// holds every estimate at `Cold`, and leaves `would_trim_k_floor`
    /// permanently `None`. Pricing needs the upstream id, but it is looked up at
    /// chain-build time and arrives here already resolved as `effective`, so
    /// this function never sees the wire id at all. When a target has no
    /// nickname (`served_model` falls back to the wire id), no sample is ever
    /// recorded for that dispatch either, so the fallback never mis-keys a
    /// populated window.
    pub(super) fn record_would_trim(
        &self,
        attempt_req: &ChatRequest,
        provider_kind: Option<&'static str>,
        served_model: &str,
        effective: &EffectiveRow,
        meta: &mut DispatchMeta,
    ) {
        let params = self.config.trim.to_params();
        let estimated_tokens = estimate_total_tokens(attempt_req);
        // The calibration numerator is stamped HERE, before the trigger
        // check, so it lands for every dispatched attempt rather than only
        // for the large ones the trim advisories select for. It reuses the
        // one estimate this function already needs, so an unconditional
        // stamp costs no extra serialization of the request.
        meta.calib_estimated_tokens = Some(estimated_tokens);
        if estimated_tokens <= params.trigger_tokens {
            return;
        }

        // The two-layer merge (baked catalog + overlay) is resolved
        // once at chain-build time and rides the resolved target
        // (`ResolvedModel::effective_row`, stamped by
        // `factory::apply_catalog_overlay`) -- this dispatch-path function
        // only reads the precomputed result, never re-runs
        // `lookup_baked_with_overrides` + `merge` per request. `Present`
        // prices normally; `Disabled` / `Missing` fold to the SAME
        // conservative sentinel behavior (`row` below is `None`). Shared
        // by both the size-baseline block below and the near-lossless
        // pass, so the two measurements price against the identical
        // resolved cell.
        let row = effective.priced();

        if let Some(plan) = propose_steady_state_trim(attempt_req, &params) {
            meta.would_trim_tokens = Some(plan.candidate.d);
            let break_even = row.and_then(|r| break_even_k(r, &plan.candidate));
            meta.would_trim_break_even_k = break_even;

            // Consult the K estimator over the SAME row whose TTL priced K*:
            // the estimator counts only samples younger than that TTL, so a
            // `k_floor` is measured over the same cache period `break_even`
            // is priced for and the two are comparable. The current sample
            // for THIS turn is recorded post-response in `record_k_sample`,
            // so the estimator reads PRIOR-turn samples only. The triple comes
            // from the shared `k_query_key` derivation, which keys the model
            // dimension on `served_model` (the nickname), NOT the upstream
            // `model`, so this read matches the triple `record_k_sample`
            // writes under. A `Disabled` / `Missing` cell has no row to read
            // `ttl_seconds` from; the sentinel's TTL is the same conservative
            // default used everywhere else a trusted row is unavailable.
            let ttl_seconds = row.map_or(CatalogRow::sentinel().ttl_seconds, |r| r.ttl_seconds);
            let k_key = k_query_key(
                attempt_req.routectl_internal.inbound_session_key.as_deref(),
                provider_kind,
                served_model,
            );
            let estimate = self.k_estimator.estimate(&k_key.query(
                Duration::from_secs(u64::from(ttl_seconds)),
                SystemTime::now(),
            ));

            meta.would_trim_k_floor = would_trim_k_floor_for_meta(break_even, &estimate);

            // Shadow misfire monitor: recording only, never mutates attempt_req.
            // Compute a fingerprint of the trimmed cacheable prefix and compare
            // it against the stored value for this triple. A Misfire means the
            // prefix shifted turn-to-turn -- the canary that a real cut would
            // break the upstream cache.
            //
            // Keyed off the SAME `k_key` the estimate above was read under, so
            // the monitor and the estimator describe ONE population. The
            // monitor exists to explain the estimator's behavior; keying it on
            // the upstream wire id instead would silently split the two under
            // any alias whose nickname differs from that id. `store_key` is
            // `None` exactly when the request is keyless, which is also when
            // there is no window to monitor.
            if let (Some(session_key), Some(shadow_key)) = (
                attempt_req.routectl_internal.inbound_session_key.as_deref(),
                k_key.store_key(),
            ) {
                let fp = trimmed_prefix_fingerprint(attempt_req, &plan);
                let outcome =
                    self.shadow_store
                        .record_and_compare(shadow_key, fp, SystemTime::now());
                match outcome {
                    crate::k_estimator::ShadowOutcome::Stable => {
                        meta.would_trim_shadow_misfire = Some(0);
                    }
                    crate::k_estimator::ShadowOutcome::Misfire => {
                        meta.would_trim_shadow_misfire = Some(1);
                        // The session key is caller-controlled and the schema
                        // forbids logging it raw, but the misfire is only
                        // actionable if an operator can correlate the same
                        // triple across lines -- so the key rides as a hash
                        // salted per process. Correlation within a run is all
                        // the event needs, and the per-process salt denies an
                        // offline dictionary attack on a client that keys its
                        // session by a stable per-user string.
                        //
                        // `model` renders the served nickname, matching the
                        // dimension the entry is keyed under: an operator
                        // correlating an event against a K window must read one
                        // label, not two.
                        tracing::debug!(
                            session_key_hash = crate::log_hash::salted_log_hash(session_key),
                            provider_kind = provider_kind.unwrap_or(""),
                            model = %routectl_core::sanitize_for_log(served_model),
                            "would_trim_shadow_misfire: trimmed cacheable prefix shifted turn-to-turn",
                        );
                    }
                    crate::k_estimator::ShadowOutcome::FirstSeen => {}
                }
            }
        }

        record_near_lossless_marks(attempt_req, &params, row, meta);
    }

    /// Should BOTH auto-cache markers be withheld from this target because
    /// the session's measured per-turn reuse cannot pay the write premium?
    ///
    /// Consulted ONCE per chain entry, immediately before
    /// `apply_auto_cache_placement` and above the same-target retry loop, so
    /// every retry of one target dispatches byte-identical bytes while a
    /// fallback target re-evaluates against ITS own row and triple. The
    /// verdict is per-(session, provider_kind, served nickname, priced row),
    /// which is why it is passed as an argument rather than stored on
    /// `AutoCacheRequestPlan` (whose facts are target-invariant).
    ///
    /// Every branch fails toward EMIT -- the direction that preserves the
    /// previously-shipped auto-emission behavior and can only cost a wasted
    /// write premium, never lost output.
    /// The short-circuits are ordered cheapest-first and all of them read
    /// fields already in hand, so an ineligible dispatch allocates nothing:
    ///
    /// 1. `[cache] k_gated_emission` off (the default) -- no consultation at
    ///    all, so the marker-emission behavior is preserved exactly.
    /// 2. the request carries caller breakpoints -- placement defers for its
    ///    own reason and this gate must not shadow that token.
    /// 3. the global auto-emit switch is off -- likewise.
    /// 4. no inbound session key -- there is no triple to accumulate reuse
    ///    against, so no evidence can exist.
    /// 5. no served nickname -- the sample write skips such a target
    ///    entirely, so its window is permanently cold.
    ///
    /// Only then is the K triple built and the estimator consulted. The
    /// numeric compare is additionally gated on `may_suppress` (Calibrated
    /// only) and on the row having a finite emission break-even: an
    /// auto-cacher, a premium-free row, or an untrusted (`Disabled` /
    /// `Missing`) merge result never suppresses.
    pub(super) fn k_emission_suppressed(
        &self,
        plan: &AutoCacheRequestPlan,
        target: &DispatchTarget,
        session_key: Option<&str>,
    ) -> bool {
        if !self.config.cache.k_gated_emission
            || plan.has_caller_breakpoints
            || !plan.global_auto_emit_enabled
        {
            return false;
        }
        let (Some(session_key), Some(served_model)) = (session_key, target.nickname.as_deref())
        else {
            return false;
        };

        let Some(row) = target.model.effective_row.priced() else {
            return false;
        };
        let Some(break_even) = emission_break_even_k(row) else {
            return false;
        };

        let key = k_query_key(Some(session_key), target.provider_kind, served_model);
        let estimate = self.k_estimator.estimate(&key.query(
            Duration::from_secs(u64::from(row.ttl_seconds)),
            SystemTime::now(),
        ));
        may_suppress(&estimate) && estimate.k_floor < break_even
    }
}

/// Recorder-version marker stamped onto `meta.would_trim_recorder_version`
/// whenever the near-lossless pass runs (the estimated-token trigger
/// cleared), regardless of whether it found any marks. Lets offline
/// reporting filter version-stamped rows from rows written before the
/// recorder existed, without confounding semantics across a deploy boundary.
pub(super) const NEAR_LOSSLESS_RECORDER_VERSION: i64 = 1;

/// NON-MUTATING near-lossless attribution pass. Measures the dedup +
/// supersession opportunity over the SAME `[head_keep, scan_end)` window the
/// shipped trimmer scans, INDEPENDENT of whether the shipped size-baseline
/// plan (`propose_steady_state_trim`) found a cut -- called unconditionally
/// by `record_would_trim` once the estimated-token trigger clears. Stamps
/// zero-or-more-marks results as `Some(0)` (a measured zero), distinct from
/// the caller's `None` (pass did not run, below trigger).
///
/// `would_trim_context_fraction` is fail-closed: `None` whenever `row`'s
/// context window is unknown, never a guessed value. `row` is `None` when
/// the two-layer merge resolved `Disabled` / `Missing` -- context_fraction
/// and break-even fold to the same `None` in that case.
///
/// Prices the near-lossless candidate via the UNCHANGED `break_even_k` gate,
/// present-row-only, mirroring the `would_trim_break_even_k` convention --
/// but only LOGS the result via `tracing::debug!`. There is no persisted
/// column for it: unlike the shipped size-baseline plan, the near-lossless
/// candidate has no DispatchMeta / UsageRecord economics field.
///
/// Single O(parts) walk: `collect_near_lossless_marks` performs one forward
/// scan over the request's content parts; this function makes no second
/// pass. It is a SIBLING scan to the one `propose_steady_state_trim` already
/// ran above (by design -- the near-lossless heuristics are a distinct
/// question from the size baseline), not a repeat of it.
fn record_near_lossless_marks(
    attempt_req: &ChatRequest,
    params: &SteadyStateTrimParams,
    row: Option<&CatalogRow>,
    meta: &mut DispatchMeta,
) {
    let n = attempt_req.messages.len();
    let scan_end = n.saturating_sub(params.keep_recent_messages);
    let marks = collect_near_lossless_marks(attempt_req, params.head_keep_messages, scan_end);

    meta.would_trim_recorder_version = Some(NEAR_LOSSLESS_RECORDER_VERSION);
    meta.would_trim_dedup_tokens = Some(marks.dedup_tokens);
    meta.would_trim_supersession_tokens = Some(marks.supersession_tokens);
    meta.would_trim_path_units = Some(marks.path_units);
    meta.would_trim_path_extractable = Some(marks.path_extractable);
    meta.would_trim_context_fraction = row
        .and_then(|r| r.max_context_tokens)
        .map(|window| estimate_total_tokens(attempt_req) as f64 / window as f64);
    meta.would_trim_raw_marks = Some(serde_json::to_value(&marks.marks).unwrap_or(Value::Null));

    if let Some(candidate) = near_lossless_candidate(attempt_req, &marks.marks) {
        let break_even = row.and_then(|r| break_even_k(r, &candidate));
        tracing::debug!(
            dedup_tokens = marks.dedup_tokens,
            supersession_tokens = marks.supersession_tokens,
            break_even_k = ?break_even,
            "near_lossless_candidate priced (log-only, not persisted)",
        );
    }
}

/// The single (session_key, provider_kind, served_model) triple every K
/// window is written and read under.
///
/// Constructed by [`k_query_key`] and projected to whichever side's type the
/// caller needs: [`KQueryKey::query`] for a read
/// ([`crate::k_estimator::KQuery`]), [`KQueryKey::store_key`] for a write
/// ([`crate::k_estimator::KSessionKey`]). Both projections read the SAME
/// three fields, so the write triple and the read triple cannot drift apart
/// field-by-field.
///
/// The model dimension is the served NICKNAME, never the upstream wire id. A
/// mis-keyed read does not fail loudly -- it silently returns another
/// triple's window, or a permanent `Cold`.
pub(super) struct KQueryKey<'a> {
    session_key: Option<&'a str>,
    provider_kind: &'a str,
    served_model: &'a str,
}

/// Derive the K triple for one dispatch.
///
/// `provider_kind` is `None` for a target that never stamped one; it
/// normalizes to `""` so the read side keys deterministically rather than
/// skipping the store. `session_key` is `None` for a keyless request: a read
/// under it is `Cold` by construction and a write is skipped, because there
/// is no stable identity to accumulate against.
pub(super) const fn k_query_key<'a>(
    session_key: Option<&'a str>,
    provider_kind: Option<&'a str>,
    served_model: &'a str,
) -> KQueryKey<'a> {
    KQueryKey {
        session_key,
        provider_kind: match provider_kind {
            Some(kind) => kind,
            None => "",
        },
        served_model,
    }
}

impl<'a> KQueryKey<'a> {
    /// Project to the read-side estimator query. `ttl` and `now` are
    /// per-consult inputs, not part of the triple's identity.
    pub(super) const fn query(
        &self,
        ttl: Duration,
        now: SystemTime,
    ) -> crate::k_estimator::KQuery<'a> {
        crate::k_estimator::KQuery {
            session_key: self.session_key,
            provider_kind: self.provider_kind,
            model: self.served_model,
            ttl,
            now,
        }
    }

    /// Project to the write-side store key. `None` for a keyless request --
    /// the caller has nothing to record against.
    pub(super) fn store_key(&self) -> Option<crate::k_estimator::KSessionKey> {
        Some(crate::k_estimator::KSessionKey {
            session_key: self.session_key?.to_string(),
            provider_kind: self.provider_kind.to_string(),
            model: self.served_model.to_string(),
        })
    }
}

/// Is this estimate's numeric `k_floor` actionable at all?
///
/// True only for `Confidence::Calibrated`. `Low` force-clamps `k_floor` to
/// `0.0` (`k_estimator::default_impl`'s thin-sample clamp) and `Cold`
/// reports an all-zero default, so a bare numeric compare against a
/// break-even K would read those clamps as real evidence of no reuse and
/// suppress every thin-sampled or unseen session. Gating on the confidence
/// class first makes "estimator not calibrated -> do not act" one branch
/// rather than a comparison every caller has to remember to guard.
pub(super) fn may_suppress(estimate: &crate::k_estimator::KEstimate) -> bool {
    estimate.confidence == crate::k_estimator::Confidence::Calibrated
}

/// Pure helper that selects the `would_trim_k_floor` value recorded by
/// `Router::record_would_trim`. Returns `Some(estimate.k_floor)` only when
/// `break_even` is a Present-row K* AND the estimator's confidence is
/// `Calibrated`; every other case records `None`. The met/unmet/cold/
/// unpriced verdict is derived downstream as a pure query over the numeric
/// advisory columns (`would_trim_break_even_k`, `would_trim_k_floor`).
pub(super) fn would_trim_k_floor_for_meta(
    break_even: Option<f64>,
    estimate: &crate::k_estimator::KEstimate,
) -> Option<f64> {
    if break_even.is_some() && may_suppress(estimate) {
        Some(estimate.k_floor)
    } else {
        None
    }
}

/// Resolve the client-visible response `model` label for a served
/// dispatch target. Returns the target's `reported_model` override
/// when set to a non-empty string, otherwise the client's requested
/// alias (`req_model`). An empty override string is treated as unset.
/// Computed once per request so a fallback chain / multi-chunk stream
/// carries one stable label.
pub(super) fn resolve_reported_model(target: &DispatchTarget, req_model: &str) -> String {
    match target.reported_model.as_deref() {
        Some(label) if !label.is_empty() => label.to_string(),
        _ => req_model.to_string(),
    }
}

/// The upstream status of a probe-fast-fail-eligible error, or `None`
/// when `err` is not one. 429 is a rate-limit; 529 is Anthropic's
/// overload status. Both surface as `Error::Upstream { status, .. }`
/// (the anthropic-api egress forwards the raw upstream status). A probe
/// that hits one of these skips retry+fallback: on the all-Anthropic
/// chain every hop shares the same limit, so walking it is futile and
/// the probe's output is unread. A generic 5xx or a capability 4xx
/// (e.g. Bedrock's `max_tokens=1` 400) returns `None` here so it keeps
/// walking the chain -- a healthy sibling provider can still answer.
pub(super) const fn probe_fast_fail_status(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream {
            status: s @ (429 | 529),
            ..
        } => Some(*s),
        _ => None,
    }
}

/// A count_tokens CAPABILITY signal -- the selected target is
/// count_tokens-capable by kind (`anthropic-api`), but its concrete
/// upstream cannot actually count. Two shapes carry this:
///
/// - `Error::NotImplemented` -- the local trait-default (a provider that
///   never overrode `count_tokens`), and
/// - `Error::Upstream { status: 501, .. }` -- a WIRE 501 from an
///   upstream that declares it does not implement the endpoint (e.g. a
///   remote anthropic-api base_url that proxies to an egress which
///   cannot count).
///
/// Both mean "this seat cannot count", NOT "this seat is unhealthy". The
/// count_tokens walk treats them as capability signals: release the
/// probe slot without debiting the breaker and advance to the next
/// capable seat. It must NEVER reach `should_fallback` / `record_failure`
/// -- a capability signal recorded as health would trip the per-seat
/// breaker that completions gate on. Scoped to the count_tokens path
/// ONLY: on the completion path a wire 501 is a genuine upstream fault
/// and must still trip the breaker.
pub(super) const fn is_capability_error(err: &Error) -> bool {
    matches!(
        err,
        Error::NotImplemented(..) | Error::Upstream { status: 501, .. }
    )
}

/// Safe, structured inputs for the per-arm class-decision observability
/// event. Carries only already-structured facts: NEVER a body, prompt,
/// header, token, or the [`Error`] Display/Debug string.
struct ClassDecisionObs<'a> {
    provider: &'a str,
    model: &'a str,
    surface: DispatchSurface,
    /// The classifier's decision BEFORE any operator status remap.
    original_class: &'a FailureClass,
    /// The class every downstream consumer (predicates, debit, this
    /// event) actually acted on -- `original_class` unless a remap fired.
    effective_class: &'a FailureClass,
    /// How the NATIVE classification was decided; unaffected by a remap.
    matched_by: MatchedBy,
    facts: UpstreamFacts<'a>,
    fallback: bool,
    retry_cap: u32,
    /// The policy-wide hard ceiling on same-provider attempts. Invariant:
    /// `hard_retry_cap >= retry_cap` for every class, since the ceiling
    /// folds the per-class overlay. Emitted alongside `retry_cap` so a
    /// drift between the logged cap and the enforced one is visible.
    hard_retry_cap: u32,
    debit: bool,
    is_probe: bool,
    is_forwarded: bool,
    /// Whether the operator's per-provider `class_overrides` replaced the
    /// native class for this error.
    remapped: bool,
    /// The status key that matched an override, iff `remapped`. `None`
    /// when `remapped` is false.
    remap_status: Option<u16>,
}

/// Emit exactly one structured event per error-arm pass at the point the
/// class decision is settled. DEBUG normally; WARN when the classifier
/// failed closed (`FailureClass::Unknown`) on a real upstream outcome --
/// a silent fail-closed-unknown on a genuine upstream response would hide
/// a gap in the status map / token vocabulary. Safe dimensions only.
fn emit_class_decision(obs: &ClassDecisionObs<'_>) {
    let unknown_upstream =
        matches!(obs.effective_class, FailureClass::Unknown) && obs.facts.status.is_some();
    if unknown_upstream {
        tracing::warn!(
            provider = obs.provider,
            model = obs.model,
            surface = obs.surface.as_str(),
            status = ?obs.facts.status,
            upstream_type = obs.facts.upstream_type.unwrap_or(""),
            original_class = class_label(obs.original_class),
            effective_class = class_label(obs.effective_class),
            matched_by = matched_by_label(obs.matched_by),
            fallback = obs.fallback,
            retry_cap = obs.retry_cap,
            hard_retry_cap = obs.hard_retry_cap,
            debit = obs.debit,
            is_probe = obs.is_probe,
            is_forwarded = obs.is_forwarded,
            remapped = obs.remapped,
            remap_status = ?obs.remap_status,
            "unknown failure classification on upstream outcome (fail-closed)",
        );
    } else {
        tracing::debug!(
            provider = obs.provider,
            model = obs.model,
            surface = obs.surface.as_str(),
            status = ?obs.facts.status,
            upstream_type = obs.facts.upstream_type.unwrap_or(""),
            original_class = class_label(obs.original_class),
            effective_class = class_label(obs.effective_class),
            matched_by = matched_by_label(obs.matched_by),
            fallback = obs.fallback,
            retry_cap = obs.retry_cap,
            hard_retry_cap = obs.hard_retry_cap,
            debit = obs.debit,
            is_probe = obs.is_probe,
            is_forwarded = obs.is_forwarded,
            remapped = obs.remapped,
            remap_status = ?obs.remap_status,
            "router failure class decision",
        );
    }
}

#[cfg(test)]
use std::collections::BTreeMap;

#[cfg(test)]
use routectl_core::{
    CacheControl, RefusalReason,
    cache_control::MAX_BREAKPOINTS,
    context_reduction::{ReductionDelta, ReductionOutcome, apply_json_minify},
};

#[cfg(test)]
use super::cache_plan::{CacheInjection, CacheTargetGates, place_front_marker};
#[cfg(test)]
use super::dispatch_loop::reduction_strategy_token;
#[cfg(test)]
use crate::config::CacheCapability;
#[cfg(test)]
use crate::config::{AliasValue, Config};
#[cfg(test)]
use crate::resolved::ResolvedModel;

#[cfg(test)]
#[path = "remap_test_support.rs"]
mod remap_test_support;

#[cfg(test)]
#[path = "remap_tests.rs"]
mod remap_tests;

#[cfg(test)]
#[path = "provider_remap_tests.rs"]
mod provider_remap_tests;

#[cfg(all(test, feature = "bedrock"))]
#[path = "bedrock_class_remap_tests.rs"]
mod bedrock_class_remap_tests;

#[cfg(test)]
#[path = "content_commit_boundary_tests.rs"]
mod content_commit_boundary_tests;

#[cfg(test)]
#[path = "precontent_guard_tests.rs"]
mod precontent_guard_tests;

#[cfg(test)]
#[path = "context_reduction_dispatch_tests.rs"]
mod context_reduction_dispatch_tests;

#[cfg(test)]
#[path = "k_query_key_tests.rs"]
mod k_query_key_tests;

#[cfg(test)]
#[path = "k_gated_emission_tests.rs"]
mod k_gated_emission_tests;

#[cfg(test)]
#[path = "shadow_misfire_log_tests.rs"]
mod shadow_misfire_log_tests;

#[cfg(test)]
#[path = "calibration_evidence_tests.rs"]
mod calibration_evidence_tests;

#[cfg(test)]
#[path = "observability_seam_tests.rs"]
mod observability_seam_tests;

#[cfg(test)]
#[path = "auth_failure_recovery_tests.rs"]
mod auth_failure_recovery_tests;

#[cfg(test)]
#[path = "forwarded_auth_terminal_tests.rs"]
mod forwarded_auth_terminal_tests;

#[cfg(test)]
#[path = "forwarded_coexistence_tests.rs"]
mod forwarded_coexistence_tests;

#[cfg(test)]
#[path = "auto_emit_cache_control_tests.rs"]
mod auto_emit_cache_control_tests;

#[cfg(test)]
#[path = "capability_acceptance_tests.rs"]
mod capability_acceptance_tests;

#[cfg(test)]
#[cfg(feature = "openai-responses")]
#[path = "replay_degradation_observability_tests.rs"]
mod replay_degradation_observability_tests;

#[cfg(test)]
#[path = "reasoning_drop_warn_tests.rs"]
mod reasoning_drop_warn_tests;

#[cfg(test)]
#[path = "opening_usage_fallback_tests.rs"]
mod opening_usage_fallback_tests;

#[cfg(test)]
#[path = "translation_report_learn_tests.rs"]
mod translation_report_learn_tests;

#[cfg(test)]
#[path = "refusal_fallback_tests.rs"]
mod refusal_fallback_tests;
