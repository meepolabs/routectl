//! Capability pre-filter + strip-interceptor application.

use std::collections::BTreeMap;
use std::time::Instant;

use routectl_core::capability::normalize_capability_key;
use routectl_core::failure_class::MatchedBy;
use routectl_core::{ChatRequest, Error, Result};

use crate::capability_strip::{Outcome, RequestInterceptor, StripContext, StripInterceptor};
use crate::catalog::EffectiveRow;
use crate::config::ProviderEntry;

use super::{
    DispatchSurface, DispatchTarget, ProbeAdmission, Router, UpstreamFacts, matched_by_label,
    operator_betas,
};

/// A request feature key (e.g. `web_search`, `structured_output`). Same
/// vocabulary as `crate::feature_keys`; aliased here so the feature
/// filter's decision seam reads at the right level of intent.
type FeatureKey = String;

/// What flagged a feature as unsupported for a target. The feature
/// filter's decision site returns this so the skip log can distinguish a
/// provider-scoped restriction from a model-scoped one, and so the filter
/// loop can tell a hard static drop from a soft learned de-prioritization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FilterSource {
    /// Matched a route-away override whose provenance is the legacy
    /// per-provider `unsupported_features` list.
    ProviderStatic,
    /// Matched a route-away override whose provenance is the legacy
    /// per-model `unsupported_features` list.
    ModelStatic,
    /// Matched a route-away override whose provenance is a new
    /// `[capability.overrides.<spec>].unsupported` entry.
    Override,
    /// Matched a non-expired acting negative in the learned-capability
    /// registry. A soft signal: the target is de-prioritized to the tail,
    /// never hard-dropped.
    Learned,
}

impl FilterSource {
    /// Stable lowercase token for the skip-log `source` field. The
    /// `"learned"` and `"override"` tokens are a CONTRACT consumed by
    /// downstream features (action dispatch, doctor labels, the future
    /// status endpoint).
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderStatic => "provider",
            Self::ModelStatic => "model",
            Self::Override => "override",
            Self::Learned => "learned",
        }
    }
}

impl From<crate::override_registry::OverrideProvenance> for FilterSource {
    fn from(provenance: crate::override_registry::OverrideProvenance) -> Self {
        use crate::override_registry::OverrideProvenance;
        match provenance {
            OverrideProvenance::ProviderStatic => Self::ProviderStatic,
            OverrideProvenance::ModelStatic => Self::ModelStatic,
            OverrideProvenance::Override => Self::Override,
        }
    }
}

/// What a dispatch loop must do after the per-attempt strip interceptor
/// runs. The three dispatch paths map this onto their own control flow
/// (return / continue / advance-seat).
#[derive(Debug)]
pub(super) enum StripDecision {
    /// Nothing to reject: proceed to dispatch `attempt_req` (either
    /// untouched or with the droppable capability stripped in place).
    Proceed,
    /// `strict_translation` refused a would-be strip. No mutation
    /// happened; return this 400 for the attempt without dispatching.
    StrictReject(Error),
    /// The post-strip check found a strip-created hazard; the request was
    /// restored to its pre-strip bytes. Do not dispatch it -- route away
    /// for this attempt as an ordinary route-away verdict would.
    RouteAway(Error),
}

impl Router {
    /// Filter the resolved chain by request features. Per-provider
    /// `unsupported_features` lists are consulted via the provider
    /// table; the per-model list is carried on the target. An entry
    /// whose union of those two lists intersects the request feature
    /// set is dropped with a DEBUG log (tagging the matching source).
    ///
    /// No-ops when `features` is empty (no built-in tool in the
    /// request -> nothing to filter against). Returns
    /// `Error::NotImplemented` only when the input chain was non-empty,
    /// at least one feature is in the request, AND every entry got
    /// filtered out -- the architect's "terminal empty-chain" path. A
    /// chain that was empty before filtering surfaces via the existing
    /// `Err(Error::UnknownAlias(...))` path on `dispatch_chain`.
    pub(super) fn filter_chain_by_features(
        &self,
        chain: Vec<DispatchTarget>,
        features: &[String],
        alias: &str,
        admissions: &mut Vec<ProbeAdmission>,
    ) -> Result<Vec<DispatchTarget>> {
        if features.is_empty() || chain.is_empty() {
            return Ok(chain);
        }
        // SOFT-DROP: a static (provider / model) match hard-drops the
        // target; a learned match moves it to a de-prioritized tail. The
        // result is [supported...] ++ [learned tail], each in original
        // chain order.
        let mut supported: Vec<DispatchTarget> = Vec::with_capacity(chain.len());
        let mut tail: Vec<DispatchTarget> = Vec::new();
        let mut route_aways: Vec<(String, String)> = Vec::new();
        for mut target in chain {
            let mut strip_keys: Vec<String> = Vec::new();
            match self.unsupported_feature_for_target(
                &target,
                features,
                admissions,
                &mut strip_keys,
            ) {
                None => {
                    // Strip-in-place verdict: every acting negative on this
                    // target is a droppable, non-pinned capability, so it
                    // STAYS in `supported` (no tail demotion) carrying the
                    // sorted normalized keys the interceptor will strip.
                    if !strip_keys.is_empty() {
                        target.strip_capabilities = std::sync::Arc::from(strip_keys);
                    }
                    supported.push(target);
                }
                Some((feature, FilterSource::Learned)) => {
                    // The route_away event is deferred until the final chain
                    // shape is known: its level distinguishes "an alternative
                    // remains" (INFO) from "the request survives only on the
                    // learned tail" (WARN).
                    route_aways.push((target.state_key.clone(), feature));
                    tail.push(target);
                }
                Some((feature, source)) => {
                    tracing::debug!(
                        provider = %target.provider_name,
                        model = %target.nickname.as_deref().unwrap_or(""),
                        capability_key = %feature,
                        source = %source.as_str(),
                        "target skipped: capability in unsupported_features list",
                    );
                }
            }
        }
        // NotImplemented fires ONLY when the static lists hard-dropped
        // every entry (nothing survived, not even the learned tail).
        if supported.is_empty() && tail.is_empty() {
            let feature_list = features.join(", ");
            tracing::warn!(
                alias = %alias,
                features = %feature_list,
                "alias chain filtered to empty by unsupported_features; \
                 no provider in chain supports the requested features",
            );
            return Err(Error::NotImplemented(
                alias.to_string(),
                format!("no provider in chain supports features: {feature_list}"),
            ));
        }
        // Route-away observability: each learned-tail demotion emits one
        // route_away event. INFO while a supported alternative remains; WARN
        // when the chain survives ONLY via the de-prioritized learned tail
        // (the route-away floor). Capability TOKEN + state_key only -- never a
        // request body.
        let tail_only = supported.is_empty();
        if tail_only {
            self.metrics.incr_d17_tail();
        }
        for (state_key, capability_key) in route_aways {
            if tail_only {
                tracing::warn!(
                    event = "route_away",
                    state_key = %state_key,
                    capability_key = %capability_key,
                    "learned-capability negative routed this target away; request \
                     survives only via the de-prioritized learned tail",
                );
            } else {
                tracing::info!(
                    event = "route_away",
                    state_key = %state_key,
                    capability_key = %capability_key,
                    "learned-capability negative de-prioritized this target to the tail",
                );
            }
        }
        supported.extend(tail);
        Ok(supported)
    }

    /// The single decision site for "is any requested feature
    /// unsupported for this target, and by which source". Returns the
    /// FIRST matched `(feature, source)` or `None` if the target
    /// supports every requested feature.
    ///
    /// The union is over the operator-override registry plus the learned
    /// registry. The override consult flattens the legacy per-PROVIDER and
    /// per-MODEL `unsupported_features` lists and the
    /// `[capability.overrides]` table into one provenance-preserving
    /// read-model; a `RouteAway` verdict of ANY provenance is consulted
    /// FIRST so it hard-drops (and reports its preserved source label --
    /// `provider`, `model`, or `override`) ahead of any learned signal.
    /// When the kill switch is on, a non-expired acting learned negative
    /// for this `(state_key, feature)` is consulted after.
    ///
    /// A `ForceSupported` override masks a feature: it short-circuits that
    /// feature to Allow BEFORE the learned consult, suppressing an acting
    /// learned negative and -- because the mask precedes probe-admission
    /// logic (the `in_flight` flip happens inside `acting_negative_for`) --
    /// ensuring a masked cell never claims a re-probe slot.
    ///
    /// The learned consult is admission-bearing: an expired negative whose
    /// re-probe slot this caller claims returns `None` (route to the target
    /// and test it), counting the probe attempt as a side effect and
    /// recording the claim in `admissions` so the dispatch path can settle
    /// it. The `in_flight` flip itself happens inside `acting_negative_for`.
    ///
    /// Strip-vs-route verdict: when the learned pass finds acting negatives,
    /// each is classified by [`capability_strip::action_for`]. If EVERY
    /// acting negative is a droppable `Strip` capability that no operator
    /// beta floor pins to the wire, the target is NOT unsupported -- it
    /// returns `None` and the strip keys land in `strip_keys` (sorted,
    /// normalized) for the caller to attach. If ANY acting negative maps to
    /// `RouteAway` or is operator-pinned, the whole target routes away
    /// (`Some((feature, Learned))`, `strip_keys` left empty) -- a target is
    /// never half-stripped. An admitted re-probe is excluded from
    /// `strip_keys` (the full request tests the real capability); its
    /// admission still reaches `admissions`. Override `RouteAway` matches
    /// hard-drop FIRST, ahead of any learned or strip decision.
    fn unsupported_feature_for_target(
        &self,
        target: &DispatchTarget,
        features: &[FeatureKey],
        admissions: &mut Vec<ProbeAdmission>,
        strip_keys: &mut Vec<String>,
    ) -> Option<(FeatureKey, FilterSource)> {
        // Override consult replaces the two raw static-list scans: the
        // registry (built from the legacy provider / model
        // `unsupported_features` lists plus `[capability.overrides]`)
        // hard-drops on a `RouteAway` of ANY provenance, reporting the
        // preserved source label so an existing config's behavior and
        // labels stay byte-identical.
        let nickname = target.nickname.as_deref().unwrap_or("");
        for feature in features {
            if let Some((crate::override_registry::OverrideVerdict::RouteAway, provenance)) =
                self.override_registry.resolve(
                    &target.provider_name,
                    nickname,
                    feature,
                    target.provider_kind.unwrap_or(""),
                )
            {
                return Some((feature.clone(), provenance.into()));
            }
        }
        // Learned pass: consult the adaptive registry only when the kill
        // switch is on and the target carries a provider kind (legacy /
        // direct-construction targets without one skip the registry).
        // Scan EVERY feature: an earlier feature's `ProbeAdmitted` must not
        // short-circuit a later feature's `RouteAway`, and `acting_negative_for`
        // flips `in_flight` as a side effect on `ProbeAdmitted`, so every
        // admission has to reach `admissions` for its guard to settle the slot
        // -- dropping one leaks `in_flight` and blocks that feature from ever
        // re-probing. Any `RouteAway` tail-drops the target after the full scan.
        if self.config.capability.enabled
            && let Some(provider_kind) = target.provider_kind
        {
            let now = Instant::now();
            let mut route_away: Option<FeatureKey> = None;
            let mut strip: Vec<String> = Vec::new();
            for feature in features {
                // ForceSupported mask: an operator `force_supported`
                // override short-circuits this feature to Allow BEFORE
                // `acting_negative_for` runs, so a masked cell never
                // suppresses only the verdict while still claiming a
                // re-probe slot (the `in_flight` flip happens inside
                // `acting_negative_for`). A `RouteAway` override can never
                // reach here -- it hard-dropped in the consult above.
                if self.override_forces_supported(target, feature, provider_kind) {
                    continue;
                }
                match self.learned_capabilities.acting_negative_for(
                    &target.state_key,
                    feature,
                    provider_kind,
                    now,
                ) {
                    crate::learned_capability::RoutingDecision::RouteAway(_) => {
                        // Strip-vs-route: a droppable capability the operator
                        // has not pinned to the wire is stripped in place;
                        // everything else (essentials, unknowns, pinned betas)
                        // routes away. A pinned strip would be re-added
                        // downstream, so its "success" is false -- route away.
                        if matches!(
                            crate::capability_strip::action_for(feature),
                            crate::capability_strip::CapabilityAction::Strip(_)
                        ) && !self.beta_pinned_for_target(target, feature)
                        {
                            strip.push(normalize_capability_key(feature, provider_kind));
                        } else if route_away.is_none() {
                            route_away = Some(feature.clone());
                        }
                    }
                    crate::learned_capability::RoutingDecision::ProbeAdmitted => {
                        self.metrics.incr_probe_attempts();
                        let normalized = normalize_capability_key(feature, provider_kind);
                        // Probe bypass: the admitted feature tests the REAL
                        // capability on the full request, so it is never
                        // stripped -- a stripped success would falsely clear
                        // the negative the probe is meant to re-verify. When
                        // the bypassed feature WOULD otherwise have been
                        // stripped in place (a droppable `Strip` the operator
                        // has not pinned to the wire -- the exact condition the
                        // `RouteAway` arm strips on), surface it: the strip WARN
                        // vocabulary's `probe_bypassed` outcome fires here, with
                        // the same field shape as the per-decision WARN in
                        // `apply_strip_interceptor`. Route-away features do not
                        // fire -- they were never strip-eligible. Capability
                        // TOKEN and state_key only -- never request bodies.
                        if matches!(
                            crate::capability_strip::action_for(feature),
                            crate::capability_strip::CapabilityAction::Strip(_)
                        ) && !self.beta_pinned_for_target(target, feature)
                        {
                            tracing::warn!(
                                event = "strip",
                                state_key = %target.state_key,
                                capability_key = %normalized,
                                outcome = "probe_bypassed",
                                "capability_strip_decision",
                            );
                        }
                        admissions.push(ProbeAdmission {
                            state_key: target.state_key.clone(),
                            feature: normalized,
                            provider_kind,
                        });
                    }
                    crate::learned_capability::RoutingDecision::Allow => {}
                }
            }
            // ANY route-away (or operator-pinned) acting negative demotes the
            // whole target; the strip set is abandoned so a mixed target is
            // never half-stripped-half-routed.
            if let Some(feature) = route_away {
                return Some((feature, FilterSource::Learned));
            }
            if !strip.is_empty() {
                strip.sort_unstable();
                strip.dedup();
                *strip_keys = strip;
            }
        }
        None
    }

    /// Whether stripping `feature` on this target would be silently undone
    /// on the wire by an operator beta floor. A `Strip(BetaFlag)`
    /// capability's beta token can be pinned by the provider `anthropic_beta`
    /// config (Bedrock, re-added post-strip) or a provider/model
    /// `header_extras` `anthropic-beta` contribution (Anthropic-API,
    /// re-added via `operator_betas`). Either source makes the strip
    /// ineffective, so the caller must route away instead. Non-beta strips
    /// (e.g. a tool-shape strip) carry no beta token and are never pinned.
    fn beta_pinned_for_target(&self, target: &DispatchTarget, feature: &str) -> bool {
        let tokens = crate::capability_strip::strip_beta_tokens(feature);
        if tokens.is_empty() {
            return false;
        }
        let provider_entry = self.config.providers.get(&target.provider_name);
        let provider_floor = provider_entry.map_or(&[][..], ProviderEntry::anthropic_beta_floor);
        let header_floor = operator_betas(
            provider_entry.map(ProviderEntry::header_extras),
            &target.model.header_extras,
        );
        tokens.iter().any(|token| {
            provider_floor.iter().any(|pinned| pinned == token)
                || header_floor.iter().any(|pinned| pinned == token)
        })
    }

    /// Whether an operator `force_supported` override masks `feature` for
    /// this target -- the single consult shared by the act side (which
    /// short-circuits a masked feature to Allow before probe admission) and
    /// the learn side (which suppresses the observe for a masked cell). The
    /// same `(provider, nickname)` two-tier resolve both paths key on, so a
    /// mask is never honored on one side and missed on the other.
    pub(super) fn override_forces_supported(
        &self,
        target: &DispatchTarget,
        feature: &str,
        provider_kind: &str,
    ) -> bool {
        matches!(
            self.override_registry.resolve(
                &target.provider_name,
                target.nickname.as_deref().unwrap_or(""),
                feature,
                provider_kind,
            ),
            Some((crate::override_registry::OverrideVerdict::ForceSupported, _))
        )
    }

    /// Run the single request interceptor over one per-attempt clone and
    /// map its outcome to a loop-actionable [`StripDecision`], emitting the
    /// per-decision observability (a structured WARN per capability key
    /// plus the matching `RouterMetrics` counter).
    ///
    /// Called at all three dispatch paths immediately after
    /// `apply_layered_overlays` and before context reduction / auto-cache,
    /// so the bytes reduction, cache planning, and dispatch observe are the
    /// stripped bytes, and the strip runs downstream of the beta floor. The
    /// caller's original `req` is never passed here -- only `attempt_req`.
    ///
    /// `target.strip_capabilities` is consumed as-is: it is empty unless an
    /// acting learned negative resolved to a non-pinned droppable, so a
    /// disabled kill switch (or a probe-admitted / operator-pinned feature)
    /// leaves this inert by construction. The keys arrive already sorted
    /// and normalized.
    pub(super) fn apply_strip_interceptor(
        &self,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
    ) -> StripDecision {
        if target.strip_capabilities.is_empty() {
            return StripDecision::Proceed;
        }
        let strict = self.config.server.strict_translation;
        let ctx = StripContext {
            keys: target.strip_capabilities.to_vec(),
            strict,
        };
        let outcome = StripInterceptor.apply(attempt_req, &ctx);
        let outcome_token = match &outcome {
            Outcome::Stripped => "applied",
            Outcome::Unchanged => "noop",
            Outcome::Reject(_) if strict => "strict_rejected",
            Outcome::Reject(_) => "validation_rolled_back",
        };
        // One WARN per strip decision. `capability_key` names the verdict's
        // keys (already sorted + normalized); the outcome is the run-level
        // decision, so joining avoids misreporting a per-key outcome the
        // aggregate `Outcome` cannot distinguish. Capability TOKEN and
        // state_key only -- never request bodies (log hygiene).
        // `probe_bypassed` is emitted upstream at the verdict site (an
        // admitted feature never reaches this verdict -- it arrives empty).
        // `disabled` has no per-decision emission: a disabled kill switch
        // skips the verdict entirely, so no per-decision context exists to
        // name.
        tracing::warn!(
            event = "strip",
            state_key = %target.state_key,
            capability_key = %target.strip_capabilities.join(", "),
            outcome = outcome_token,
            "capability_strip_decision",
        );
        match outcome {
            Outcome::Stripped => {
                self.metrics.incr_strip();
                StripDecision::Proceed
            }
            Outcome::Unchanged => StripDecision::Proceed,
            Outcome::Reject(err) if strict => {
                self.metrics.incr_strip_strict_rejected();
                StripDecision::StrictReject(err)
            }
            Outcome::Reject(err) => {
                self.metrics.incr_strip_rollback();
                StripDecision::RouteAway(err)
            }
        }
    }
}

/// The merged catalog capability priors for a resolved model, cloned off
/// its `EffectiveRow`. Empty when the cell is `Disabled` / `Missing` (the
/// conservative no-prior baseline) or carries no capability data.
pub(super) fn catalog_capabilities(effective_row: &EffectiveRow) -> BTreeMap<String, bool> {
    effective_row
        .priced()
        .map(|row| row.capabilities.clone())
        .unwrap_or_default()
}

/// Emit the stable FeatureUnsupported observability event at a dispatch
/// error arm. Fired only when the classifier lifted the failure to
/// [`FailureClass::FeatureUnsupported`]. Carries only safe, structured
/// dimensions -- NEVER a body, prompt, header, token, or the error's
/// Display/Debug text. `capability` is the upstream token the classifier
/// matched, already best-effort and non-sensitive. `remapped` is true
/// when this FeatureUnsupported came from an operator status remap
/// (carrying the `OPERATOR_REMAP_CAPABILITY` token) rather than a real
/// upstream lift.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_feature_unsupported(
    provider: &str,
    provider_kind: Option<&str>,
    model: &str,
    capability: &str,
    facts: &UpstreamFacts<'_>,
    matched_by: MatchedBy,
    surface: DispatchSurface,
    is_forwarded: bool,
    remapped: bool,
) {
    tracing::info!(
        target: "routectl::feature_unsupported",
        provider,
        provider_kind = provider_kind.unwrap_or(""),
        model,
        capability,
        status = facts.status.unwrap_or(0),
        upstream_type = facts.upstream_type.unwrap_or(""),
        upstream_code = facts.upstream_code.unwrap_or(""),
        matched_by = matched_by_label(matched_by),
        surface = surface.as_str(),
        is_forwarded,
        remapped,
        "upstream reported an unsupported capability",
    );
}

#[cfg(test)]
#[path = "feature_filter_tests.rs"]
mod feature_filter_tests;

#[cfg(test)]
#[path = "capability_override_filter_tests.rs"]
mod capability_override_filter_tests;

#[cfg(test)]
#[path = "strip_interceptor_dispatch_tests.rs"]
mod strip_interceptor_dispatch_tests;

#[cfg(test)]
#[path = "strip_wire_egress_tests.rs"]
mod strip_wire_egress_tests;
