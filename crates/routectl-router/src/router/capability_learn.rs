//! Learned-capability observation, expiry, and snapshot.

use std::collections::HashSet;
use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::failure_class::ClassifiedFailure;
use routectl_core::{ChatRequest, Error};

use super::{DispatchMeta, DispatchTarget, LearnedProbeGuard, Router};
use crate::capability_matcher::resolve_requested_capability;

/// The native AWS Bedrock provider `kind` string; the only kind whose flat
/// `ValidationException` envelope the drift observer inspects.
const BEDROCK_PROVIDER_KIND: &str = "bedrock";

/// Per-request dedupe key for the learn path. The capability arm dedupes on
/// `(state_key, feature_key)`; the Bedrock-validation drift signal dedupes on
/// `state_key` alone. Distinct enum variants keep the two namespaces disjoint
/// by TYPE rather than by a whitespace-bearing sentinel string, so a drift
/// key can never collide with a token-shaped capability key regardless of
/// what an upstream names.
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
        let Some((feature_key, tier)) = resolve_requested_capability(provider_kind, err, cf) else {
            self.observe_bedrock_validation_drift(provider_kind, err, target, dedupe);
            return;
        };
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
            dedupe.insert(LearnDedupeKey::Capability {
                state_key,
                feature_key,
            });
            return;
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
            // PLACEHOLDER: the F2 resolver arm rewires this to the resolved
            // phase; every negative minted here is F1 until then.
            FailurePhase::F1,
            Instant::now(),
        );
        let acting = matches!(outcome, crate::learned_capability::ObserveOutcome::Acting);
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
                upstream_code = upstream_code.as_deref().unwrap_or(""),
                upstream_param = %param,
                signal_tier = tier.as_str(),
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
                upstream_code = upstream_code.as_deref().unwrap_or(""),
                signal_tier = tier.as_str(),
                observations,
                acting,
                "learned-capability negative observed",
            ),
        }

        if acting {
            self.metrics.incr_learned_negatives();
            meta.learned_capabilities.push(CapabilityLearnEvent {
                state_key,
                capability_key: feature_key,
                provider_kind: provider_kind.to_string(),
                signal_tier: tier,
                observations,
                upstream_status,
                remapped,
                request_features,
                phase: FailurePhase::F1,
                source: EvidenceSource::Live,
            });
        }
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
}

#[cfg(test)]
#[path = "learn_capture_tests.rs"]
mod learn_capture_tests;
