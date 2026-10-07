//! Learning from the beta flags a provider reports it stripped.
//!
//! A provider that drops a client `anthropic-beta` flag after the upstream
//! rejected it by name, and then succeeds without it, records the flag on the
//! per-attempt [`BetaRepairReport`]. The attempt succeeded, but the upstream
//! still refused that flag on this lane, so the router mints a `beta:<flag>`
//! negative for the target through the same probe-settle and mint path a 400
//! rejection takes.
//!
//! The converse is evidence too. A target admitted to re-probe a lapsed beta
//! negative sent that flag; if the attempt succeeded and the report does not
//! name it, the upstream accepted it, and the router records a verified
//! positive. A plain clear would not do: the flag's cell would fall back to
//! the shipped seed, which withholds it again, so live acceptance could never
//! override the seed.

use std::collections::HashSet;
use std::time::Instant;

use routectl_core::capability::{BETA_ACCEPTED, EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{BetaRepairReport, ChatRequest};

use super::capability_learn::{LearnDedupeKey, LearnedMint};
use super::capability_observe::CapabilityObserveEvent;
use super::runtime_gate::ProbeAdmission;
use super::{DispatchMeta, DispatchTarget, LearnedProbeGuard, Router};
use crate::beta_capability::beta_capability_key;
use crate::capability_detect::ObservationDirection;
use crate::learned_capability::{GenerationOutcome, PositiveOutcome};

/// The status recorded on a beta negative's learn event: the upstream named
/// the flag in a 400 before the provider's stripped retry succeeded.
const BETA_REJECTION_STATUS: u16 = 400;

/// Install a fresh, empty report slot on the request about to go upstream and
/// return the handle the router reads after the call.
///
/// Called once per provider call, never once per target: every call clones
/// the request, and clones share one slot, so a slot reused across retries
/// would carry an earlier failed attempt's flags into a later success.
pub(super) fn install_beta_repair_report(attempt_req: &mut ChatRequest) -> BetaRepairReport {
    let report = BetaRepairReport::default();
    attempt_req.routectl_internal.beta_repair_report = Some(report.clone());
    report
}

impl Router {
    /// Settle a successful attempt, in three steps: every flag the provider
    /// reported stripping, as a rejection; then every held beta re-probe the
    /// report did not name, as a verified positive; then the remaining held
    /// re-probes as successes.
    ///
    /// The order is load-bearing. A reported flag was rejected even though the
    /// attempt succeeded; settled after the success, an admitted re-probe for
    /// that flag would clear the very negative the upstream just re-confirmed.
    /// An unreported beta re-probe must leave the held set before the success
    /// settlement, which would otherwise clear it.
    pub(super) fn settle_attempt_success(
        &self,
        beta_report: &BetaRepairReport,
        target: &DispatchTarget,
        req: &ChatRequest,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
        probe_guard: &mut LearnedProbeGuard,
    ) {
        let reported = beta_report.take();
        self.learn_reported_betas(&reported, target, req, dedupe, meta, probe_guard);
        let reported_keys: Vec<String> = reported
            .iter()
            .filter_map(|flag| beta_capability_key(flag))
            .collect();
        for admission in probe_guard.take_unreported_beta_admissions(&reported_keys) {
            self.record_accepted_beta(&admission, req, meta);
        }
        meta.cleared_capabilities
            .extend(probe_guard.settle_success());
    }

    /// Record a held beta re-probe the upstream accepted as a verified
    /// positive, through the generation barrier at the generation that granted
    /// the admission, and ride a `verified` observation out on `meta` only when
    /// the verdict transitioned. A same-verdict refresh restates nothing a warm
    /// rebuild needs; a stale generation or a purge lease records nothing.
    ///
    /// Recording the positive also releases the admission: the resident
    /// negative is replaced, so its `in_flight` slot goes with it.
    fn record_accepted_beta(
        &self,
        admission: &ProbeAdmission,
        req: &ChatRequest,
        meta: &mut DispatchMeta,
    ) {
        let outcome = self
            .learned_capabilities
            .observe_accepted_beta_in_generation(
                admission.generation,
                &admission.learned_key,
                &admission.feature,
                admission.provider_kind,
                Instant::now(),
            );
        let GenerationOutcome::Applied {
            value,
            generation: persistence_generation,
            incarnation,
        } = outcome
        else {
            tracing::debug!(
                event = "probe_settlement_stale",
                state_key = %routectl_core::sanitize_for_log(&admission.state_key),
                capability_key = %admission.feature,
                attempted_outcome = "beta_accepted",
                "accepted beta re-probe not recorded: its admission predates the live capability generation",
            );
            return;
        };
        if value != PositiveOutcome::Recorded {
            return;
        }
        self.metrics.incr_verified_working();
        tracing::info!(
            event = "observe",
            state_key = %routectl_core::sanitize_for_log(&admission.state_key),
            lane = %routectl_core::sanitize_for_log(&admission.learned_key),
            capability_key = %admission.feature,
            provider_kind = admission.provider_kind,
            evidence_class = BETA_ACCEPTED,
            direction = "verified",
            "beta re-probe accepted by the upstream; recorded as verified",
        );
        meta.capability_observations.push(CapabilityObserveEvent {
            persistence_generation,
            incarnation,
            state_key: admission.learned_key.clone(),
            capability_key: admission.feature.clone(),
            provider_kind: admission.provider_kind.to_string(),
            evidence_class: BETA_ACCEPTED.to_string(),
            direction: ObservationDirection::Verified,
            signal_tier: SignalTier::SelfIdentifying,
            source: EvidenceSource::Live,
            request_features: super::field_repair::request_feature_keys(req),
        });
    }

    /// Mint a `beta:<flag>` negative for each flag the provider reported,
    /// unless the flag was not a client beta on this request, is not a
    /// well-formed flag, the target forwards the client's credential, the
    /// learning switch is off, or an operator `force_supported` override masks
    /// it. A held re-probe for the same key settles instead of minting.
    fn learn_reported_betas(
        &self,
        flags: &[String],
        target: &DispatchTarget,
        req: &ChatRequest,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
        probe_guard: &mut LearnedProbeGuard,
    ) {
        if flags.is_empty() || !self.config.capability.enabled || target.use_forwarded_credential {
            return;
        }
        let Some(provider_kind) = target.provider_kind else {
            return;
        };
        for flag in flags {
            if !req.anthropic_beta.iter().any(|sent| sent.trim() == flag) {
                continue;
            }
            let Some(feature_key) = beta_capability_key(flag) else {
                tracing::debug!(
                    state_key = %routectl_core::sanitize_for_log(&target.state_key),
                    flag = %routectl_core::sanitize_for_log(flag),
                    "reported beta flag is not a well-formed flag; not learned",
                );
                continue;
            };
            let Some(learned_key) = target.learned_key(&feature_key) else {
                return;
            };
            if self.override_forces_supported(target, &feature_key, provider_kind) {
                continue;
            }
            if self.settle_probe_rejection(
                learned_key,
                &feature_key,
                provider_kind,
                dedupe,
                probe_guard,
            ) {
                continue;
            }
            let mut request_features = super::field_repair::request_feature_keys(req);
            request_features.push(feature_key.clone());
            self.mint_learned_negative(
                LearnedMint {
                    learned_key: learned_key.to_string(),
                    feature_key,
                    state_key: &target.state_key,
                    provider_kind,
                    tier: SignalTier::SelfIdentifying,
                    phase: FailurePhase::F1,
                    upstream_status: BETA_REJECTION_STATUS,
                    upstream_code: None,
                    upstream_param: None,
                    remapped: false,
                    request_features,
                },
                dedupe,
                meta,
            );
        }
    }
}

#[cfg(all(test, feature = "bedrock"))]
#[path = "beta_report_learn_tests.rs"]
mod tests;
