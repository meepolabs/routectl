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

/// One resident learned entry whose truth is independent of the catalog
/// revision, in the shape a persisted restatement needs.
///
/// Produced by [`Router::catalog_independent_survivors`] when a reload moves
/// the replay boundary: each survivor must be re-appended past the new
/// boundary or the next boot cannot see it (the ledger read starts at the
/// newest tombstone). Every field is carried VERBATIM from the resident
/// entry -- a restatement re-states an existing fact, so refreshing its
/// evidence or its decay age would silently extend a verdict's life every
/// time an operator reloads config.
///
/// Plain owned strings: the consumer builds a leaf-crate ledger row that
/// depends on none of this crate's types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogIndependentSurvivor {
    /// Breaker state key (nickname-or-provider) the entry applies to.
    pub state_key: String,
    /// Normalized capability key.
    pub capability: String,
    /// Persisted verdict token.
    pub verdict: String,
    /// Persisted phase token.
    pub phase: String,
    /// Persisted evidence-source token.
    pub source: String,
    /// Persisted signal-tier token.
    pub tier: String,
    /// How many observations the entry had accrued.
    ///
    /// Load-bearing for an INFERRED negative: it acts only once corroborated
    /// (two observations), so restating one row would replay a corroborated entry
    /// as a single pending observation -- resident but NOT acting, silently
    /// downgrading a verdict that was routing traffic. A self-identifying entry
    /// acts on one observation and needs no second row.
    pub observations: u32,
    /// The pinned observation-evidence token, when this verdict carries one.
    ///
    /// Load-bearing, not forensic: the warm rebuild fails closed on a
    /// `verified` / `suspect` row whose class is absent or unrecognized, so a
    /// restatement that dropped it would be SKIPPED at the next boot and the
    /// verdict would be evicted -- the exact outcome the restatement exists to
    /// prevent. `None` for a `broken` verdict, which carries none.
    pub evidence_class: Option<String>,
    /// Provider-kind token, resolved through the one shared resolver
    /// (`Router::provider_kind_for_state_key`).
    pub provider_kind: String,
    /// When the entry was first observed. Carried so a restatement preserves
    /// the original observation time.
    pub first_seen: Instant,
    /// When the entry was most recently observed -- the age a restatement
    /// must preserve rather than reset.
    pub last_seen: Instant,
}

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
    /// The EFFECTIVE persistence generation this event must be stamped with.
    ///
    /// Taken from the registry operation that produced the event, atomically
    /// under the same guard as its read or mutation -- never sampled before or
    /// after. A separate read could be taken across a boundary and stamp the
    /// event with a generation that does not describe the state it reports. A
    /// single request legitimately spans a boundary, so events on one request
    /// may carry DIFFERENT generations.
    pub persistence_generation: u64,
    /// The INCARNATION of the key's state this event describes, from the same
    /// guarded mutation.
    ///
    /// What the generation cannot express: a purge and a later relearn of ONE key
    /// both happen inside one generation, so a stale event queued before the
    /// purge and a genuine post-purge relearn are indistinguishable by generation
    /// alone. The writer compares this against the key's purge floor and drops
    /// only the superseded one.
    pub incarnation: u64,

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
    /// Fixed to `Live` today.
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
                // Through the barrier: this sweep runs on the REPLACEMENT Router
                // during a carry-over, so its generation is the live one -- but
                // routing it through the facade keeps the invariant that no
                // catalog-scoped mutation bypasses a generation check, rather
                // than relying on where this happens to be called from.
                if matches!(
                    self.learned_capabilities.expire_keyed_in_generation(
                        self.registry_generation(),
                        &entry.state_key,
                        &entry.feature_key,
                        provider_kind,
                        now,
                    ),
                    crate::learned_capability::GenerationOutcome::Stale
                ) {
                    continue;
                }
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

    /// The provider-kind token for a learned-registry `state_key`.
    ///
    /// THE single owner of this resolution. Two surfaces need it -- restating
    /// a survivor's persisted row across a reload boundary, and removing a
    /// keyed entry on operator purge -- and both must agree exactly, because
    /// the kind feeds `normalize_capability_key`: two call sites disagreeing
    /// would compute different registry keys for the same target and each
    /// would silently miss the other's rows.
    ///
    /// Resolves through the same shared `override_identity_for` map the
    /// override comparison uses, so a pooled seat key resolves through its
    /// base model exactly as it does there. An unresolvable key yields the empty string
    /// rather than a guess: an empty kind is INERT in the normalization
    /// (only the exact `bedrock` token reduces a key), so it reconstructs the
    /// identical key instead of corrupting it.
    pub fn provider_kind_for_state_key(&self, state_key: &str) -> &str {
        // Resolved identity first: a live row is the truth a dispatch would use,
        // so a reload that repointed a nickname is honoured over the config
        // tables.
        let (provider_name, nickname) = self.override_identity_for(state_key);
        if !nickname.is_empty()
            && let Some(kind) = self.kind_of_provider(&provider_name)
        {
            return kind;
        }
        // A model CONFIGURED but absent from the resolved table -- what a
        // provider that failed to build leaves behind. The learn path still keys
        // entries on such a target, so its kind must still resolve: falling
        // through to the provider-name lookup below would yield the empty kind,
        // which silently changes the registry key on any provider whose
        // normalization is not the identity (only `bedrock` today). Both the
        // exact nickname and a pooled seat's base are tried, in that order.
        if let Some(kind) = self.configured_model_kind(state_key).or_else(|| {
            state_key
                .split_once('#')
                .and_then(|(base, _label)| self.configured_model_kind(base))
        }) {
            return kind;
        }
        // A provider-scoped key (legacy or direct construction, no model scope).
        // Last, so a model shape never resolves through a same-named provider.
        self.kind_of_provider(state_key).unwrap_or("")
    }

    /// The kind of the provider a CONFIGURED model names, or `None` when the
    /// nickname is not in `[models]` or its provider is not in `[providers]`.
    fn configured_model_kind(&self, nickname: &str) -> Option<&str> {
        let model = self.config.models.get(nickname)?;
        self.kind_of_provider(&model.provider)
    }

    /// The stable kind token of a configured provider, or `None` when absent.
    fn kind_of_provider(&self, provider_name: &str) -> Option<&str> {
        self.config
            .providers
            .get(provider_name)
            .map(|p| p.kind_str())
    }

    /// Record a learned negative through the generation barrier.
    ///
    /// THE entry point for the learn path. Submits this Router's generation, so
    /// a catalog-scoped observation arriving through a superseded Router is
    /// refused ([`crate::learned_capability::GenerationOutcome::Stale`]) and the
    /// caller must then emit no ledger event and bump no metric. A wire-shape
    /// observation is accepted regardless of age -- its truth does not depend on
    /// the catalog revision, and the shared registry means it lands in the store
    /// the published Router reads.
    // Mirrors the registry call it forwards to; grouping the arguments would
    // only introduce a type that exists to satisfy a lint.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_learned_capability(
        &self,
        state_key: &str,
        feature_key: &str,
        provider_kind: &str,
        tier: SignalTier,
        phase: FailurePhase,
        source: EvidenceSource,
        evidence_class: Option<&str>,
        now: Instant,
    ) -> crate::learned_capability::GenerationOutcome<crate::learned_capability::ObserveOutcome>
    {
        self.learned_capabilities.observe_in_generation(
            self.registry_generation(),
            state_key,
            feature_key,
            provider_kind,
            tier,
            phase,
            source,
            evidence_class,
            now,
        )
    }

    /// Record a verified positive through the generation barrier, with the same
    /// staleness rule as [`Self::observe_learned_capability`].
    pub fn observe_verified_capability(
        &self,
        state_key: &str,
        feature_key: &str,
        provider_kind: &str,
        source: EvidenceSource,
        evidence_class: Option<&str>,
        now: Instant,
    ) -> crate::learned_capability::GenerationOutcome<crate::learned_capability::PositiveOutcome>
    {
        self.learned_capabilities.observe_positive_in_generation(
            self.registry_generation(),
            state_key,
            feature_key,
            provider_kind,
            source,
            evidence_class,
            now,
        )
    }

    /// The act-side routing decision, through the generation barrier.
    ///
    /// `None` means this Router's generation may not read this key -- a
    /// superseded Router asking about catalog-scoped truth. The caller treats it
    /// as no verdict rather than routing on state the reload replaced.
    /// Reached only from tests today: the production read path wants the
    /// generation alongside the decision and calls
    /// `acting_negative_with_generation`. Kept because it is the narrower of the
    /// two and the barrier tests assert on it directly.
    #[cfg(test)]
    pub(crate) fn acting_negative_for_generation(
        &self,
        state_key: &str,
        feature_key: &str,
        provider_kind: &str,
        now: Instant,
    ) -> Option<crate::learned_capability::RoutingDecision> {
        self.learned_capabilities
            .acting_negative_in_generation(
                self.registry_generation(),
                state_key,
                feature_key,
                provider_kind,
                now,
            )
            .map(|(decision, _generation)| decision)
    }

    /// Clear a keyed entry (the probe settlement) through the generation
    /// barrier. A stale settlement on a catalog-scoped key is a no-op and
    /// reports `Stale`, so no cleared event rides out to the ledger.
    pub fn clear_learned_capability(
        &self,
        state_key: &str,
        feature_key: &str,
        provider_kind: &str,
    ) -> crate::learned_capability::GenerationOutcome<bool> {
        self.learned_capabilities.remove_keyed_in_generation(
            self.registry_generation(),
            state_key,
            feature_key,
            provider_kind,
        )
    }

    /// Whether the capability is verified-working, or `false` when this
    /// Router's generation may not read it.
    ///
    /// A superseded Router must not treat catalog-scoped positive truth as its
    /// own; `false` falls through to the ordinary no-positive path.
    pub fn is_verified_working_or_false(
        &self,
        state_key: &str,
        feature_key: &str,
        provider_kind: &str,
        now: Instant,
    ) -> bool {
        self.learned_capabilities
            .is_verified_working_in_generation(
                self.registry_generation(),
                state_key,
                feature_key,
                provider_kind,
                now,
            )
            .unwrap_or(false)
    }

    /// The act-side routing decision AND the effective persistence generation it
    /// was read under.
    ///
    /// The generation is paired with the read under one guard, so a probe
    /// admission derived from this decision settles against the generation that
    /// GRANTED it -- not one sampled later, which a boundary could have moved.
    /// A stale read yields `Allow` and the live generation: there is no verdict
    /// to act on, so nothing will be admitted from it.
    pub(crate) fn acting_negative_with_generation(
        &self,
        state_key: &str,
        feature_key: &str,
        provider_kind: &str,
        now: Instant,
    ) -> (crate::learned_capability::RoutingDecision, u64) {
        match self.learned_capabilities.acting_negative_in_generation(
            self.registry_generation(),
            state_key,
            feature_key,
            provider_kind,
            now,
        ) {
            Some((decision, generation)) => (decision, generation),
            None => (
                crate::learned_capability::RoutingDecision::Allow,
                self.registry_generation(),
            ),
        }
    }

    /// Every resident learned entry whose truth does NOT depend on the catalog
    /// revision, in the shape a persisted restatement needs.
    ///
    /// Membership is the shared catalog-scope predicate's call, so a
    /// catalog-scoped entry is absent by construction -- restating one would
    /// resurrect exactly what a revision change must evict. Each survivor
    /// carries its observation time and evidence fields verbatim so a
    /// restatement preserves them rather than minting a fresh observation:
    /// a survivor is the SAME fact re-appended past a new boundary, not new
    /// evidence, so its decay age must not be refreshed.
    pub fn catalog_independent_survivors(&self) -> Vec<CatalogIndependentSurvivor> {
        self.learned_capabilities
            .snapshot()
            .into_iter()
            .filter(|entry| {
                !crate::field_capability::capability_key_is_catalog_scoped(&entry.feature_key)
            })
            .map(|entry| CatalogIndependentSurvivor {
                provider_kind: self
                    .provider_kind_for_state_key(&entry.state_key)
                    .to_string(),
                state_key: entry.state_key,
                capability: entry.feature_key,
                verdict: entry.verdict.as_str().to_string(),
                phase: entry.phase.as_str().to_string(),
                source: entry.source.as_str().to_string(),
                tier: entry.signal_tier.as_str().to_string(),
                observations: entry.observations,
                evidence_class: entry.evidence_class.clone(),
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
            })
            .collect()
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

    /// The reasoning-replay lifecycle riding on the learned-capability
    /// registry. `&self` delegate over the private field, so the dispatch
    /// arm claims a carry slot and settles it without reaching inside.
    pub(crate) fn learned_replay(&self) -> &crate::learned_replay::ReplayLearnRegistry {
        &self.learned_replay
    }

    /// The envelope-field verdict lifecycle riding on the SAME
    /// learned-capability registry. `&self` delegate over the private field,
    /// exactly as [`Router::learned_replay`] is, so the repair arm admits and
    /// settles without reaching inside.
    ///
    /// Sharing the registry is what makes a field verdict carry across a hot
    /// reload, reach the doctor surfaces, and replay from the ledger on the
    /// same terms as every other capability key -- this type owns only the
    /// in-flight coordination.
    pub(crate) fn field_verdicts(&self) -> &crate::field_verdict::FieldVerdictRegistry {
        &self.field_verdicts
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
    /// the request's derived feature set
    /// (`super::field_repair::request_feature_keys`'s catalog+field
    /// vocabulary). The resolver keys on the request-capability namespace, so this final
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
        // vocabulary (`super::field_repair::request_feature_keys`'s
        // catalog+field output); the resolver now emits
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
        let request_features = super::field_repair::request_feature_keys(req);
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
        match probe_guard.settle_same_capability(&state_key, &feature_key, provider_kind) {
            // A STALE settlement released its admission but recorded nothing, so
            // none of the consequences below may follow: no probe-failure metric
            // (no probe failure was booked), no F1Seen marker (nothing was
            // reconfirmed), and no dedupe key (there is no refreshed entry for a
            // retry to avoid re-observing). Returning early also keeps it off the
            // observe path, which would mint against a generation the daemon left.
            super::runtime_gate::SameCapabilitySettlement::Stale => return,
            super::runtime_gate::SameCapabilitySettlement::NoMatch => {}
            super::runtime_gate::SameCapabilitySettlement::Applied => {
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

        // Through the generation barrier: a catalog-scoped negative arriving on
        // a superseded Router is refused, so it neither lands in the shared
        // registry nor rides an event out to the ledger.
        let outcome = self
            .learned_capabilities
            .observe_in_generation_with_observations(
                self.registry_generation(),
                &state_key,
                &feature_key,
                provider_kind,
                tier,
                phase,
                EvidenceSource::Live,
                // A learned `broken` negative carries no evidence class; the
                // positive-detection verdicts are the ones that do.
                None,
                Instant::now(),
            );
        // The generation, the outcome, and the observation count all come FROM
        // the mutation under one guard. Reading the count separately (a second
        // snapshot after this guard releases) could straddle a sibling
        // observation on the same key and stamp this event with a count it never
        // captured. Both refusals return without an event: a stale observation
        // describes a catalog revision the daemon left, and a lease-refused one
        // would refresh an entry a purge already captured. Neither may leave the
        // dedupe entry installed either -- the mutation this request attempted
        // never happened, so the entry must not block a same-request retry once
        // the refusal condition clears (a released lease, a caught-up
        // generation); undo the speculative insert above on every refusal arm.
        let crate::learned_capability::GenerationOutcome::Applied {
            value: (observe_outcome, observations),
            generation: persistence_generation,
            incarnation,
        } = outcome
        else {
            dedupe.remove(&LearnDedupeKey::Capability {
                state_key,
                feature_key,
            });
            return;
        };
        let acting = matches!(
            observe_outcome,
            crate::learned_capability::ObserveOutcome::Acting
        );
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
                persistence_generation,
                incarnation,
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
            req.response_format.as_ref(),
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
