//! Learning from the beta flags a provider reports it stripped.
//!
//! A provider that drops a client `anthropic-beta` flag after the upstream
//! rejected it by name, and then succeeds without it, records the flag on the
//! per-attempt [`BetaRepairReport`]. The attempt succeeded, but the upstream
//! still refused that flag on this lane, so the router mints a `beta:<flag>`
//! negative for the target through the same probe-settle and mint path a 400
//! rejection takes.

use std::collections::HashSet;

use routectl_core::capability::{FailurePhase, SignalTier};
use routectl_core::{BetaRepairReport, ChatRequest};

use super::capability_learn::{LearnDedupeKey, LearnedMint};
use super::{DispatchMeta, DispatchTarget, LearnedProbeGuard, Router};
use crate::beta_capability::beta_capability_key;

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
    /// Settle a successful attempt: first every flag the provider reported
    /// stripping, as a rejection, then the target's held re-probes as
    /// successes.
    ///
    /// The order is load-bearing. A reported flag was rejected even though the
    /// attempt succeeded; settled after the success, an admitted re-probe for
    /// that flag would clear the very negative the upstream just re-confirmed.
    pub(super) fn settle_attempt_success(
        &self,
        beta_report: &BetaRepairReport,
        target: &DispatchTarget,
        req: &ChatRequest,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
        probe_guard: &mut LearnedProbeGuard,
    ) {
        self.learn_reported_betas(beta_report, target, req, dedupe, meta, probe_guard);
        meta.cleared_capabilities
            .extend(probe_guard.settle_success());
    }

    /// Mint a `beta:<flag>` negative for each flag the provider reported,
    /// unless the flag was not a client beta on this request, is not a
    /// well-formed flag, the target forwards the client's credential, the
    /// learning switch is off, or an operator `force_supported` override masks
    /// it. A held re-probe for the same key settles instead of minting.
    fn learn_reported_betas(
        &self,
        beta_report: &BetaRepairReport,
        target: &DispatchTarget,
        req: &ChatRequest,
        dedupe: &mut HashSet<LearnDedupeKey>,
        meta: &mut DispatchMeta,
        probe_guard: &mut LearnedProbeGuard,
    ) {
        let flags = beta_report.take();
        if flags.is_empty() || !self.config.capability.enabled || target.use_forwarded_credential {
            return;
        }
        let Some(provider_kind) = target.provider_kind else {
            return;
        };
        for flag in &flags {
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
