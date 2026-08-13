//! Dispatch retry state machine. Walks the resolved fallback chain
//! attempting each provider until one succeeds or all are exhausted,
//! retrying within a provider per `RetryPolicy` with exponential
//! backoff. Owns failure-class remap/fallback/retry decisions, breaker
//! accounting, class-decision observability, the would-trim recording +
//! K-floor, context-reduction + auto-cache injection, and forwarded-
//! credential handling for both `complete` and `stream`.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    CacheControl, ChatChunk, ChatRequest, ChatResponse, Error, Provider, ReplayScheme, Result,
    cache_control::{MAX_BREAKPOINTS, validate_source},
    context_reduction::{ReductionOutcome, apply_json_minify},
    failure_class::{
        ClassifiedFailure, FailureClass, LastOutcome, MatchedBy, classify, classify_with_attempt,
    },
    sanitize_for_log, scan_caller_prefix_advisory,
};
use serde_json::Value;

use crate::capability_strip::strip_replay_artifacts;
use crate::catalog::{CatalogRow, EffectiveRow};
use crate::config::{CacheCapability, RetryPolicy};
use crate::context_trim::{
    SteadyStateTrimParams, collect_near_lossless_marks, estimate_total_tokens,
    near_lossless_candidate, propose_steady_state_trim, trimmed_prefix_fingerprint,
};
use crate::cost_gate::break_even_k;
use crate::feature_keys::derive_feature_keys;

use super::ReplayDegradation;
use super::cache_plan::{AutoCacheRequestPlan, CacheInjection};
use super::capability_learn::LearnDedupeKey;
use super::class_observe::{
    DispatchSurface, UpstreamFacts, class_label, matched_by_label, upstream_facts,
};
use super::feature_filter::{StripDecision, emit_feature_unsupported};
use super::overlays::apply_layered_overlays;
use super::runtime_gate::{
    LearnedProbeGuard, ProbeAdmissionSet, is_probe_request, log_probe_fast_fail,
};
use super::{DispatchMeta, DispatchTarget, Dispatched, DispatchedStream, Router, RouterOptions};

/// Message/event name of the single aggregated reasoning-replay
/// degradation WARN. Stable, greppable, closed-set.
const REPLAY_DEGRADE_EVENT: &str = "reasoning_replay_degraded";
/// Action token: the fixed strip-repair correctness branch stripped the
/// carried reasoning artifacts and re-dispatched the same target once.
const REPLAY_ACTION_STRIP_REPAIR: &str = "strip_repair";
/// Reason token: the carried variant drew the proven upstream replay
/// rejection.
const REPLAY_REASON_UPSTREAM_REJECTION: &str = "upstream_replay_rejection";

/// The one provider kind whose egress can represent the
/// OpenAI-Responses-dialect `reasoning.context` / `reasoning.mode`
/// sub-keys. Matches `ProviderEntry::kind_str`, so it round-trips with the
/// `kind = "..."` discriminant in the operator's provider table.
const RESPONSES_PROVIDER_KIND: &str = "openai-responses";

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

/// Emit the single aggregated reasoning-replay degradation WARN for a
/// resolved request. Fires exactly ONCE when the strip-repair branch
/// degraded a carried reasoning artifact anywhere in the chain walk, and
/// not at all otherwise. Carries closed-set tokens and counts only --
/// never the artifact bytes, a reasoning item id, a hash, the session
/// key, or the upstream body. The request span already supplies
/// `request_id` correlation across the retry and fallback hops.
fn emit_replay_degradation(meta: &DispatchMeta) {
    let Some(deg) = meta.replay_degradation.as_ref() else {
        return;
    };
    tracing::warn!(
        action = deg.action,
        target_lane = deg.target_lane.as_str(),
        state_key = %deg.state_key,
        source_schemes = %join_schemes(deg.source_schemes.as_slice()),
        reason = deg.reason,
        artifact_count = deg.artifact_count,
        repair_attempted = deg.repair_attempted,
        repair_succeeded = deg.repair_succeeded,
        learned = deg.learned,
        "{REPLAY_DEGRADE_EVENT}",
    );
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

/// A body-free rebuild of a classified reasoning-replay rejection, or
/// `None` when the class is anything else (leaving the original error
/// untouched -- no clone on the common path). The upstream body of a
/// replay rejection is a rejection envelope that can echo a reasoning
/// artifact fragment, and the generic retry/fallback logs below
/// debug-render the `Error`. The rebuilt `Upstream` keeps the status and
/// the already-structured classifier tokens every downstream consumer
/// reads, dropping ONLY the body.
fn replay_rejection_body_free(err: &Error, class: &FailureClass, provider: &str) -> Option<Error> {
    if !Router::is_replay_rejection_class(class) {
        return None;
    }
    let facts = upstream_facts(err);
    Some(Error::upstream_full(
        provider,
        facts.status.unwrap_or(0),
        "",
        None,
        facts.upstream_type.map(str::to_string),
        facts.upstream_code.map(str::to_string),
    ))
}

/// The largest upstream reset hint we honor as an in-loop, same-provider
/// retry sleep (blocking the request thread). A reset at or below this
/// cap is folded into the next backoff sleep; a larger reset parks the
/// provider via the breaker instead, so the request falls over to a
/// sibling rather than blocking on a multi-minute (or hostile) hint.
const INLOOP_RETRY_AFTER_CAP: Duration = Duration::from_secs(5);

impl Router {
    /// Advisory-only WARN when the region the CALLER marked cacheable carries
    /// per-request-volatile content (fresh ids/timestamps): such a prefix
    /// writes a fresh cache entry every request that is never re-read. Reads
    /// the ORIGINAL request (caller breakpoints only; auto-emit is routectl's
    /// own and exempt), mutates nothing, and never logs a raw value -- only
    /// the structural component, volatile kind, and breakpoint position.
    /// Edge-triggered: at most one WARN per process per (component, kind).
    fn warn_volatile_in_caller_prefix(&self, req: &ChatRequest) {
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
    fn warn_dropped_reasoning_dialect(
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
            provider = %provider_name,
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
        let result = self.complete_inner(req, opts, &mut meta).await;
        emit_replay_degradation(&meta);
        Dispatched { meta, result }
    }

    /// Dispatch-loop body for `complete_with_options`. Mutates `meta` as
    /// the chain is walked -- `attempt_count` on every upstream attempt,
    /// `fallback_count` on every hop to a later entry, and the served_*
    /// fields on each dispatched target -- so the caller's `meta` is
    /// correct at every early return, including the all-failed Err path.
    async fn complete_inner(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
        meta: &mut DispatchMeta,
    ) -> Result<ChatResponse> {
        let (chain, probe_admissions) = self.dispatch_chain_for_request(&req)?;
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
            DispatchSurface::Complete.as_str(),
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
        let chain_len = chain.len();
        let policy = self.policy_for(&req.model);
        let hard_cap = policy.hard_retry_cap();
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
        let auto_cache_plan =
            AutoCacheRequestPlan::build(&req, self.config.cache.auto_emit_top_level_breakpoint);
        if auto_cache_plan.has_caller_breakpoints {
            self.warn_volatile_in_caller_prefix(&req);
        }
        let mut last_err: Option<Error> = None;
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

        'chain: for (chain_idx, target) in chain.iter().enumerate() {
            let provider_name = target.provider_name.as_str();
            let state_key = target.state_key.as_str();
            let model = target.upstream.as_str();
            // Learned re-probe settle guard for THIS target, scoped to the
            // whole chain-iteration (persists across same-provider retries,
            // drops -> OtherError on any exit). Inert unless the filter
            // admitted at least one probe for this state_key; the move takes
            // ownership out of the set so the set's drop cannot settle it again.
            let mut learned_probe_guard = match probe_set.take(state_key) {
                Some(admissions) => LearnedProbeGuard::armed(
                    self.learned_capabilities.clone(),
                    admissions,
                    probe_set.surface,
                ),
                None => LearnedProbeGuard::inert(),
            };
            let Some(provider) = target.provider.clone() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            };
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
            if let Some(err) = missing_forwarded_bearer_error(target, &req) {
                return Err(err);
            }

            let mut attempt_req = req.clone();
            // A forwarded target keeps the client's requested model
            // verbatim (model transparency); an own target rewrites to
            // this target's `upstream` as before.
            if !target.use_forwarded_credential {
                attempt_req.model = model.to_string();
            }
            // v0.6: layered config compose. The provider's
            // header_extras + payload_extras are looked up by
            // provider_name; the model's contribution lives on the
            // dispatch target.
            apply_layered_overlays(&self.config, target, &mut attempt_req);
            // INTERCEPTOR HOOK: strip runs after layered config compose,
            // before auto-cache / context reduction. Any future interceptor
            // over the per-attempt canonical clone goes here, ordered by
            // data-dependency. A strict-mode refusal returns the 400 for
            // this attempt; a rolled-back post-strip hazard routes away
            // (advance the chain as an ordinary route-away would).
            match self.apply_strip_interceptor(target, &mut attempt_req) {
                StripDecision::Proceed => {}
                StripDecision::StrictReject(err) => return Err(err),
                StripDecision::RouteAway(err) => {
                    last_err = Some(err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue;
                }
            }
            let provider_cfg = self.config.providers.get(provider_name);
            // Context reduction: runs strictly AFTER overlays and strictly
            // BEFORE auto-cache so any auto-emitted breakpoint covers the
            // REDUCED bytes. `apply_json_minify` computes the caller frozen
            // floor itself (off this pre-auto-emit clone) and only touches
            // the mutable tail; it fails closed (no mutation on any doubt).
            // Effective only when the global switch is on AND the provider
            // did not explicitly opt out (`None` inherits the global).
            let reduction_effective = self.config.reduction.enabled
                && provider_cfg.and_then(crate::config::ProviderEntry::reduction_enabled)
                    != Some(false);
            let reduction_outcome = if reduction_effective {
                Some(apply_json_minify(&mut attempt_req))
            } else {
                None
            };
            let reduction_token =
                reduction_strategy_token(reduction_effective, reduction_outcome.as_ref());
            meta.reduction_strategy = Some(reduction_token);
            if let Some(ReductionOutcome::Applied(delta)) = &reduction_outcome {
                tracing::debug!(
                    provider = %provider_name,
                    model = %model,
                    strategy = reduction_token,
                    strings_minified = delta.strings_minified,
                    bytes_saved = delta.bytes_saved,
                    est_tokens_saved = delta.est_tokens_saved,
                    "context_reduction",
                );
            }
            // Auto-cache: maybe inject a top-level cache_control breakpoint
            // on THIS per-attempt clone, after overlays (the last
            // dispatch-time touch of cache_control) and before the retry
            // loop, so every retry on this target reuses identical bytes.
            // Never mutates the original `req`; any doubt sends un-injected.
            let cache_injection = maybe_apply_auto_cache_control(
                &mut attempt_req,
                &auto_cache_plan,
                provider_cfg.map(crate::config::ProviderEntry::cache_capability),
                provider_cfg
                    .and_then(crate::config::ProviderEntry::auto_emit_top_level_breakpoint)
                    .unwrap_or(true),
            );
            // Cache observability: stamp the per-request decision token so the
            // outcome log can see what was decided (the usage DB's
            // `strategy` column is write-stopped), and
            // emit the per-request decision at debug. No bodies / secrets:
            // only provider name, model id, and the stable strategy token.
            let strategy_token = cache_injection.strategy_str();
            meta.cache_strategy = Some(strategy_token);
            tracing::debug!(
                provider = %provider_name,
                model = %model,
                strategy = strategy_token,
                "cache_auto_decision",
            );

            // Steady-state would-trim advisory: NON-MUTATING. Reads the
            // dispatched clone, prices the would-cut candidate, and records it
            // onto `meta`. Never touches `attempt_req` (no `apply_trim_plan`),
            // so what is sent upstream is byte-identical with or without this.
            self.record_would_trim(
                &attempt_req,
                target.provider_kind,
                model,
                target.nickname.as_deref().unwrap_or(model),
                &target.model.effective_row,
                meta,
            );

            // Reasoning-replay carry admission: claim the single-flight slot
            // for every non-portable artifact scheme this request carries
            // toward the target lane. `Some` holds the guards and leaves the
            // carried variant intact; `None` either found nothing to repair or
            // already stripped `attempt_req` proactively (an acting negative or
            // a peer probe). Runs after every request-shaping step so the
            // gray-artifact count reflects the exact carried bytes.
            let now_admit = Instant::now();
            let mut replay_plan = self.plan_replay_carry(target, &mut attempt_req, now_admit);
            let mut replay_repair_attempted = false;
            let mut replay_reject_status: u16 = 0;
            let mut skip_replay_backoff = false;

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
                if let Some((gate_kind, gate_err)) = self.gate_check(state_key, provider_name) {
                    tracing::warn!(
                        provider = provider_name,
                        model = %target.nickname.as_deref().unwrap_or(""),
                        gate_kind,
                        error = ?gate_err,
                        "gate blocked",
                    );
                    // Keep the FIRST real error: a synthetic gate error
                    // (status 0 "circuit breaker open" / RPM) on a later
                    // chain entry must not overwrite an earlier entry's
                    // genuine upstream failure, or the client sees the
                    // synthetic error instead of the real 503/timeout.
                    if last_err.is_none() {
                        last_err = Some(gate_err);
                    }
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue 'chain;
                }

                // Cancellation backstop (see ProbeSlotGuard): if the gate
                // admitted THIS dispatch as the half-open probe, free the slot
                // should the future be dropped before an outcome arm settles
                // it. Disarmed at each settle below; inert + a no-op otherwise.
                let mut probe_guard = self.probe_slot_guard(state_key);

                if attempts_made > 0 && !skip_replay_backoff {
                    let jittered = add_jitter(backoff, policy.jitter_ms);
                    tokio::time::sleep(jittered).await;
                    backoff = mul_duration(backoff, policy.backoff_multiplier);
                }
                skip_replay_backoff = false;

                let attempt_policy = self.compose_attempt_policy(
                    &policy,
                    provider_name,
                    target.stream_first_byte_timeout_ms,
                );
                // Fidelity WARN at the dispatch point: reads the per-target
                // clone about to go upstream, at most once per request.
                Self::warn_dropped_reasoning_dialect(
                    provider_name,
                    target.provider_kind,
                    &attempt_req,
                    &mut reasoning_drop_warned,
                );
                let result = run_with_timeout(
                    provider_name,
                    provider.as_ref(),
                    &attempt_req,
                    &attempt_policy,
                )
                .await;
                attempts_made += 1;
                meta.attempt_count += 1;

                match result {
                    Ok(mut resp) => {
                        self.record_success(state_key);
                        probe_guard.disarm();
                        // Stamp the served upstream provider name only
                        // when the served target opts in
                        // (`visible_routectl_provider`, default true).
                        // When suppressed, leave the field None so
                        // serde's skip_serializing_if drops it from the
                        // response. Internal accounting keys off
                        // `DispatchMeta.served_provider` / `served_upstream`,
                        // not this client-visible field, so suppression
                        // does not affect usage capture.
                        // Authoritative gate: every concrete provider
                        // pre-stamps `routectl_provider` with its own id
                        // before returning, so suppression MUST clear the
                        // field, not merely skip setting it -- otherwise the
                        // provider's value leaks through.
                        if target.visible_routectl_provider {
                            resp.routectl_provider = Some(provider_name.to_string());
                        } else {
                            resp.routectl_provider = None;
                        }
                        // Client-visible label: the serving target's
                        // `reported_model` override when set (and non-empty),
                        // else the client's requested alias. Internal
                        // accounting keys off `DispatchMeta`, not this field.
                        resp.model = resolve_reported_model(target, &req.model);
                        // A 2xx proves the capability is not rejected: clear
                        // any learned negative this dispatch re-probed, and
                        // ride each clear out on the meta so the ledger records
                        // it and a warm rebuild does not resurrect the negative.
                        meta.cleared_capabilities
                            .extend(learned_probe_guard.settle_success());
                        // Response-evidence observer: the success-arm mirror of
                        // `observe_for_learning`. Reads structural positive /
                        // suspected-absence evidence off the assembled response
                        // and admits it (read-only, post-response, no dispatch
                        // blocking). Self-gates on the kill switch. The
                        // streaming arm records nothing (no assembled response
                        // exists there -- fail closed).
                        // Settle the replay carry: a stripped repair that
                        // reached success confirms the negative (commit); a
                        // carried variant that succeeded outright proves the
                        // pair works, so clear any resident (lapsed) negative
                        // and ride each clear out on the meta so the ledger
                        // records it and a warm rebuild does not resurrect it.
                        if let Some(plan) = replay_plan.take() {
                            if replay_repair_attempted {
                                let features = derive_feature_keys(
                                    req.tools.as_deref().unwrap_or(&[]),
                                    req.provider_extras.as_ref(),
                                    req.response_format.as_ref(),
                                );
                                meta.learned_capabilities.extend(plan.commit(
                                    replay_reject_status,
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
                        self.observe_capabilities(&req, &resp, target, meta, Instant::now());
                        return Ok(resp);
                    }
                    Err(mut e) => {
                        let native_cf = match replay_plan.as_ref() {
                            Some(plan) => {
                                classify_with_attempt(&e, target.provider_kind, plan.attempt())
                            }
                            None => classify(&e, target.provider_kind),
                        };
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
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
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
                                model = %target.nickname.as_deref().unwrap_or(""),
                                attempt = attempts_made,
                                "upstream 401; refreshing auth and retrying once",
                            );
                            // A refresh failure means the OAuth identity is
                            // dead; surface it without walking the chain. But
                            // first release any half-open probe slot this
                            // attempt claimed via the gate, or the breaker
                            // stays locked open until restart.
                            if let Err(refresh_err) = provider.on_auth_failure().await {
                                self.release_probe_slot(state_key);
                                probe_guard.disarm();
                                return Err(refresh_err);
                            }
                            // Refresh succeeded. Release the half-open probe
                            // slot this attempt claimed at the gate BEFORE the
                            // `continue` re-enters the loop and re-runs
                            // `gate_check`. While this caller still holds the
                            // slot, the in-loop re-gate's `try_dispatch` sees
                            // `half_open_in_flight` and returns CircuitOpen,
                            // which would leave the breaker locked open until
                            // restart. Releasing here lets the re-gate claim a
                            // fresh slot (the per-attempt accounting the
                            // in-loop gate promises).
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            // Preserve the genuine upstream 401 as last_err
                            // before re-gating. If the re-gate then refuses
                            // (CircuitOpen / RPM), the `last_err.is_none()`
                            // guard above keeps this real error rather than
                            // overwriting it with the synthetic status-0 gate
                            // error, so the client sees the true 401.
                            last_err = Some(e);
                            continue;
                        }
                        // Reasoning-replay strip repair: a FIXED correctness
                        // branch, not a retry policy. When the optimistically
                        // carried variant drew the proven replay rejection,
                        // switch to the pre-stripped variant and re-dispatch
                        // this same target exactly ONCE with no backoff. Fires
                        // at most once per target and never nests inside the
                        // per-target retry or the fallback walk, so the call
                        // count stays additive; it never re-attempts the
                        // carried variant. The held guards settle at the
                        // success arm (commit) or on any later exit (release /
                        // drop, learning nothing).
                        if !replay_repair_attempted
                            && let Some(plan) = replay_plan.as_ref()
                            && Self::is_replay_rejection_class(&cf.class)
                        {
                            replay_repair_attempted = true;
                            replay_reject_status = upstream_facts(&e).status.unwrap_or(0);
                            let lane = plan.lane();
                            meta.replay_degradation = Some(ReplayDegradation {
                                action: REPLAY_ACTION_STRIP_REPAIR,
                                target_lane: lane,
                                state_key: sanitize_for_log(state_key),
                                source_schemes: plan.source_schemes().to_vec(),
                                reason: REPLAY_REASON_UPSTREAM_REJECTION,
                                artifact_count: plan.artifact_count(),
                                repair_attempted: true,
                                repair_succeeded: false,
                                learned: false,
                            });
                            strip_replay_artifacts(&mut attempt_req, lane);
                            skip_replay_backoff = true;
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            // Preserve the genuine replay-rejection error as
                            // last_err before re-gating the stripped variant.
                            // If the re-gate refuses (CircuitOpen / RPM), the
                            // `last_err.is_none()` guard keeps the real
                            // upstream rejection rather than surfacing the
                            // synthetic status-0 gate error. Store the body-free
                            // form so a re-gate refusal cannot surface the
                            // reasoning blob a replay rejection may echo.
                            last_err = Some(
                                replay_rejection_body_free(&e, &cf.class, provider_name)
                                    .unwrap_or(e),
                            );
                            continue;
                        }
                        if let Some(body_free) =
                            replay_rejection_body_free(&e, &cf.class, provider_name)
                        {
                            e = body_free;
                        }
                        let do_fallback = should_fallback(&e, &cf.class, &policy, is_probe);
                        // Probe fast-fail: a probe (max_tokens <=
                        // probe_max_tokens) that hit a rate-limit/overload
                        // (429/529) returns the status immediately via an
                        // explicit early return -- no retry, no fallback,
                        // no breaker failure debit. The early return below
                        // precedes the debit site, so a probe 429/529 never
                        // reaches health accounting at all. It does still
                        // release the half-open slot it may have claimed at
                        // the gate (see below).
                        let probe_fast_failed = if is_probe {
                            probe_fast_fail_status(&e)
                        } else {
                            None
                        };
                        if let Some(status) = probe_fast_failed {
                            log_probe_fast_fail(
                                provider_name,
                                target.nickname.as_deref().unwrap_or(""),
                                status,
                                req.max_tokens,
                            );
                            // Release the half-open slot this probe claimed
                            // at the gate: a 429/529 probe fast-fail is a
                            // transient upstream condition we deliberately do
                            // NOT count as a provider fault (that is why
                            // should_fallback is false here), so the slot
                            // must be freed without a breaker debit.
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
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
                                && next_is_sibling_seat(&chain, chain_idx, target)
                            {
                                last_err = Some(e);
                                continue 'chain;
                            }
                            return Err(e);
                        }
                        // The honored upstream reset for THIS error (clamped
                        // to the ceiling), computed once for both the park
                        // decision below and the in-loop sleep bump in the
                        // retry branch. `None` for every non-rate-limit error.
                        let reset_hint = rate_limit_reset_hint(&e, &policy);
                        // The breaker DEBIT keys off the failure CLASS, not
                        // the chain-hop decision: a seat looks unhealthy when
                        // it emits a transient-health failure (429 / 5xx /
                        // status-0 / overload), independent of whether the
                        // operator routes past that status. A non-debiting
                        // class (4xx caller error, capability, auth) that
                        // still falls back releases its slot without a debit
                        // in the fallback branch below.
                        let debits = class_debits(&cf.class);
                        // Whether THIS attempt force-opened (parked) the
                        // breaker. When it did, the next in-loop gate is
                        // guaranteed CircuitOpen, so a same-provider retry
                        // would be pure waste -- and is exactly the path that
                        // used to discard the genuine error and let the
                        // synthetic status-0 gate error surface in its place.
                        let mut breaker_parked = false;
                        let mut breaker_effect = "none";
                        if debits {
                            match reset_hint {
                                // Non-probe LARGE reset: park the provider for
                                // the honored duration (force_open) instead of
                                // a threshold-gated debit, so an exhausted seat
                                // is skipped until it actually resets. The
                                // in-loop re-gate then diverts to fallback /
                                // fail. Probes never park, and a small
                                // reset is honored as an in-loop sleep (below),
                                // so only the large non-probe case parks here.
                                Some(h) if !is_probe && h > INLOOP_RETRY_AFTER_CAP => {
                                    self.park_provider(state_key, h);
                                    breaker_parked = true;
                                    breaker_effect = "parked";
                                }
                                _ => {
                                    if self.record_failure_opened(
                                        state_key,
                                        LastOutcome::from_failure_class(&cf.class),
                                    ) {
                                        breaker_effect = "opened";
                                    }
                                }
                            }
                            probe_guard.disarm();
                        }
                        self.emit_class_observability(
                            &e,
                            &cf,
                            &original_class,
                            remapped,
                            remap_status,
                            DispatchSurface::Complete,
                            provider_name,
                            target,
                            do_fallback,
                            &policy,
                            debits,
                            is_probe,
                            is_forwarded,
                        );
                        self.observe_for_learning(
                            &e,
                            &cf,
                            remapped,
                            target,
                            is_forwarded,
                            &req,
                            &mut learn_dedupe,
                            meta,
                            &mut learned_probe_guard,
                        );
                        if opts.disable_fallbacks {
                            // Terminal error exit: free any half-open probe
                            // slot this attempt claimed. A no-op when a
                            // debiting class already routed through
                            // record_failure (which clears the slot).
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            return Err(e);
                        }
                        let can_retry_here = attempts_made < hard_cap
                            && should_retry_same_provider(
                                &e,
                                &cf.class,
                                &policy,
                                attempts_made,
                                is_probe,
                            )
                            && !breaker_parked;
                        let facts = upstream_facts(&e);
                        tracing::debug!(
                            provider = provider_name,
                            state_key = %state_key,
                            surface = DispatchSurface::Complete.as_str(),
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
                                model = %target.nickname.as_deref().unwrap_or(""),
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
                            // re-probing the SAME provider. Only when we were
                            // already going to retry here (can_retry_here is
                            // unchanged), only for a reset within the
                            // in-loop cap (a larger reset parked the provider
                            // above instead of blocking this thread), and never
                            // for a probe.
                            if let Some(h) = reset_hint
                                && !is_probe
                                && h <= INLOOP_RETRY_AFTER_CAP
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
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            continue;
                        }
                        // Done with this provider. Decide fallback vs propagate.
                        if do_fallback {
                            let has_next = chain_idx + 1 < chain_len;
                            if has_next {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %target.nickname.as_deref().unwrap_or(""),
                                    state_key = %state_key,
                                    error = ?e,
                                    "fallback to next",
                                );
                            } else {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %target.nickname.as_deref().unwrap_or(""),
                                    state_key = %state_key,
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
                                self.release_probe_slot(state_key);
                                probe_guard.disarm();
                            }
                            last_err = Some(e);
                            continue 'chain;
                        }
                        // Terminal non-fallbackable error. Free any half-open
                        // probe slot this attempt claimed so the breaker is
                        // not left locked open.
                        self.release_probe_slot(state_key);
                        probe_guard.disarm();
                        return Err(e);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
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
        let result = self.stream_inner(req, opts, &mut meta).await;
        emit_replay_degradation(&meta);
        DispatchedStream { meta, result }
    }

    /// Dispatch-loop body for `stream_with_options`. Mutates `meta` as
    /// the chain is walked. The served_* fields are captured at the
    /// `Ok(stream)` arm (the winning target is known synchronously,
    /// before any stream body is consumed).
    async fn stream_inner(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
        meta: &mut DispatchMeta,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let (chain, probe_admissions) = self.dispatch_chain_for_request(&req)?;
        // See `complete_inner`: re-probes the filter admitted, owned by a
        // request-scoped set that settles each on transfer (`take`) or on drop
        // (`OtherError` for any admission the loop never reached).
        let mut probe_set = ProbeAdmissionSet::new(
            self.learned_capabilities.clone(),
            probe_admissions,
            DispatchSurface::Stream.as_str(),
        );
        // See `complete_inner`: request-global observability dimension
        // ONLY, fed to `emit_class_observability`. The per-target
        // `use_forwarded_credential` flag drives the model-rewrite
        // bypass and the 401/403/429 terminal re-key.
        let is_forwarded = req.routectl_internal.forwarded_bearer.is_some();
        let chain_len = chain.len();
        let policy = self.policy_for(&req.model);
        // Availability-probe detection (see `complete_with_options`). A
        // streaming probe that 429/529s fast-fails the chain too.
        let is_probe = is_probe_request(&req, &policy);
        // Auto-cache decision inputs: computed ONCE off the ORIGINAL req
        // (see `complete_inner`). Reused across retries and fallback
        // targets so the decision never drifts.
        let auto_cache_plan =
            AutoCacheRequestPlan::build(&req, self.config.cache.auto_emit_top_level_breakpoint);
        if auto_cache_plan.has_caller_breakpoints {
            self.warn_volatile_in_caller_prefix(&req);
        }
        let mut last_err: Option<Error> = None;
        // One learned-capability observation per request per
        // (state_key, feature): the error arm fires per attempt, so this
        // set stops a same-request retry from manufacturing a second
        // observation. See `observe_for_learning`.
        let mut learn_dedupe: HashSet<LearnDedupeKey> = HashSet::new();
        // One reasoning-drop fidelity WARN per client request -- see
        // `complete_inner`. Same stack-local once-flag over the dispatch-point
        // emit, so the streaming path is not a second site that repeats it.
        let mut reasoning_drop_warned = false;

        'chain: for (chain_idx, target) in chain.iter().enumerate() {
            let provider_name = target.provider_name.as_str();
            let state_key = target.state_key.as_str();
            let model = target.upstream.as_str();
            // See `complete_inner`: learned re-probe settle guard, scoped to
            // the whole chain-iteration; holds every admission the target owns
            // and drops -> OtherError on any exit. The move takes ownership out
            // of the set so the set's drop cannot settle it again.
            let mut learned_probe_guard = match probe_set.take(state_key) {
                Some(admissions) => LearnedProbeGuard::armed(
                    self.learned_capabilities.clone(),
                    admissions,
                    probe_set.surface,
                ),
                None => LearnedProbeGuard::inert(),
            };
            let Some(provider) = target.provider.clone() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            };
            // This entry is being dispatched: record it as the terminal
            // target (overwritten by a later entry on fallback) and count
            // the hop -- see `complete_inner` for the `chain_idx` rationale.
            meta.mark_target(target, &req.model);
            meta.fallback_count = chain_idx as u32;

            // Missing-bearer terminal guard: see `complete_inner`.
            if let Some(err) = missing_forwarded_bearer_error(target, &req) {
                return Err(err);
            }

            let mut attempt_req = req.clone();
            // See `complete_inner`: a forwarded target keeps the client's
            // requested model verbatim instead of rewriting to `upstream`.
            if !target.use_forwarded_credential {
                attempt_req.model = model.to_string();
            }
            apply_layered_overlays(&self.config, target, &mut attempt_req);
            // INTERCEPTOR HOOK (see `complete_inner`): strip runs after
            // layered config compose, before auto-cache / context
            // reduction. Strict refusal returns the 400; a rolled-back
            // hazard routes away for this attempt.
            match self.apply_strip_interceptor(target, &mut attempt_req) {
                StripDecision::Proceed => {}
                StripDecision::StrictReject(err) => return Err(err),
                StripDecision::RouteAway(err) => {
                    last_err = Some(err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue;
                }
            }
            let provider_cfg = self.config.providers.get(provider_name);
            // Context reduction: see `complete_inner`. Runs after overlays
            // and before auto-cache so the auto-emitted breakpoint covers the
            // reduced bytes. Effective only when global on AND provider not
            // explicitly off. `apply_json_minify` fails closed.
            let reduction_effective = self.config.reduction.enabled
                && provider_cfg.and_then(crate::config::ProviderEntry::reduction_enabled)
                    != Some(false);
            let reduction_outcome = if reduction_effective {
                Some(apply_json_minify(&mut attempt_req))
            } else {
                None
            };
            let reduction_token =
                reduction_strategy_token(reduction_effective, reduction_outcome.as_ref());
            meta.reduction_strategy = Some(reduction_token);
            if let Some(ReductionOutcome::Applied(delta)) = &reduction_outcome {
                tracing::debug!(
                    provider = %provider_name,
                    model = %model,
                    strategy = reduction_token,
                    strings_minified = delta.strings_minified,
                    bytes_saved = delta.bytes_saved,
                    est_tokens_saved = delta.est_tokens_saved,
                    "context_reduction",
                );
            }
            // Auto-cache: see `complete_inner`. Same once-vs-per-attempt
            // split; injected on this clone after overlays, before the
            // inner loop. Original `req` is never mutated.
            let cache_injection = maybe_apply_auto_cache_control(
                &mut attempt_req,
                &auto_cache_plan,
                provider_cfg.map(crate::config::ProviderEntry::cache_capability),
                provider_cfg
                    .and_then(crate::config::ProviderEntry::auto_emit_top_level_breakpoint)
                    .unwrap_or(true),
            );
            // Cache observability: see `complete_inner`. Stamp the decision
            // token and emit the per-request decision at debug. No bodies /
            // secrets: only provider name, model id, strategy token.
            let strategy_token = cache_injection.strategy_str();
            meta.cache_strategy = Some(strategy_token);
            tracing::debug!(
                provider = %provider_name,
                model = %model,
                strategy = strategy_token,
                "cache_auto_decision",
            );

            // Steady-state would-trim advisory: see `complete_inner`. The same
            // NON-MUTATING shared helper runs on the streaming path so the two
            // dispatch sites never diverge. Never touches `attempt_req`.
            self.record_would_trim(
                &attempt_req,
                target.provider_kind,
                model,
                target.nickname.as_deref().unwrap_or(model),
                &target.model.effective_row,
                meta,
            );

            // Reasoning-replay carry admission (see `complete_inner`): claim
            // the single-flight slot for each non-portable carried scheme, or
            // strip proactively when an acting negative / peer probe refuses.
            let now_admit = Instant::now();
            let mut replay_plan = self.plan_replay_carry(target, &mut attempt_req, now_admit);
            let mut replay_repair_attempted = false;
            let mut replay_reject_status: u16 = 0;
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
                // ordinary errors, so the only second iteration is the
                // auth-recovery retry; the gate runs once per attempt, so
                // the first attempt is debited exactly once.
                if let Some((gate_kind, gate_err)) = self.gate_check(state_key, provider_name) {
                    tracing::warn!(
                        provider = provider_name,
                        model = %target.nickname.as_deref().unwrap_or(""),
                        gate_kind,
                        error = ?gate_err,
                        "stream gate blocked",
                    );
                    // Keep the FIRST real error: a synthetic gate error
                    // (status 0 "circuit breaker open" / RPM) on a later
                    // chain entry must not overwrite an earlier entry's
                    // genuine upstream failure, or the client sees the
                    // synthetic error instead of the real 503/timeout.
                    if last_err.is_none() {
                        last_err = Some(gate_err);
                    }
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue 'chain;
                }

                // Gate granted Allow. Capture NOW whether this dispatch
                // claimed the half-open probe slot: only a probe's first
                // CONTENT chunk should close + release the breaker at the Ok
                // arm below. Reading the flag at first-content time instead
                // would race a concurrent dispatch.
                let was_half_open_probe = self.is_half_open_probe(state_key);
                // Cancellation backstop (see ProbeSlotGuard): free the
                // half-open probe slot if this future is dropped before an
                // outcome arm settles it (e.g. consumer disconnect during the
                // pre-content wait against a hung upstream). Re-reads the same
                // flag as `was_half_open_probe` above; both reads are
                // consistent under the single-probe invariant.
                let mut probe_guard = self.probe_slot_guard(state_key);

                // Fidelity WARN at the dispatch point -- see `complete_inner`.
                Self::warn_dropped_reasoning_dialect(
                    provider_name,
                    target.provider_kind,
                    &attempt_req,
                    &mut reasoning_drop_warned,
                );
                let r = try_stream_with_first_content(
                    provider_name,
                    model,
                    provider.clone(),
                    attempt_req.clone(),
                    &attempt_policy,
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
                        // mid-stream failure recorded by the wrap below is
                        // counted as a normal failure accumulating toward
                        // `circuit_failures` -- a probe that delivered
                        // content then errors does NOT get a special immediate
                        // re-trip. With circuit_failures = 1 a single
                        // post-close mid-stream error re-quarantines at once
                        // (fast-flap); with >= 2 a still-degraded upstream
                        // may serve up to that many content-then-error
                        // streams before re-opening -- the throughput-vs-
                        // quarantine tradeoff of closing on first content
                        // (see runtime_state.rs).
                        let state = self.state.get(state_key).cloned();
                        if was_half_open_probe && let Some(st) = state.as_ref() {
                            st.lock().record_success(Instant::now());
                        }
                        // The probe (if any) is settled; the wrapped stream's
                        // BreakerAccounting owns the tail. Disarm so a drop here
                        // does not free a slot a later probe may hold.
                        probe_guard.disarm();
                        // First content proves the capability is not rejected:
                        // clear any learned negative this dispatch re-probed,
                        // and ride each clear out on the meta so the ledger
                        // records it and a warm rebuild does not resurrect it.
                        meta.cleared_capabilities
                            .extend(learned_probe_guard.settle_success());
                        // No response-evidence observation on the stream path:
                        // no assembled response exists here to read structural
                        // evidence from, so positive detection fails closed. A
                        // later stream assembler is purely additive.
                        // Stamp the client-visible label on every Ok chunk
                        // (including the terminal / usage-only chunk) before
                        // the breaker wrap. The closure owns the label String
                        // (moved in via `move`) to satisfy the 'static
                        // BoxStream bound; `chunk.model` needs an owned String,
                        // so the label is cloned per Ok chunk. `Err` passes
                        // through byte-for-byte unchanged.
                        let label = resolve_reported_model(target, &req.model);
                        let relabeled = stream.map(move |item| match item {
                            Ok(mut chunk) => {
                                chunk.model = label.clone();
                                Ok(chunk)
                            }
                            Err(e) => Err(e),
                        });
                        // Settle the replay carry (see `complete_inner`): a
                        // stripped repair reaching first content confirms the
                        // negative (commit); carried content proves the
                        // pair works, so clear any resident (lapsed) negative
                        // and ride each clear out on the meta.
                        if let Some(plan) = replay_plan.take() {
                            if replay_repair_attempted {
                                let features = derive_feature_keys(
                                    req.tools.as_deref().unwrap_or(&[]),
                                    req.provider_extras.as_ref(),
                                    req.response_format.as_ref(),
                                );
                                meta.learned_capabilities.extend(plan.commit(
                                    replay_reject_status,
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
                        return Ok(wrap_with_breaker_accounting(
                            relabeled.boxed(),
                            state,
                            target.provider_kind,
                        ));
                    }
                    Err(mut e) => {
                        let native_cf = match replay_plan.as_ref() {
                            Some(plan) => {
                                classify_with_attempt(&e, target.provider_kind, plan.attempt())
                            }
                            None => classify(&e, target.provider_kind),
                        };
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
                        // `complete_inner` for the full rationale). This
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
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
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
                                model = %target.nickname.as_deref().unwrap_or(""),
                                attempt = attempts_made,
                                "stream 401 pre-content; refreshing auth and retrying once",
                            );
                            if let Err(refresh_err) = provider.on_auth_failure().await {
                                self.release_probe_slot(state_key);
                                probe_guard.disarm();
                                return Err(refresh_err);
                            }
                            // Refresh succeeded. Release the half-open probe
                            // slot this attempt claimed at the gate BEFORE the
                            // `continue` re-enters the loop and re-runs
                            // `gate_check`. While this caller still holds the
                            // slot, the in-loop re-gate's `try_dispatch` sees
                            // `half_open_in_flight` and returns CircuitOpen,
                            // which would leave the breaker locked open until
                            // restart. Releasing here lets the re-gate claim a
                            // fresh slot.
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            // Preserve the genuine upstream 401 as last_err
                            // before re-gating (see `complete_inner`): if the
                            // re-gate refuses, the `last_err.is_none()` guard
                            // keeps the real 401 over the synthetic gate error.
                            last_err = Some(e);
                            continue;
                        }
                        // Reasoning-replay strip repair (see `complete_inner`):
                        // a FIXED correctness branch. On the proven replay
                        // rejection, switch to the pre-stripped variant and
                        // re-dispatch this target exactly ONCE. Streams take no
                        // in-loop backoff, so the retry is immediate; it never
                        // nests across the fallback walk and never re-attempts
                        // the carried variant.
                        if !replay_repair_attempted
                            && let Some(plan) = replay_plan.as_ref()
                            && Self::is_replay_rejection_class(&cf.class)
                        {
                            replay_repair_attempted = true;
                            replay_reject_status = upstream_facts(&e).status.unwrap_or(0);
                            let lane = plan.lane();
                            meta.replay_degradation = Some(ReplayDegradation {
                                action: REPLAY_ACTION_STRIP_REPAIR,
                                target_lane: lane,
                                state_key: sanitize_for_log(state_key),
                                source_schemes: plan.source_schemes().to_vec(),
                                reason: REPLAY_REASON_UPSTREAM_REJECTION,
                                artifact_count: plan.artifact_count(),
                                repair_attempted: true,
                                repair_succeeded: false,
                                learned: false,
                            });
                            strip_replay_artifacts(&mut attempt_req, lane);
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            // Preserve the genuine replay-rejection error as
                            // last_err before re-gating the stripped variant
                            // (see `complete_inner`): if the re-gate refuses,
                            // the `last_err.is_none()` guard keeps the real
                            // rejection over the synthetic gate error. Store the
                            // body-free form so a re-gate refusal cannot surface
                            // the reasoning blob a replay rejection may echo.
                            last_err = Some(
                                replay_rejection_body_free(&e, &cf.class, provider_name)
                                    .unwrap_or(e),
                            );
                            continue;
                        }
                        if let Some(body_free) =
                            replay_rejection_body_free(&e, &cf.class, provider_name)
                        {
                            e = body_free;
                        }
                        let do_fallback = should_fallback(&e, &cf.class, &policy, is_probe);
                        // Probe fast-fail: a probe that hit a rate-limit/
                        // overload (429/529) returns the status immediately
                        // -- no fallback, no breaker failure debit. The early
                        // return below precedes the debit site, so a probe
                        // 429/529 never reaches health accounting. It does
                        // release the half-open slot it may have claimed at
                        // the gate (see below). Streams never retry the same
                        // provider, so there is no can_retry_here to guard.
                        let probe_fast_failed = if is_probe {
                            probe_fast_fail_status(&e)
                        } else {
                            None
                        };
                        if let Some(status) = probe_fast_failed {
                            log_probe_fast_fail(
                                provider_name,
                                target.nickname.as_deref().unwrap_or(""),
                                status,
                                req.max_tokens,
                            );
                            // Release the half-open slot this probe claimed
                            // at the gate: a 429/529 probe fast-fail is a
                            // transient upstream condition we deliberately do
                            // NOT count as a provider fault, so free the slot
                            // without a breaker debit.
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            // Fail over to a sibling seat of the SAME pool
                            // before fast-failing (see `complete_inner`): a
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
                        // Stream dispatch never retries the same provider (no
                        // in-loop sleep), so a reset hint only sizes the
                        // breaker park. A non-probe reset parks the provider
                        // for the honored, clamped duration; a probe never
                        // parks and a no-hint error keeps the
                        // threshold-gated debit.
                        let reset_hint = rate_limit_reset_hint(&e, &policy);
                        // The DEBIT keys off the failure CLASS, not the
                        // chain-hop decision (see `complete_inner`): a seat is
                        // unhealthy when it emits a transient-health failure,
                        // independent of routing. A non-debiting class that
                        // still falls back releases its slot without a debit
                        // in the fallback branch below.
                        let debits = class_debits(&cf.class);
                        if debits {
                            match reset_hint {
                                Some(h) if !is_probe => self.park_provider(state_key, h),
                                _ => self.record_failure(
                                    state_key,
                                    LastOutcome::from_failure_class(&cf.class),
                                ),
                            }
                            probe_guard.disarm();
                        }
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
                        );
                        self.observe_for_learning(
                            &e,
                            &cf,
                            remapped,
                            target,
                            is_forwarded,
                            &req,
                            &mut learn_dedupe,
                            meta,
                            &mut learned_probe_guard,
                        );
                        if opts.disable_fallbacks {
                            // Terminal error exit: free any half-open probe
                            // slot this attempt claimed. A no-op when a
                            // debiting class already routed through
                            // record_failure (which clears the slot).
                            self.release_probe_slot(state_key);
                            probe_guard.disarm();
                            return Err(e);
                        }
                        if do_fallback {
                            let has_next = chain_idx + 1 < chain_len;
                            if has_next {
                                tracing::warn!(
                                    provider = provider_name,
                                    model = %target.nickname.as_deref().unwrap_or(""),
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
                                    model = %target.nickname.as_deref().unwrap_or(""),
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
                                self.release_probe_slot(state_key);
                                probe_guard.disarm();
                            }
                            last_err = Some(e);
                            continue 'chain;
                        }
                        // Terminal non-fallbackable error. Free any half-open
                        // probe slot this attempt claimed so the breaker is
                        // not left locked open.
                        self.release_probe_slot(state_key);
                        probe_guard.disarm();
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
    ) {
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
    /// `params.trigger_tokens`. Below the trigger, records nothing at all
    /// (every `would_trim_*` field stays `None`) and returns early. At or
    /// above it, runs TWO independent measurements against the same
    /// resolved `(provider_kind, model)` pricing row:
    ///
    /// 1. The shipped size-baseline plan (`propose_steady_state_trim`):
    ///    freed-token count `d`, break-even K*, `k_floor`, and the shadow
    ///    misfire monitor. UNCHANGED by this task -- still driven entirely
    ///    by the `propose` path, and still records nothing when `propose`
    ///    finds no cut (its own columns stay `None`).
    /// 2. The near-lossless pass (`record_near_lossless_marks`): dedup /
    ///    supersession attribution, path-extractability counts, the
    ///    recorder-version marker, the raw-marks blob, and the
    ///    context-fraction advisory. Runs INDEPENDENTLY of whether (1)
    ///    found a cut, so near-lossless opportunity is measured even where
    ///    the size baseline declines.
    ///
    /// CRITICAL: this NEVER mutates `attempt_req`. Both measurements only
    /// read the request and never call `apply_trim_plan`, so the bytes sent
    /// upstream are identical with or without this call. It is extracted
    /// into ONE helper -- invoked from both `complete_inner` and
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
    /// `inbound_session_key` and the SAME pricing row's `ttl_seconds`
    /// (threaded through for provenance / reserved for future
    /// age-conditioning; the per-turn hazard model does not split on it).
    /// The estimate's `k_floor` is stamped onto `meta.would_trim_k_floor` only
    /// for a `Calibrated` confidence (the only bound the cost gate may
    /// consult to authorize a cut). The met/unmet/cold/unpriced verdict is
    /// DERIVED downstream from the numeric advisory columns
    /// (`would_trim_tokens`, `would_trim_break_even_k`, `would_trim_k_floor`);
    /// this function never overwrites `meta.reduction_strategy`, which
    /// remains owned by the reduction path. Advisory only -- the
    /// dispatched bytes never change.
    ///
    /// The two model dimensions are DELIBERATELY split. `model` is the
    /// UPSTREAM wire id and keys the pricing lookup -- pricing cells are baked
    /// per upstream model. `served_model` is the served NICKNAME and keys the
    /// K estimator's [`crate::k_estimator::KQuery`], because
    /// [`Router::record_k_sample`] writes each per-session K window under the
    /// served nickname (`meta.served_model` == `target.nickname`). Keying the
    /// query on `served_model` (not `model`) is what makes the query triple
    /// byte-identical to the sample-write triple; keying it on the upstream id
    /// silently misses the store, holds every estimate at `Cold`, and leaves
    /// `would_trim_k_floor` permanently `None`. When a target has no nickname
    /// (`served_model` falls back to `model`), no sample is ever recorded for
    /// that dispatch either, so the fallback never mis-keys a populated window.
    fn record_would_trim(
        &self,
        attempt_req: &ChatRequest,
        provider_kind: Option<&'static str>,
        model: &str,
        served_model: &str,
        effective: &EffectiveRow,
        meta: &mut DispatchMeta,
    ) {
        let params = self.config.trim.to_params();
        if estimate_total_tokens(attempt_req) <= params.trigger_tokens {
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

            // Consult the K estimator over the SAME row whose TTL priced K*: the
            // TTL is threaded through for provenance only -- the per-turn
            // hazard model does not split on it -- so a `k_floor` is
            // comparable to `break_even`. The current sample for THIS turn
            // is recorded post-response in `record_k_sample`, so the
            // estimator reads PRIOR-turn samples only. The query keys on
            // `served_model` (the nickname), NOT the upstream `model`, so the
            // query triple matches the triple `record_k_sample` writes under.
            // A `Disabled` / `Missing` cell has no row to read `ttl_seconds`
            // from; the sentinel's TTL is the same conservative default used
            // everywhere else a trusted row is unavailable.
            let ttl_seconds = row.map_or(CatalogRow::sentinel().ttl_seconds, |r| r.ttl_seconds);
            let estimate = self.k_estimator.estimate(&crate::k_estimator::KQuery {
                session_key: attempt_req.routectl_internal.inbound_session_key.as_deref(),
                provider_kind: provider_kind.unwrap_or(""),
                model: served_model,
                ttl: Duration::from_secs(u64::from(ttl_seconds)),
                now: SystemTime::now(),
            });

            meta.would_trim_k_floor = would_trim_k_floor_for_meta(break_even, &estimate);

            // Shadow misfire monitor: recording only, never mutates attempt_req.
            // Compute a fingerprint of the trimmed cacheable prefix and compare it
            // against the stored value for this (session, provider_kind, model) triple.
            // A Misfire means the prefix shifted turn-to-turn -- the canary that a
            // real cut would break the upstream cache.
            if let Some(session_key) = attempt_req.routectl_internal.inbound_session_key.as_deref()
            {
                let fp = trimmed_prefix_fingerprint(attempt_req, &plan);
                let shadow_key = crate::k_estimator::KSessionKey {
                    session_key: session_key.to_string(),
                    provider_kind: provider_kind.unwrap_or("").to_string(),
                    model: model.to_string(),
                };
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
                        // triple across lines -- so the key rides as a stable
                        // FNV-1a hash (toolchain-stable, unlike DefaultHasher).
                        tracing::warn!(
                            session_key_hash = crate::context_trim::fnv1a_hash(session_key.as_bytes()),
                            provider_kind = provider_kind.unwrap_or(""),
                            model = %model,
                            "would_trim_shadow_misfire: trimmed cacheable prefix shifted turn-to-turn",
                        );
                    }
                    crate::k_estimator::ShadowOutcome::FirstSeen => {}
                }
            }
        }

        record_near_lossless_marks(attempt_req, &params, row, meta);
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
    if break_even.is_some() && estimate.confidence == crate::k_estimator::Confidence::Calibrated {
        Some(estimate.k_floor)
    } else {
        None
    }
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
/// | `ReductionOutcome::NothingToStrip` | `skipped:nothing-to-strip`|
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
        Some(ReductionOutcome::NothingToStrip) => "skipped:nothing-to-strip",
        // `ReductionOutcome` is `#[non_exhaustive]`; a future variant we do
        // not yet map means reduction RAN but produced an outcome this build
        // does not recognize -- which is distinct from "disabled", so record
        // it as its own token. `None` only happens when `effective == false`,
        // already handled above.
        _ => "skipped:unknown",
    }
}

/// Decide-and-maybe-inject a single top-level `cache_control` ephemeral_5m
/// breakpoint on the PER-ATTEMPT clone. Mutates ONLY `attempt_req`, never
/// the original request. Never returns `Err` and never panics: any doubt
/// degrades to "dispatch the un-injected clone".
///
/// clone -> set -> validate -> keep-or-rollback: the only mutation is a
/// single assignment to `attempt_req.cache_control`, and it is reverted if
/// `validate_source` rejects the injected shape. Called once per chain
/// entry, AFTER `apply_layered_overlays` (so injection is the last
/// dispatch-time touch of `cache_control`) and before the inner retry
/// loop, so all retries on a target reuse byte-identical bytes.
///
/// `capability` is `None` when the target's provider is absent from the
/// table -> fail closed (no injection). Cheap field checks run before the
/// validate call so the common skip paths never allocate.
///
/// `caller_supplied` is evaluated FIRST as a request-level fact: a request
/// that already carries caller breakpoints is independent of which target /
/// provider is selected, so it takes precedence over every per-target /
/// config `auto_skipped:*` reason (global / provider kill-switch, capability).
fn maybe_apply_auto_cache_control(
    attempt_req: &mut ChatRequest,
    plan: &AutoCacheRequestPlan,
    capability: Option<CacheCapability>,
    provider_auto_emit_enabled: bool,
) -> CacheInjection {
    if plan.has_caller_breakpoints {
        return CacheInjection::SkippedCallerSupplied;
    }
    if !plan.global_auto_emit_enabled {
        return CacheInjection::SkippedGlobalDisabled;
    }
    if !provider_auto_emit_enabled {
        return CacheInjection::SkippedProviderDisabled;
    }
    match capability {
        Some(c) if c.supports_top_level_cache_control => {}
        _ => return CacheInjection::SkippedNoCapability,
    }
    if plan.volatile_high_veto {
        return CacheInjection::SkippedVolatileHigh;
    }
    // Defensive drift guard: no-caller implies 0 today, so +1 is always
    // within MAX. Kept so a future change that injects alongside caller
    // markers cannot silently exceed the cap.
    if plan.caller_breakpoint_count.saturating_add(1) > MAX_BREAKPOINTS {
        return CacheInjection::SkippedBreakpointCap;
    }

    // clone -> set -> validate -> keep-or-rollback, local to this clone.
    let original = attempt_req.cache_control.clone();
    attempt_req.cache_control = Some(CacheControl::ephemeral_5m());
    if validate_source(attempt_req).is_ok() {
        CacheInjection::Emitted
    } else {
        attempt_req.cache_control = original;
        CacheInjection::ValidationRolledBack
    }
}

/// Resolve the client-visible response `model` label for a served
/// dispatch target. Returns the target's `reported_model` override
/// when set to a non-empty string, otherwise the client's requested
/// alias (`req_model`). An empty override string is treated as unset.
/// Computed once per request so a fallback chain / multi-chunk stream
/// carries one stable label.
fn resolve_reported_model(target: &DispatchTarget, req_model: &str) -> String {
    match target.reported_model.as_deref() {
        Some(label) if !label.is_empty() => label.to_string(),
        _ => req_model.to_string(),
    }
}

/// Execute one upstream `complete()` call with the policy's per-request
/// timeout (if configured). Timeout expiry surfaces as a status-0
/// upstream error, treated as a network class for retry/fallback. The
/// error reports `provider_name` (the config-key the chain walks) so
/// it lines up with `routectl_provider` in responses and with
/// gate-error sources -- never the kind-prefixed `provider.id()`.
async fn run_with_timeout(
    provider_name: &str,
    provider: &dyn Provider,
    req: &ChatRequest,
    policy: &RetryPolicy,
) -> Result<ChatResponse> {
    match policy.request_timeout_ms {
        Some(ms) => {
            match tokio::time::timeout(Duration::from_millis(ms), provider.complete(req.clone()))
                .await
            {
                Ok(r) => r,
                Err(_) => Err(Error::upstream(
                    provider_name,
                    0,
                    format!("request timed out after {ms}ms"),
                )),
            }
        }
        None => provider.complete(req.clone()).await,
    }
}

/// Wrap an upstream stream so the breaker records success on clean
/// completion (None / EOS) and records ONE failure on the first error
/// that bubbles out of the stream. Subsequent errors do not double-count.
///
/// For a half-open PROBE the call site already closed the breaker on
/// first content, so a mid-stream failure here re-trips it and a
/// clean completion / consumer cancellation is a benign re-zeroing of the
/// just-closed breaker. For a HEALTHY (closed) breaker the call site did
/// NOT touch the breaker, so this wrap is where mid-stream failures
/// accumulate toward the threshold (and a clean completion resets the
/// counter). Consumer cancellation before any error is treated as success
/// in both cases.
fn wrap_with_breaker_accounting(
    inner: BoxStream<'static, Result<ChatChunk>>,
    state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
    provider_kind: Option<&'static str>,
) -> BoxStream<'static, Result<ChatChunk>> {
    use futures::stream::StreamExt as _;
    struct BreakerAccounting {
        state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
        settled: bool,
    }

    impl BreakerAccounting {
        const fn new(state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>) -> Self {
            Self {
                state,
                settled: false,
            }
        }

        fn with_state(&self, f: impl FnOnce(&mut crate::runtime_state::ProviderState)) {
            let Some(st) = &self.state else {
                return;
            };
            f(&mut st.lock());
        }

        fn record_success(&mut self) {
            if self.settled {
                return;
            }
            self.settled = true;
            self.with_state(|state| state.record_success(Instant::now()));
        }

        fn record_failure(&mut self, outcome: LastOutcome) {
            if self.settled {
                return;
            }
            self.settled = true;
            self.with_state(|state| {
                state.record_failure(Instant::now(), outcome);
            });
        }
    }

    impl Drop for BreakerAccounting {
        fn drop(&mut self) {
            // Clean completion OR consumer cancellation before any error:
            // both record success. For a probe stream the breaker is
            // already closed (call-site first-content close), so this is a
            // no-op re-zeroing; for a healthy-breaker stream this resets
            // any accumulated failure count on a fully-consumed success.
            if !self.settled {
                self.record_success();
            }
        }
    }

    let mut accounting = BreakerAccounting::new(state);
    let s = async_stream::stream! {
        let mut inner = inner;
        while let Some(item) = inner.next().await {
            if let Err(e) = &item {
                accounting.record_failure(LastOutcome::from_failure_class(
                    &classify(e, provider_kind).class,
                ));
            }
            yield item;
        }
        accounting.record_success();
    };
    Box::pin(s)
}

/// Upper bound on the content-free leading chunks buffered while waiting
/// for the first content-bearing one. A healthy stream opens with at most
/// a handful of metadata chunks (a role delta, an id/model stamp); a stream
/// that exceeds this before emitting content is treated as a pre-content
/// failure rather than buffering unboundedly.
const MAX_PRECONTENT_CHUNKS: usize = 8;

/// True when `chunk` carries client-visible generated content: non-empty
/// text or reasoning text, typed reasoning blocks, tool-call requests, or
/// opaque SSE blocks. Content-free metadata -- a leading role delta,
/// id/model/upstream_meta stamps, a usage-only tail, a bare finish_reason,
/// or empty choices -- returns false: those are not the content-commit
/// boundary and may still be followed by a fallback to a sibling provider.
///
/// Opaque carriers count as content: they are client-visible unknown block
/// data that cannot be mixed with a different provider's output, so once one
/// arrives the provider is committed exactly as a text chunk commits it.
fn is_content_bearing(chunk: &ChatChunk) -> bool {
    if !chunk.opaque_events.is_empty() {
        return true;
    }
    chunk.choices.iter().any(|choice| {
        let delta = &choice.delta;
        delta.content.as_deref().is_some_and(|t| !t.is_empty())
            || delta.reasoning.as_deref().is_some_and(|t| !t.is_empty())
            || !delta.reasoning_details.is_empty()
            || delta
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
    })
}

/// Open the upstream stream and pull chunks until the first CONTENT-bearing
/// one (see `is_content_bearing`). The hard non-retry boundary is first
/// content, not stream-open: leading content-free chunks (a `delta.role`
/// opener, id/model metadata) are buffered, so a fallbackable error, EOS, or
/// buffer overflow in the [stream-open, first-content] window still walks to
/// the next provider. If a content chunk arrives, return a `BoxStream` that
/// yields the buffered metadata (in order, upstream_meta preserved), then the
/// first content chunk, then the rest of the upstream stream -- mid-stream
/// errors propagate.
///
/// The buffer is never exposed to the client before content commits: the API
/// cannot both surface those chunks and re-enter the outer fallback loop.
///
/// `policy.stream_first_byte_timeout_ms` (when set) caps the wait for
/// stream-open PLUS the entire pre-content pull loop, so leading content-free
/// chunks neither reset nor satisfy it -- a role-then-hang still trips the
/// first-content timeout and falls over. Expiry surfaces as a status-0
/// upstream error which is fallbackable per `should_fallback`.
///
/// Also emits a debug-level first-activity log the moment the upstream
/// response headers arrive, ahead of the pre-content pull below (see the
/// `attempt_start` comment inside).
async fn try_stream_with_first_content(
    provider_name: &str,
    upstream_model: &str,
    provider: Arc<dyn Provider>,
    req: ChatRequest,
    policy: &RetryPolicy,
) -> Result<BoxStream<'static, Result<ChatChunk>>> {
    // Per-attempt clock for the first-activity mark below -- NOT the
    // request-level clock `UsageCapture::start` uses for `ttfb_ms`
    // (that lives in a higher crate this one cannot see). For the
    // common case (no prior fallback hop on this request) the two are
    // within noise of each other, so `elapsed_ms` here is directly
    // comparable to the ledger's `ttfb_ms` to derive the first-activity
    // -> first-content gap. A request that fell back through one
    // or more dead chain entries first will show extra unaccounted time
    // on the ttfb side that this attempt's clock does not carry -- that
    // is chain-walk overhead, a separate concern from the prefill gap
    // this instrumentation targets.
    let attempt_start = Instant::now();
    let open_and_first = async {
        let mut upstream = provider.stream(req).await?;
        // First sign of upstream life: response headers arrived (every
        // egress's `stream()` awaits `client.execute()` -- which
        // resolves once the status line + headers are in -- BEFORE
        // constructing the lazy body-byte stream below). Distinct from
        // the first-CONTENT mark (`mark_first_byte` in
        // `ingress_handle.rs`), which additionally waits out any
        // upstream `message_start`/`ping` events the SSE parser
        // swallows. One site here covers every provider (Anthropic,
        // Bedrock, gemini, openai-compat, openai-responses) since they
        // all share this `stream()` -> execute -> byte-stream shape.
        // Debug-only; no bodies/prompts/PII, structured fields only.
        // Manual capture recipe: docs/LOGGING.md, "Stream first-activity
        // mark" -- run with ROUTECTL_LOG=routectl_router=debug and issue
        // a streaming request; this line's `elapsed_ms` is the gap
        // between upstream headers and the existing first-content
        // ttfb_ms mark.
        tracing::debug!(
            provider = provider_name,
            upstream = upstream_model,
            elapsed_ms = attempt_start.elapsed().as_millis() as u64,
            "stream first-activity: upstream response headers received",
        );
        // Buffer content-free leading chunks (role opener, id/model
        // metadata) until the first content-bearing one. Bounded: a
        // stream that never produces content must not buffer forever.
        let mut buffered: Vec<ChatChunk> = Vec::new();
        loop {
            match upstream.next().await {
                Some(Ok(chunk)) if is_content_bearing(&chunk) => {
                    // Commit: buffered metadata (order + upstream_meta
                    // preserved) -> first content -> upstream tail.
                    let head = std::mem::take(&mut buffered);
                    let merged = futures::stream::iter(head.into_iter().map(Ok))
                        .chain(futures::stream::once(async move { Ok(chunk) }))
                        .chain(upstream);
                    return Ok(merged.boxed());
                }
                Some(Ok(chunk)) => {
                    if buffered.len() >= MAX_PRECONTENT_CHUNKS {
                        // Buffer overflow before any content: a
                        // pre-content failure. Discard the buffer (nothing
                        // reached the client) and fall over. Fallbackable
                        // per `should_fallback`; the breaker records it.
                        return Err(Error::Streaming(format!(
                            "{provider_name} emitted more than {MAX_PRECONTENT_CHUNKS} \
                             content-free chunks before any content",
                        )));
                    }
                    buffered.push(chunk);
                }
                Some(Err(e)) => return Err(e),
                // Upstream ended before any content (empty stream, or only
                // content-free chunks). This is NOT a successful empty
                // completion -- a healthy provider always emits content
                // (even a role opener is followed by content or an error).
                // Discard the buffer and treat as a fallbackable streaming
                // error so the chain walks to the next provider AND the
                // breaker records a failed probe. Without this, an upstream
                // that closes before producing content would be reported as
                // a successful completion to both the client and the
                // router's health accounting.
                None => {
                    return Err(Error::Streaming(format!(
                        "{provider_name} stream closed before any content arrived",
                    )));
                }
            }
        }
    };

    match policy.stream_first_byte_timeout_ms {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), open_and_first).await {
            Ok(r) => r,
            Err(_) => Err(Error::upstream(
                provider_name,
                0,
                format!(
                    "stream first-content timeout after {ms}ms; content-free leading \
                     chunks (role opener, id/model metadata) do not satisfy the deadline"
                ),
            )),
        },
        None => open_and_first.await,
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
const fn probe_fast_fail_status(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream {
            status: s @ (429 | 529),
            ..
        } => Some(*s),
        _ => None,
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
    ))
}

/// The honored rate-limit reset for `err`, clamped to the configured
/// ceiling, or `None` when `err` carries no reset hint. A reset hint is
/// present ONLY on a rate-limit/overload (429/503/529) where the
/// upstream told us when it resets (`Retry-After`, or the Codex
/// `usage_limit_reached` `resets_at` / `resets_in_seconds`), so its
/// presence is itself the rate-limit signal. The clamp bounds both the
/// in-loop honored sleep and the breaker park to
/// `RetryPolicy::max_honored_retry_after` so a hostile or buggy upstream
/// cannot pin a provider open indefinitely. A zero-duration hint (e.g.
/// `Retry-After: 0`, a past HTTP-date, or a saturated `resets_at`) yields
/// `None` so it falls through to the normal threshold-gated debit instead
/// of a degenerate zero-length park (a `force_open(.., ZERO)` would leave
/// the breaker stuck half-open without ever tripping).
pub(super) fn rate_limit_reset_hint(err: &Error, policy: &RetryPolicy) -> Option<Duration> {
    match err {
        Error::Upstream {
            retry_after: Some(d),
            ..
        } => Some((*d).min(policy.max_honored_retry_after())).filter(|d| !d.is_zero()),
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

/// The same-provider retry cap for `class` under `policy` -- the value the
/// retry branch compares `attempts_made` against. Delegates to
/// [`RetryPolicy::resolved_class`], which layers any operator per-class
/// `[retry.classes]` override on top of the baked class default. Shared by
/// [`should_retry_same_provider`] and the class-decision observability so
/// the logged cap never drifts from the cap actually enforced.
pub(super) fn retry_cap_for(class: &FailureClass, policy: &RetryPolicy) -> u32 {
    policy.resolved_class(class).0
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

/// The upstream HTTP status carried by `err`, for the per-provider class
/// remap lookup ONLY: `Some` for an [`Error::Upstream`] status in
/// `400..=599`, `None` for status 0 and every non-upstream variant. This
/// seam consults the target's own `class_overrides`, never `policy`.
pub(super) fn upstream_status_for_remap(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream { status, .. } if (400..=599).contains(status) => Some(*status),
        _ => None,
    }
}

/// Apply the operator's per-provider status remap on top of the
/// classifier's native decision. A `status` that keys into `overrides`
/// replaces the class with the operator's override, keeping the NATIVE
/// `matched_by` -- the remap changes WHICH class was decided, not HOW the
/// classifier itself matched. No status, or no matching key (including
/// the empty-map default), returns `native` unchanged. Returns the
/// effective `ClassifiedFailure` plus whether a remap fired.
pub(super) fn apply_remap(
    native: ClassifiedFailure,
    status: Option<u16>,
    overrides: &BTreeMap<u16, FailureClass>,
) -> (ClassifiedFailure, bool) {
    match status.and_then(|s| overrides.get(&s)) {
        Some(class) => (
            ClassifiedFailure {
                class: class.clone(),
                matched_by: native.matched_by,
            },
            true,
        ),
        None => (native, false),
    }
}

/// Whether a failure of this class debits the per-seat circuit breaker's
/// health accounting. True ONLY for the fixed transient-health set --
/// conditions a fallback or a cooldown can recover from; false for
/// caller-shaped or capability faults that retrying the same seat would
/// never fix, and (fail-closed) for any class the classifier gains later.
///
/// Deliberately independent of the fallback/retry decision: routing
/// (whether to advance the chain) and health accounting (whether the seat
/// looks unhealthy) are separate concerns.
pub const fn class_debits(class: &FailureClass) -> bool {
    matches!(
        class,
        FailureClass::RateLimited
            | FailureClass::ServerError
            | FailureClass::Timeout
            | FailureClass::NetworkError
            | FailureClass::Overloaded
    )
}

pub(super) fn should_fallback(
    err: &Error,
    class: &FailureClass,
    policy: &RetryPolicy,
    is_probe: bool,
) -> bool {
    // Availability-probe fast-fail: a probe (max_tokens <=
    // probe_max_tokens) that hits a rate-limit (429) or overload (529)
    // does not fall back. Every OTHER error class -- generic 5xx,
    // network/status-0, Streaming, and every 4xx including the
    // Bedrock-style max_tokens=1 400 -- falls through to the normal
    // predicate below, so real fallback is untouched.
    if is_probe && probe_fast_fail_status(err).is_some() {
        return false;
    }
    // An unknown provider id in the chain is a config-shaped fault the
    // caller routes past: always fallbackable, independent of class.
    if let Error::UnknownProvider(_) = err {
        return true;
    }
    policy.resolved_class(class).1
}

pub(super) fn should_retry_same_provider(
    err: &Error,
    class: &FailureClass,
    policy: &RetryPolicy,
    attempts_made: u32,
    is_probe: bool,
) -> bool {
    // Probe fast-fail mirrors `should_fallback`: a probe must not burn
    // retry attempts against a rate-limited/overloaded provider (429 /
    // 529). All other error classes fall through to the cap below.
    if is_probe && probe_fast_fail_status(err).is_some() {
        return false;
    }
    let cap = retry_cap_for(class, policy);
    attempts_made < cap
}

fn add_jitter(base: Duration, jitter_ms: u64) -> Duration {
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

fn mul_duration(d: Duration, factor: f64) -> Duration {
    let nanos = d.as_nanos() as f64 * factor;
    Duration::from_nanos(nanos.min(u64::MAX as f64) as u64)
}

/// Whether the chain entry AFTER `chain_idx` is another seat of the SAME
/// pooled model as `target` -- i.e. a sibling seat a probe may fail over to
/// before fast-failing. All seats of one pool share the model `Arc`
/// (`dispatch_target_for_seat` clones the same `Arc<ResolvedModel>`), while
/// distinct chain entries carry distinct model Arcs, so pointer identity is
/// the exact same-pool discriminant.
fn next_is_sibling_seat(
    chain: &[DispatchTarget],
    chain_idx: usize,
    target: &DispatchTarget,
) -> bool {
    chain
        .get(chain_idx + 1)
        .is_some_and(|next| std::sync::Arc::ptr_eq(&next.model, &target.model))
}

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
#[path = "context_reduction_dispatch_tests.rs"]
mod context_reduction_dispatch_tests;

#[cfg(test)]
#[path = "k_query_key_tests.rs"]
mod k_query_key_tests;

#[cfg(test)]
#[path = "shadow_misfire_log_tests.rs"]
mod shadow_misfire_log_tests;

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
#[path = "replay_degradation_observability_tests.rs"]
mod replay_degradation_observability_tests;

#[cfg(test)]
#[path = "reasoning_drop_warn_tests.rs"]
mod reasoning_drop_warn_tests;
