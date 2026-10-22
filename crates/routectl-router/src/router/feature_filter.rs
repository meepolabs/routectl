//! Capability pre-filter + strip-interceptor application.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::time::Instant;

use routectl_core::capability::normalize_capability_key;
use routectl_core::failure_class::MatchedBy;
use routectl_core::{ChatRequest, Error, Result, sanitize_for_log};

use crate::capability_precedence::{
    CellContext, Layer, LearnedInput, OpenCell, Resolution, Routing, Stage, Wire, resolve_open,
    resolve_operator,
};
use crate::capability_strip::{Outcome, RequestInterceptor, StripContext, StripInterceptor};
use crate::catalog::EffectiveRow;
use crate::learned_capability::{BetaLaneReading, ProbeClaim};

use super::class_observe::{DispatchSurface, UpstreamFacts, matched_by_label};
use super::edit_declare::{declare_learned_strip, strip_before};
use super::{DispatchTarget, ProbeAdmission, Router};

/// A request feature key (e.g. `web_search`, `structured_output`). Same
/// vocabulary as `crate::feature_keys`; aliased here so the feature
/// filter's decision seam reads at the right level of intent.
type FeatureKey = String;

/// What flagged a feature as unsupported for a target. The feature
/// filter's decision site returns this so the skip log can name the
/// source, and so the filter loop can tell a hard override drop from a
/// soft learned or prior de-prioritization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FilterSource {
    /// Matched a route-away `[capability.overrides.<spec>].unsupported`
    /// entry.
    Override,
    /// Matched a non-expired acting negative in the learned-capability
    /// registry. A soft signal: the target is de-prioritized to the tail,
    /// never hard-dropped.
    Learned,
    /// Matched a `Some(false)` catalog capability prior for a feature the
    /// learned pass did not already decide. The weakest signal: a static
    /// catalog assertion of absence, de-prioritized to the tail (ahead of
    /// the learned tail), never hard-dropped and never contributing a
    /// strip key.
    Prior,
}

impl FilterSource {
    /// Stable lowercase token for the skip-log `source` field. The
    /// `"learned"`, `"override"`, and `"prior"` tokens are a CONTRACT
    /// consumed by downstream features (action dispatch, doctor labels, the
    /// future status endpoint).
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Learned => "learned",
            Self::Prior => "prior",
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
    /// happened; do not dispatch this target. A messages walk moves on to
    /// the next target; the token count ends on it.
    StrictReject(Error),
    /// The post-strip check found a strip-created hazard; the request was
    /// restored to its pre-strip bytes. Do not dispatch it -- route away
    /// for this attempt as an ordinary route-away verdict would.
    RouteAway(Error),
}

impl Router {
    /// Filter the resolved chain by request features. An entry whose
    /// provider- or model-scoped `[capability.overrides]` `unsupported`
    /// list intersects the request feature set is dropped with a DEBUG
    /// log (tagging the matching source); learned and catalog-prior
    /// negatives demote the entry to a tail instead.
    ///
    /// No-ops when `features` is empty (no built-in tool in the
    /// request -> nothing to filter against). Returns
    /// `Error::NotImplemented` only when the input chain was non-empty,
    /// at least one feature is in the request, AND every entry got
    /// filtered out -- the architect's "terminal empty-chain" path. A
    /// chain that was empty before filtering surfaces via the existing
    /// `Err(Error::UnknownAlias(...))` path on `dispatch_chain`.
    ///
    /// On an inference `surface`, a feature every target refuses outright
    /// fails the request with `Error::CapabilityUnsupportedEverywhere`
    /// before the claiming pass runs (see
    /// [`Self::refuse_if_excluded_everywhere`]).
    pub(super) fn filter_chain_by_features(
        &self,
        chain: Vec<DispatchTarget>,
        features: &[String],
        alias: &str,
        surface: DispatchSurface,
        admissions: &mut Vec<ProbeAdmission>,
    ) -> Result<Vec<DispatchTarget>> {
        if features.is_empty() || chain.is_empty() {
            return Ok(chain);
        }
        self.refuse_if_excluded_everywhere(&chain, features, alias, surface)?;
        // SOFT-DROP: an override `unsupported` match hard-drops the
        // target; a learned or catalog-prior match moves it to a
        // de-prioritized tail. The result is
        // [supported...] ++ [prior tail] ++ [learned tail]: a catalog
        // prior is weaker evidence than a learned negative, so
        // prior-demoted targets sort ahead of learned-demoted ones;
        // original chain order is preserved within each bucket.
        let mut supported: Vec<DispatchTarget> = Vec::with_capacity(chain.len());
        let mut prior_tail: Vec<DispatchTarget> = Vec::new();
        let mut learned_tail: Vec<DispatchTarget> = Vec::new();
        let mut route_aways: Vec<(String, String, FilterSource)> = Vec::new();
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
                    // tail" (WARN).
                    route_aways.push((target.state_key.clone(), feature, FilterSource::Learned));
                    learned_tail.push(target);
                }
                Some((feature, FilterSource::Prior)) => {
                    // Prior demotion: the catalog asserts this feature absent.
                    // F1 strip keys the learned pass already resolved SURVIVE
                    // so a demoted lane that still gets attempted strips its
                    // known tokens (the prior itself never adds a strip key).
                    if !strip_keys.is_empty() {
                        target.strip_capabilities = std::sync::Arc::from(strip_keys);
                    }
                    route_aways.push((target.state_key.clone(), feature, FilterSource::Prior));
                    prior_tail.push(target);
                }
                Some((feature, source)) => {
                    tracing::debug!(
                        provider = %routectl_core::sanitize_for_log(&target.provider_name),
                        model = %routectl_core::sanitize_for_log(
                            target.nickname.as_deref().unwrap_or("")
                        ),
                        capability_key = %feature,
                        source = %source.as_str(),
                        "target skipped: capability in an unsupported override",
                    );
                }
            }
        }
        // NotImplemented fires ONLY when override `unsupported` entries
        // hard-dropped every entry (nothing survived, not even the de-prioritized tail).
        if supported.is_empty() && prior_tail.is_empty() && learned_tail.is_empty() {
            let feature_list = features.join(", ");
            tracing::warn!(
                alias = %sanitize_for_log(alias),
                features = %sanitize_for_log(&feature_list),
                "alias chain filtered to empty by unsupported overrides; \
                 no provider in chain supports the requested features",
            );
            return Err(Error::NotImplemented(
                alias.to_string(),
                format!("no provider in chain supports features: {feature_list}"),
            ));
        }
        // Route-away observability: each tail demotion (learned or catalog
        // prior) emits one route_away event tagged with its source. INFO
        // while a supported alternative remains; WARN when the chain
        // survives ONLY via the de-prioritized tail (the route-away floor).
        // Capability TOKEN + state_key only -- never a request body.
        let tail_only = supported.is_empty();
        if tail_only {
            self.metrics.incr_d17_tail();
        }
        for (state_key, capability_key, source) in route_aways {
            if tail_only {
                tracing::warn!(
                    event = "route_away",
                    state_key = %sanitize_for_log(&state_key),
                    capability_key = %capability_key,
                    source = %source.as_str(),
                    "capability negative routed this target away; request \
                     survives only via the de-prioritized tail",
                );
            } else {
                tracing::info!(
                    event = "route_away",
                    state_key = %sanitize_for_log(&state_key),
                    capability_key = %capability_key,
                    source = %source.as_str(),
                    "capability negative de-prioritized this target to the tail",
                );
            }
        }
        // Prior-demoted targets sort ahead of learned-demoted ones.
        supported.extend(prior_tail);
        supported.extend(learned_tail);
        Ok(supported)
    }

    /// The single decision site for "is any requested feature
    /// unsupported for this target, and by which source". Returns the
    /// strongest `(feature, source)` or `None` if the target keeps its place.
    ///
    /// This gathers each feature's inputs and maps the precedence seam's
    /// [`Resolution`] onto the filter's outputs; every precedence decision
    /// is the seam's ([`resolve_operator`], then [`resolve_open`]).
    ///
    /// The operator stage runs over EVERY feature before any learned read, so
    /// an override drop, a `force_supported` mask or a pinned beta key never
    /// reaches the claiming read and never flips `in_flight`. A pinned
    /// feature key stays open (the pin only rules out strip), so it still
    /// reads, and a lapsed negative on it still re-probes.
    ///
    /// The learned read is admission-bearing: a lapsed negative whose
    /// re-probe slot this caller claims counts the probe attempt and lands in
    /// `admissions` so the dispatch path can settle it. Every open feature is
    /// read, so no admission is dropped by an earlier feature's verdict.
    ///
    /// Mapping: a learned tail demotes the whole target with `strip_keys`
    /// left empty (a target is never half-stripped); otherwise every
    /// strip-in-place feature lands in `strip_keys` (sorted, normalized), and
    /// they survive a catalog-prior tail.
    fn unsupported_feature_for_target(
        &self,
        target: &DispatchTarget,
        features: &[FeatureKey],
        admissions: &mut Vec<ProbeAdmission>,
        strip_keys: &mut Vec<String>,
    ) -> Option<(FeatureKey, FilterSource)> {
        let open = match self.operator_stage(target, features) {
            ControlFlow::Break(dropped) => return Some((dropped.clone(), FilterSource::Override)),
            ControlFlow::Continue(open) => open,
        };
        let now = Instant::now();
        let resolved: Vec<(&FeatureKey, Resolution)> = open
            .into_iter()
            .map(|(feature, ctx, cell)| {
                let learned = self.learned_input_for(target, feature, ctx, cell, admissions, now);
                // A target with no provider kind consults no capability truth,
                // the catalog prior included.
                let prior = target
                    .provider_kind
                    .and_then(|_| target.capability_prior(feature));
                (feature, resolve_open(learned, None, prior, cell))
            })
            .collect();
        map_resolutions(&resolved, target.provider_kind, strip_keys)
    }

    /// The operator stage for every feature: `Break` with the first feature
    /// an override drops, else `Continue` with the features left open.
    fn operator_stage<'f>(
        &self,
        target: &DispatchTarget,
        features: &'f [FeatureKey],
    ) -> ControlFlow<&'f FeatureKey, Vec<(&'f FeatureKey, CellContext, OpenCell)>> {
        let mut open = Vec::with_capacity(features.len());
        for feature in features {
            let (ctx, stage) = self.operator_cell(target, feature);
            match stage {
                Stage::Decided(decided) if decided.routing() == Routing::Drop => {
                    return ControlFlow::Break(feature);
                }
                Stage::Decided(_) => {}
                Stage::Open(cell) => open.push((feature, ctx, cell)),
            }
        }
        ControlFlow::Continue(open)
    }

    /// The operator tiers' verdict on one `(target, feature)` cell, with the
    /// cell's context: the one place both the claiming pass and the
    /// hard-exclusion pre-pass gather the operator inputs.
    fn operator_cell(&self, target: &DispatchTarget, feature: &str) -> (CellContext, Stage) {
        let verdict = self.override_registry.resolve(
            &target.provider_name,
            target.nickname.as_deref().unwrap_or(""),
            feature,
            target.provider_kind.unwrap_or(""),
        );
        let ctx = CellContext::new(
            feature,
            &self.config.capability,
            target.egress_kind,
            &target.upstream,
        );
        let stage = resolve_operator(self.beta_pinned_for_target(target, feature), verdict, ctx);
        (ctx, stage)
    }

    /// Fail the request when some requested feature is refused outright on
    /// every target, before any re-probe slot is claimed.
    ///
    /// It runs ahead of the claiming pass and reads the registry without
    /// claiming: an error returned after a claim would leave that slot held
    /// with no dispatch to settle it, locking the lane out of its re-probe.
    /// A target counts as refusing only when the seam calls its cell
    /// [`Resolution::hard_exclusion_eligible`]. A lapsed negative whose slot
    /// is free makes the chain ineligible, so the claiming pass dispatches the
    /// probe; one whose slot another request holds still counts as refusing,
    /// so a burst of requests does not each send a known rejection.
    ///
    /// count_tokens keeps the soft tail: it never fails here. Neither does an
    /// envelope-field key: a field negative is answered by repairing the
    /// field (the pre-flight and reactive repair paths), never by refusing
    /// the request.
    fn refuse_if_excluded_everywhere(
        &self,
        chain: &[DispatchTarget],
        features: &[FeatureKey],
        alias: &str,
        surface: DispatchSurface,
    ) -> Result<()> {
        if matches!(surface, DispatchSurface::CountTokens) {
            return Ok(());
        }
        let now = Instant::now();
        let Some(feature) = features.iter().find(|feature| {
            !crate::field_capability::capability_key_is_field_verdict(feature)
                && chain
                    .iter()
                    .all(|target| self.cell_refuses_outright(target, feature, now))
        }) else {
            return Ok(());
        };
        let mut lanes: Vec<String> = Vec::with_capacity(chain.len());
        for lane in chain
            .iter()
            .filter_map(|target| target.learned_key(feature))
        {
            if !lanes.iter().any(|seen| seen == lane) {
                lanes.push(lane.to_string());
            }
        }
        let lane_list = lanes
            .iter()
            .map(|lane| sanitize_for_log(lane))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            event = "hard_exclusion",
            alias = %sanitize_for_log(alias),
            capability_key = %sanitize_for_log(feature),
            lanes = %lane_list,
            surface = surface.as_str(),
            "capability refused on every lane; request excluded before dispatch",
        );
        self.metrics.incr_capability_hard_excluded();
        Err(Error::capability_unsupported_everywhere(
            feature.as_str(),
            lanes,
        ))
    }

    /// Whether the seam calls this target's cell for `feature` refused
    /// outright, from a learned read that claims nothing.
    fn cell_refuses_outright(&self, target: &DispatchTarget, feature: &str, now: Instant) -> bool {
        let Stage::Open(cell) = self.operator_cell(target, feature).1 else {
            return false;
        };
        let learned = self.peeked_learned_input(target, feature, cell, now);
        let prior = target
            .provider_kind
            .and_then(|_| target.capability_prior(feature));
        resolve_open(learned, None, prior, cell).hard_exclusion_eligible()
    }

    /// The learned read for one open feature that never claims a re-probe
    /// slot: a lapsed negative with a free slot reads as lapsed, one whose
    /// slot is held reads as still acting.
    fn peeked_learned_input(
        &self,
        target: &DispatchTarget,
        feature: &str,
        cell: OpenCell,
        now: Instant,
    ) -> Option<LearnedInput> {
        if !cell.reads_learned() {
            return None;
        }
        let provider_kind = target.provider_kind?;
        let learned_key = target.learned_key(feature)?;
        let (reading, _) = self.beta_reading_with_generation(
            learned_key,
            feature,
            provider_kind,
            ProbeClaim::Peek,
            now,
        );
        LearnedInput::from_reading(reading)
    }

    /// The claiming learned read for one open feature, or `None` when the
    /// learned tier cannot speak (kill switch off, no provider kind, no
    /// lane). A claimed re-probe is recorded in `admissions`.
    fn learned_input_for(
        &self,
        target: &DispatchTarget,
        feature: &str,
        ctx: CellContext,
        cell: OpenCell,
        admissions: &mut Vec<ProbeAdmission>,
        now: Instant,
    ) -> Option<LearnedInput> {
        if !cell.reads_learned() {
            return None;
        }
        let provider_kind = target.provider_kind?;
        let learned_key = target.learned_key(feature)?;
        let (reading, generation) = self.beta_reading_with_generation(
            learned_key,
            feature,
            provider_kind,
            ProbeClaim::Claim,
            now,
        );
        if let BetaLaneReading::ProbeAdmitted { .. } = reading {
            self.metrics.incr_probe_attempts();
            let normalized = normalize_capability_key(feature, provider_kind);
            if probe_bypasses_a_strip(ctx, cell) {
                tracing::warn!(
                    event = "strip",
                    state_key = %sanitize_for_log(&target.state_key),
                    capability_key = %normalized,
                    outcome = "probe_bypassed",
                    "capability_strip_decision",
                );
            }
            admissions.push(ProbeAdmission {
                state_key: target.state_key.clone(),
                learned_key: learned_key.to_string(),
                feature: normalized,
                provider_kind,
                // From the read that granted it, not a later sample.
                generation,
            });
        }
        LearnedInput::from_reading(reading)
    }

    /// Whether an operator beta floor pins capability `feature` on this
    /// target, so the wire carries it whatever the router decides. A beta
    /// key is pinned when its flag is. A `Strip(BetaFlag)` capability's beta
    /// token can be pinned by the provider `anthropic_beta` config (Bedrock,
    /// re-added post-strip) or a provider/model `header_extras`
    /// `anthropic-beta` contribution (Anthropic-API, re-added via
    /// `operator_betas`). Either source makes the strip ineffective, so the
    /// caller must route away instead. Non-beta strips (e.g. a tool-shape
    /// strip) carry no beta token and are never pinned.
    pub(super) fn beta_pinned_for_target(&self, target: &DispatchTarget, feature: &str) -> bool {
        crate::capability_precedence::operator_pins_capability(
            &self.config,
            &target.provider_name,
            &target.model.header_extras,
            feature,
        )
    }

    /// Whether an operator `force_supported` override masks `feature` for
    /// this target, for the learn side (which suppresses the observe for a
    /// masked cell). It reads the same `(provider, nickname)` two-tier
    /// resolve the act side hands the precedence seam, so a mask is never
    /// honored on one side and missed on the other.
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
            Some(crate::override_registry::OverrideVerdict::ForceSupported)
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
        let before = strip_before(target, attempt_req);
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
            state_key = %sanitize_for_log(&target.state_key),
            capability_key = %target.strip_capabilities.join(", "),
            outcome = outcome_token,
            "capability_strip_decision",
        );
        match outcome {
            Outcome::Stripped => {
                if let Some(before) = &before {
                    declare_learned_strip(attempt_req, before);
                }
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

/// Whether an admitted re-probe skips a strip the cell would otherwise get:
/// an acting F1 negative strips a droppable feature key the operator does
/// not pin. An admitted probe sends the real capability (a stripped success
/// would falsely clear the negative), so the `probe_bypassed` strip outcome
/// is reported instead.
const fn probe_bypasses_a_strip(ctx: CellContext, cell: OpenCell) -> bool {
    !ctx.beta_flag() && ctx.droppable() && !cell.pinned()
}

/// Map each open feature's [`Resolution`] onto the filter's outputs. A
/// learned tail outranks a prior tail whatever the feature order, and the
/// first feature in request order names the tail within a tier.
fn map_resolutions(
    resolved: &[(&FeatureKey, Resolution)],
    provider_kind: Option<&'static str>,
    strip_keys: &mut Vec<String>,
) -> Option<(FeatureKey, FilterSource)> {
    let tail = resolved
        .iter()
        .filter(|(_, resolution)| resolution.routing() == Routing::Tail)
        .min_by_key(|(_, resolution)| resolution.layer());
    if let Some((feature, resolution)) = tail
        && resolution.layer() == Some(Layer::Learned)
    {
        return Some(((*feature).clone(), FilterSource::Learned));
    }
    let mut strip: Vec<String> = resolved
        .iter()
        .filter(|(_, resolution)| resolution.wire() == Wire::Strip)
        .map(|(feature, _)| normalize_capability_key(feature, provider_kind.unwrap_or("")))
        .collect();
    if !strip.is_empty() {
        strip.sort_unstable();
        strip.dedup();
        *strip_keys = strip;
    }
    tail.map(|(feature, _)| ((*feature).clone(), FilterSource::Prior))
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
/// [`FailureClass::FeatureUnsupported`](routectl_core::failure_class::FailureClass::FeatureUnsupported).
/// Carries only safe, structured dimensions -- NEVER a body, prompt, header,
/// token, or the error's Display/Debug text. `capability` is the upstream token
/// the classifier matched, already best-effort and non-sensitive. `remapped` is
/// true when this FeatureUnsupported came from an operator status remap
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

#[cfg(test)]
#[path = "capability_precedence_matrix_tests.rs"]
mod capability_precedence_matrix_tests;

#[cfg(test)]
#[path = "capability_precedence_walk_tests.rs"]
mod capability_precedence_walk_tests;

#[cfg(test)]
#[path = "feature_filter_read_tests.rs"]
mod feature_filter_read_tests;

#[cfg(test)]
#[path = "computer_tool_key_tests.rs"]
mod computer_tool_key_tests;

#[cfg(test)]
#[path = "hard_exclusion_tests.rs"]
mod hard_exclusion_tests;
