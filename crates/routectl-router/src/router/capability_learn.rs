//! Learned-capability observation, expiry, and snapshot.

use std::collections::HashSet;
use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::failure_class::{ClassifiedFailure, FailureClass};
use routectl_core::{ChatRequest, Error};

use super::{DispatchMeta, DispatchTarget, LearnedProbeGuard, Router};
use crate::capability_matcher::resolve_requested_capability;

/// The native AWS Bedrock provider `kind` string; the only kind whose flat
/// `ValidationException` envelope the drift observer inspects.
const BEDROCK_PROVIDER_KIND: &str = "bedrock";

/// Per-request dedupe key for the learn path. The capability arm dedupes on
/// `(state_key, feature_key)`; the drift signals dedupe on `state_key` alone;
/// the F1-seen marker keys on `feature_key` alone (cross-lane -- it records
/// that ANY lane in this attempt chain already minted an F1 negative for that
/// capability). Distinct enum variants keep the namespaces disjoint by TYPE
/// rather than by a whitespace-bearing sentinel string, so no key can collide
/// with a token-shaped capability key regardless of what an upstream names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum LearnDedupeKey {
    /// One capability observation per `(target, capability)` per request.
    Capability {
        /// Breaker state key of the rejecting target.
        state_key: String,
        /// Normalized capability key the rejection named.
        feature_key: String,
    },
    /// One Bedrock validation-drift signal per target per request.
    BedrockDrift {
        /// Breaker state key of the rejecting target.
        state_key: String,
    },
    /// One feature-naming drift signal per target per request.
    FeatureNamingDrift {
        /// Breaker state key of the rejecting target.
        state_key: String,
    },
    /// Marks that an F1 negative for this capability was minted earlier in
    /// this attempt chain. Keys on `feature_key` ALONE (cross-lane): a later
    /// F2 candidate for the same capability -- on any lane -- is suppressed
    /// rather than blind-minted. Riding the existing dedupe set threads the
    /// cross-lane signal through both dispatch arms with no extra parameter.
    F1Seen {
        /// Normalized capability key the F1 negative named.
        feature_key: String,
    },
    /// Dedupes the same-chain-F1 F2-suppression WARN + counter. Keys on
    /// `feature_key` ALONE (cross-lane): the suppression is a per-chain signal,
    /// so N demoted lanes surface exactly one WARN + one counter bump per
    /// request regardless of how many lanes rejected the capability.
    F2Suppressed {
        /// Normalized capability key the suppressed F2 candidate named.
        feature_key: String,
    },
}

/// A single learned-capability observation captured on a dispatch error
/// arm, riding out on [`DispatchMeta`] to the usage-capture layer. The
/// router does not depend on the ledger writer, so learn events travel
/// on the dispatch meta rather than being written here.
///
/// `capability_key` is already normalized (via `normalize_capability_key`),
/// so the writer and any future warm-rebuild replayer key off identical
/// strings. `remapped` is always `false` by construction (the capture
/// gate rejects a config-remapped class); it is carried so a replay can
/// filter defensively. No request body, prompt, or upstream message text
/// ever enters this struct -- only the classifier's structured facts.
#[derive(Debug, Clone)]
pub struct CapabilityLearnEvent {
    /// Breaker state key (nickname-or-provider) of the rejecting target.
    pub state_key: String,
    /// Normalized capability key the rejection named.
    pub capability_key: String,
    /// Stable provider-kind token of the rejecting target.
    pub provider_kind: String,
    /// Whether the evidence was self-identifying or inferred.
    pub signal_tier: SignalTier,
    /// Observation count on the entry after this observation.
    pub observations: u32,
    /// The upstream request-fault status (400 or 422) that carried the
    /// rejection.
    pub upstream_status: u16,
    /// Always `false` here (a remapped class never reaches capture);
    /// persisted for defensive replay filtering.
    pub remapped: bool,
    /// The request's derived feature set at capture time. Replay verifies
    /// the learned capability was actually in flight.
    pub request_features: Vec<String>,
    /// The detection phase that attributed this negative. In-memory
    /// ride-along only -- no `capability_learn_events` column.
    pub phase: FailurePhase,
    /// Whether the evidence came from live traffic or an out-of-band probe.
    /// Fixed to `Live` in this milestone.
    pub source: EvidenceSource,
}

impl Router {
    /// After a config-only carry-over, lapse into a single re-probe every
    /// learned negative whose EFFECTIVE operator override verdict changed
    /// across the reload -- a `force_supported` mask (or any override cell)
    /// added, removed, or flipped for that `(target, capability)`. The
    /// operator's intent for the cell moved, so the resident learned verdict
    /// is re-verified against live upstream behavior rather than trusted; the
    /// entry is expired (decay clock reset), NOT dropped, so its observation
    /// history and backoff survive. Entries whose override resolution is
    /// unchanged ride across intact -- this never clears the whole registry
    /// (that stays keyed to catalog / overlay changes).
    pub(super) fn expire_learned_on_override_change(&self, previous: &Self) {
        let now = Instant::now();
        for entry in self.learned_capabilities.snapshot() {
            let (provider_name, nickname) = self.override_identity_for(&entry.state_key);
            let provider_kind = self
                .config
                .providers
                .get(&provider_name)
                .map_or("", |p| p.kind_str());
            let before = previous
                .override_registry
                .resolve(&provider_name, &nickname, &entry.feature_key, provider_kind)
                .map(|(verdict, _)| verdict);
            let after = self
                .override_registry
                .resolve(&provider_name, &nickname, &entry.feature_key, provider_kind)
                .map(|(verdict, _)| verdict);
            if before != after {
                self.learned_capabilities.expire_keyed(
                    &entry.state_key,
                    &entry.feature_key,
                    provider_kind,
                    now,
                );
                tracing::debug!(
                    state_key = %entry.state_key,
                    capability_key = %entry.feature_key,
                    "override cell changed across reload; lapsed learned negative into a re-probe",
                );
            }
        }
    }

    /// Map a learned-registry `state_key` to the `(provider_name, nickname)`
    /// pair the override registry resolves against. A per-model target keys
    /// by nickname; a pooled seat keys by `nickname#label` (recover the base
    /// model); a legacy / direct-construction target keys by the provider
    /// name itself (no model scope). Enables comparing a learned entry's
    /// effective override verdict across a reload.
    pub(super) fn override_identity_for(&self, state_key: &str) -> (String, String) {
        if let Some(model) = self.resolved_models.get(state_key) {
            return (model.provider_name.clone(), state_key.to_string());
        }
        if let Some((base, _label)) = state_key.split_once('#')
            && let Some(model) = self.resolved_models.get(base)
        {
            return (model.provider_name.clone(), base.to_string());
        }
        (state_key.to_string(), String::new())
    }

    /// Read-only snapshot of the learned-capability registry: every resident
    /// per-(target, feature) negative in the fixed contract shape. `&self`
    /// delegate over the private `learned_capabilities` registry so the
    /// status surface can surface learned negatives without reaching into
    /// the field.
    pub fn learned_capability_snapshot(
        &self,
    ) -> Vec<crate::learned_capability::LearnedRegistryEntry> {
        self.learned_capabilities.snapshot()
    }

    /// Learn-path capture, called from both dispatch error arms beside
    /// [`Router::emit_class_observability`]. On an eligible, deduped
    /// capability rejection it records a learned negative in the registry,
    /// emits a structured WARN, and (once the entry is acting) rides a
    /// [`CapabilityLearnEvent`] out on `meta`.
    ///
    /// Every gate short-circuits, so a common (non-capability) failure
    /// pays only the cheap early checks. The full eligibility gate (all
    /// must hold): the kill switch is on; the upstream fault is a request
    /// fault (400/422); the class was not operator-remapped; the request
    /// did not carry a forwarded bearer; the resolver attributes the fault
    /// to a canonical capability; that capability is not the
    /// operator-remap provenance token; and the capability is a member of
    /// the request's derived feature set (`derive_feature_keys`). The
    /// resolver keys on the request-capability namespace, so this final
    /// membership check learns a negative ONLY for a capability the request
    /// actually carried -- a misbehaving upstream naming an off-request
    /// param never plants a routing entry.
    ///
    /// `dedupe` carries one [`LearnDedupeKey`] per deduped signal for the
    /// life of a single request: the error arm fires per attempt, so a
    /// same-request retry (or a per-target re-entry) must never manufacture
    /// a second observation and falsely confirm an inferred signal.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn observe_for_learning(
        &self,
        err: &Error,
        cf: &ClassifiedFailure,
        remapped: bool,
        target: &DispatchTarget,
        is_forwarded: bool,
        req: &ChatRequest,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
        probe_guard: &mut LearnedProbeGuard,
    ) {
        if !self.config.capability.enabled {
            return;
        }
        let Error::Upstream {
            status: status @ (400 | 422),
            upstream_code,
            ..
        } = err
        else {
            return;
        };
        let upstream_status = *status;
        if remapped || is_forwarded {
            return;
        }
        let Some(provider_kind) = target.provider_kind else {
            return;
        };
        let Some(resolved) = resolve_requested_capability(provider_kind, err, cf) else {
            self.observe_bedrock_validation_drift(provider_kind, err, target, dedupe);
            self.observe_feature_naming_drift(provider_kind, cf, target, req, dedupe);
            return;
        };
        self.commit_learned_observation(
            resolved,
            &cf.class,
            err,
            upstream_status,
            upstream_code.as_deref(),
            provider_kind,
            target,
            req,
            remapped,
            dedupe,
            meta,
            probe_guard,
        );
    }

    /// Given a resolved `(capability, tier, phase)` for an eligible upstream
    /// request fault, apply the remaining mint gates and -- when they all hold
    /// -- record the learned negative, emit the structured WARN, and ride a
    /// [`CapabilityLearnEvent`] out on `meta`.
    ///
    /// Beyond the request-membership, mask, probe-settle, and per-request
    /// dedupe gates shared with the F1 wire-token path, an F2 feature-naming
    /// candidate mints ONLY when both hold: the evidence is self-identifying of
    /// a deterministic request fault (an inferred or transient-derived F2 never
    /// mints -- [`f2_evidence_is_mintable`]), and no F1 negative for the same
    /// capability was already observed earlier in this attempt chain (a
    /// cross-lane fallback must not blind-mint an F2 after an F1 strip on a
    /// sibling lane; the reverse ordering self-heals -- no deferred-commit
    /// state machine). Every F1 mint records an [`LearnDedupeKey::F1Seen`]
    /// marker so a later same-chain F2 candidate is suppressed with a dedicated
    /// WARN + counter.
    ///
    /// Split from [`Router::observe_for_learning`] so the mint pipeline can be
    /// driven with a provisional F2 resolution in tests -- the production F2
    /// tables ship empty, so the real resolver never returns F2 on live input.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_learned_observation(
        &self,
        resolved: (String, SignalTier, FailurePhase),
        class: &FailureClass,
        err: &Error,
        upstream_status: u16,
        upstream_code: Option<&str>,
        provider_kind: &'static str,
        target: &DispatchTarget,
        req: &ChatRequest,
        remapped: bool,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
        probe_guard: &mut LearnedProbeGuard,
    ) {
        let (feature_key, tier, phase) = resolved;
        if feature_key == crate::class_policy::OPERATOR_REMAP_CAPABILITY {
            return;
        }
        // Request-membership gate: learn a negative ONLY for a capability the
        // request actually carried. `request_features` is the act-side lookup
        // vocabulary (`derive_feature_keys` output); the resolver now emits
        // canonical act-side keys, so a genuine rejection is a member by
        // construction -- `response_format` -> `structured_output` for a
        // request whose `output_config.format` was set, and a tool-type
        // passthrough for a request that carried that tool type. A resolved
        // key the request never sent (a poisoned or spurious upstream param,
        // or a capability with no act-side derivation -- an inferred `prefill`,
        // a paramless geo-block token) fails the check and never learns, so a
        // misbehaving upstream cannot plant a routing entry the act side could
        // never look up. This is the gate the original cross-namespace check
        // meant to be: correct now that both sides meet on identical keys.
        let request_features = crate::feature_keys::derive_feature_keys(
            req.tools.as_deref().unwrap_or(&[]),
            req.provider_extras.as_ref(),
        );
        if !request_features.contains(&feature_key) {
            return;
        }
        let state_key = target.state_key.clone();
        // MASK: an operator `force_supported` override for this (target,
        // feature) masks the learned negative. The act side already
        // short-circuited both the routing verdict AND the probe admission
        // for a masked cell, so it never claims a re-probe slot; here the
        // learn is suppressed too -- a masked-cell rejection never refreshes
        // or increments the resident entry (its `expires_at` is untouched, so
        // wall-clock decay continues). Upstream still rejected the capability
        // the operator forced on, so surface the contradiction exactly once
        // per request (deduped) with a dedicated counter. Capability TOKEN and
        // state_key only -- never a request body.
        if self.override_forces_supported(target, &feature_key, provider_kind) {
            if dedupe.insert(LearnDedupeKey::Capability {
                state_key: state_key.clone(),
                feature_key: feature_key.clone(),
            }) {
                self.metrics.incr_mask_suppressed();
                tracing::warn!(
                    event = "suppression",
                    state_key = %state_key,
                    capability_key = %feature_key,
                    "force_supported override contradicted: masked capability still rejected upstream",
                );
            }
            return;
        }
        // If this target was admitted as the single re-probe for this same
        // capability, the rejection SETTLES the probe (capped backoff owns the
        // observation bump and expiry) instead of feeding the observe path.
        // The dedupe key is inserted too, so a same-request retry that hits
        // this arm again does not re-observe the entry the probe refreshed.
        if probe_guard.settle_same_capability(&state_key, &feature_key, provider_kind) {
            self.metrics.incr_probe_failures();
            // A re-probe that reconfirms an F1 negative is F1 evidence for this
            // capability earlier in this attempt chain (criterion (c) reads
            // "no F1 seen", not "no F1 freshly minted"): record F1Seen so a
            // later cross-lane F2 candidate is suppressed rather than
            // blind-minted past the reconfirmed F1. Phase-conditional -- a
            // reconfirmed F2 must NOT set it, or a sibling lane's own F2 would
            // be wrongly suppressed.
            if self.settled_negative_phase(&state_key, &feature_key) == Some(FailurePhase::F1) {
                dedupe.insert(LearnDedupeKey::F1Seen {
                    feature_key: feature_key.clone(),
                });
            }
            dedupe.insert(LearnDedupeKey::Capability {
                state_key,
                feature_key,
            });
            return;
        }
        // F2 mint gates. A feature-naming negative is minted only on
        // self-identifying evidence of a deterministic request fault, and never
        // when an ACTING F1 negative for this same capability was already
        // observed earlier in this attempt chain -- otherwise a later cross-lane
        // 400 could blind-mint an F2 for a capability an F1 strip on a sibling
        // lane already handled. The suppression WARN dedupes on the capability
        // alone (the signal is per-chain, not per-lane), so N demoted lanes
        // surface exactly one WARN + counter bump per request.
        if phase == FailurePhase::F2 {
            if !f2_evidence_is_mintable(tier, class) {
                return;
            }
            if dedupe.contains(&LearnDedupeKey::F1Seen {
                feature_key: feature_key.clone(),
            }) {
                if dedupe.insert(LearnDedupeKey::F2Suppressed {
                    feature_key: feature_key.clone(),
                }) {
                    self.metrics.incr_f2_same_chain_suppressed();
                    tracing::warn!(
                        event = "suppression",
                        state_key = %state_key,
                        capability_key = %feature_key,
                        phase = FailurePhase::F2.as_str(),
                        "f2 feature-naming negative suppressed: same-chain f1 already observed for this capability",
                    );
                }
                return;
            }
        }
        // One observation per request per (state_key, feature): a retry or
        // per-target re-entry that hits this arm again is dropped here.
        if !dedupe.insert(LearnDedupeKey::Capability {
            state_key: state_key.clone(),
            feature_key: feature_key.clone(),
        }) {
            return;
        }

        let outcome = self.learned_capabilities.observe(
            &state_key,
            &feature_key,
            provider_kind,
            tier,
            phase,
            EvidenceSource::Live,
            Instant::now(),
        );
        let acting = matches!(outcome, crate::learned_capability::ObserveOutcome::Acting);
        // An F1 negative records the cross-lane marker ONLY once it ACTS: a
        // self-identifying F1 acts on its first observation, an inferred F1 only
        // once corroborated. A still-pending inferred F1 must not suppress a
        // later same-chain self-identifying F2 -- weak evidence must never mask
        // strong. This mirrors the probe-settle path, which treats a
        // reconfirmed RESIDENT (already-acting) F1 as F1-seen.
        if acting && phase == FailurePhase::F1 {
            dedupe.insert(LearnDedupeKey::F1Seen {
                feature_key: feature_key.clone(),
            });
        }
        let observations = self
            .learned_capabilities
            .snapshot()
            .into_iter()
            .find(|entry| entry.state_key == state_key && entry.feature_key == feature_key)
            .map_or(0, |entry| entry.observations);

        let upstream_param = crate::capability_matcher::upstream_param(err);
        // Emit `upstream_param` ONLY when the sanitizer deemed it safe to log
        // verbatim (bounded, single-token, no whitespace/control bytes). An
        // adversarial or buggy upstream can put arbitrary text in `error.param`;
        // dropping the field entirely -- rather than logging a blank or the raw
        // string -- keeps injected content out of the operator log while the
        // closed-set `capability_key` and `upstream_code` still record.
        match upstream_param.as_deref() {
            Some(param) => tracing::warn!(
                event = "learn",
                state_key = %state_key,
                capability_key = %feature_key,
                provider_kind,
                upstream_status,
                upstream_code = upstream_code.unwrap_or(""),
                upstream_param = %param,
                signal_tier = tier.as_str(),
                phase = phase.as_str(),
                observations,
                acting,
                "learned-capability negative observed",
            ),
            None => tracing::warn!(
                event = "learn",
                state_key = %state_key,
                capability_key = %feature_key,
                provider_kind,
                upstream_status,
                upstream_code = upstream_code.unwrap_or(""),
                signal_tier = tier.as_str(),
                phase = phase.as_str(),
                observations,
                acting,
                "learned-capability negative observed",
            ),
        }

        if acting {
            self.metrics.incr_learned_negatives(phase);
            meta.learned_capabilities.push(CapabilityLearnEvent {
                state_key,
                capability_key: feature_key,
                provider_kind: provider_kind.to_string(),
                signal_tier: tier,
                observations,
                upstream_status,
                remapped,
                request_features,
                phase,
                source: EvidenceSource::Live,
            });
        }
    }

    /// The detection phase of the resident learned negative for `(state_key,
    /// feature_key)`, or `None` when no entry resides. Read at the probe-settle
    /// site to decide whether a reconfirmed negative is F1 evidence that must
    /// suppress a later cross-lane F2 candidate in the same attempt chain.
    fn settled_negative_phase(&self, state_key: &str, feature_key: &str) -> Option<FailurePhase> {
        self.learned_capabilities
            .snapshot()
            .into_iter()
            .find(|entry| entry.state_key == state_key && entry.feature_key == feature_key)
            .map(|entry| entry.phase)
    }

    /// Drift observability for the Bedrock validation matcher. When the
    /// shared resolver attributed no capability yet the rejection IS a flat
    /// Bedrock `ValidationException`, the anchored-template table missed a
    /// real 400: emit a structured WARN and bump a dedicated counter so
    /// wording drift is visible instead of silently reintroducing repeat
    /// rejections. Deduped to once per request per target; only a
    /// capability-token-free signal (state_key + provider_kind) reaches the
    /// log -- never a request body or the upstream message text.
    fn observe_bedrock_validation_drift(
        &self,
        provider_kind: &str,
        err: &Error,
        target: &DispatchTarget,
        dedupe: &mut HashSet<LearnDedupeKey>,
    ) {
        if provider_kind != BEDROCK_PROVIDER_KIND {
            return;
        }
        if !crate::capability_matcher::is_bedrock_validation_exception(err) {
            return;
        }
        if !dedupe.insert(LearnDedupeKey::BedrockDrift {
            state_key: target.state_key.clone(),
        }) {
            return;
        }
        self.metrics.incr_bedrock_validation_unmatched();
        tracing::warn!(
            event = "bedrock_validation_unmatched",
            state_key = %target.state_key,
            provider_kind,
            "bedrock validation rejection matched no capability template",
        );
    }

    /// Drift observability for the F2 feature-naming matcher. When the shared
    /// resolver attributed no capability yet the rejection is a deterministic
    /// request fault on a feature-carrying request against a provider that HAS
    /// a feature-naming table, the shipped-empty template table missed a real
    /// rejection shape: emit a structured WARN and bump a dedicated counter so
    /// wording drift is visible instead of silently dropping the signal --
    /// exactly the discipline the Bedrock-validation drift observer applies to
    /// the wire-token table. Gated to providers that carry an F2 table so it
    /// never fires on every unresolved rejection on every provider. Deduped to
    /// once per request per target; only a capability-token-free signal
    /// (state_key + provider_kind) reaches the log -- never a request body,
    /// prompt, or the upstream message text.
    fn observe_feature_naming_drift(
        &self,
        provider_kind: &str,
        cf: &ClassifiedFailure,
        target: &DispatchTarget,
        req: &ChatRequest,
        dedupe: &mut HashSet<LearnDedupeKey>,
    ) {
        if !crate::capability_matcher::has_feature_naming_table(provider_kind) {
            return;
        }
        if !f2_class_is_deterministic(&cf.class) {
            return;
        }
        let request_features = crate::feature_keys::derive_feature_keys(
            req.tools.as_deref().unwrap_or(&[]),
            req.provider_extras.as_ref(),
        );
        if request_features.is_empty() {
            return;
        }
        if !dedupe.insert(LearnDedupeKey::FeatureNamingDrift {
            state_key: target.state_key.clone(),
        }) {
            return;
        }
        self.metrics.incr_feature_naming_unmatched();
        tracing::warn!(
            event = "feature_naming_unmatched",
            state_key = %target.state_key,
            provider_kind,
            "deterministic feature-carrying rejection matched no feature-naming template",
        );
    }
}

/// True when a resolved F2 feature-naming candidate is eligible to mint a
/// learned negative on its evidence alone: self-identifying tier (an inferred
/// F2 never mints) of a deterministic request-fault class. This is the
/// F2-specific half of the mint gate; the request-membership, mask,
/// probe-settle, and same-chain-F1 gates are applied separately at the mint
/// site.
pub(super) fn f2_evidence_is_mintable(tier: SignalTier, class: &FailureClass) -> bool {
    tier == SignalTier::SelfIdentifying && f2_class_is_deterministic(class)
}

/// True when `class` is a deterministic request fault an F2 feature-naming
/// negative may be minted from: `BadRequest` or `FeatureUnsupported`. Every
/// transient or server-side class -- anything a config class-override could
/// derive a request fault from without the upstream self-reporting a feature
/// rejection -- returns `false`, so a remapped transient can never plant an F2
/// negative. A new `#[non_exhaustive]` `FailureClass` variant defaults to
/// rejected (the safe side) until it is explicitly admitted here.
pub(super) const fn f2_class_is_deterministic(class: &FailureClass) -> bool {
    matches!(
        class,
        FailureClass::BadRequest | FailureClass::FeatureUnsupported { .. }
    )
}

#[cfg(test)]
#[path = "learn_capture_tests.rs"]
mod learn_capture_tests;
