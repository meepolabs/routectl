//! Per-target withheld client beta flags, decided once at chain resolution.
//!
//! Each target gets the set of client `anthropic-beta` flags it must not send.
//! Precedence, strongest first, per (target, flag):
//!
//! 1. operator floor pin -- the flag is re-added on the wire regardless, so
//!    nothing is withheld;
//! 2. operator override on the flag's capability key -- `force_supported`
//!    sends, `unsupported` withholds (a beta flag never routes a target away);
//! 3. learned lane verdict, only with the capability subsystem enabled -- an
//!    acting negative withholds, a lapsed one admits a re-probe and sends, a
//!    verified positive sends;
//! 4. the shipped Bedrock seed -- withholds, independent of the kill switch,
//!    unless the registry holds a seed-clear marker for the cell.
//!
//! A forwarded-credential target withholds nothing: the client owns that
//! credential and its beta choices.

use std::time::Instant;

use routectl_core::ChatRequest;
use routectl_core::capability::normalize_capability_key;

use crate::beta_capability::beta_capability_key;
use crate::learned_capability::RoutingDecision;
use crate::override_registry::OverrideVerdict;

use super::{DispatchTarget, ProbeAdmission, Router};

/// What the precedence chain decided for one flag on one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BetaVerdict {
    Send,
    Withhold,
    /// Undecided by every tier above the seed.
    Open,
}

impl Router {
    /// Attach each target's withheld client beta set. Admissions for lapsed
    /// learned negatives this pass claims are appended to `admissions`; every
    /// one must be settled by the dispatch path or its re-probe slot latches.
    pub(super) fn withhold_betas_on_chain(
        &self,
        chain: Vec<DispatchTarget>,
        req: &ChatRequest,
        admissions: &mut Vec<ProbeAdmission>,
    ) -> Vec<DispatchTarget> {
        if req.anthropic_beta.is_empty() {
            return chain;
        }
        let now = Instant::now();
        chain
            .into_iter()
            .map(|target| {
                let withheld = self.withheld_betas_for_target(&target, req, admissions, now);
                if withheld.is_empty() {
                    target
                } else {
                    DispatchTarget {
                        withheld_betas: std::sync::Arc::from(withheld),
                        ..target
                    }
                }
            })
            .collect()
    }

    /// The client beta flags `target` must not send, in request order and
    /// without duplicates.
    pub(super) fn withheld_betas_for_target(
        &self,
        target: &DispatchTarget,
        req: &ChatRequest,
        admissions: &mut Vec<ProbeAdmission>,
        now: Instant,
    ) -> Vec<String> {
        if target.use_forwarded_credential {
            return Vec::new();
        }
        let mut withheld: Vec<String> = Vec::new();
        for flag in &req.anthropic_beta {
            if withheld.contains(flag) {
                continue;
            }
            if self.beta_verdict(target, flag, admissions, now) == BetaVerdict::Withhold {
                withheld.push(flag.clone());
            }
        }
        withheld
    }

    /// Run the precedence chain for one flag, falling back to the seed when
    /// every stronger tier leaves the flag open.
    fn beta_verdict(
        &self,
        target: &DispatchTarget,
        flag: &str,
        admissions: &mut Vec<ProbeAdmission>,
        now: Instant,
    ) -> BetaVerdict {
        if self.beta_flag_pinned_for_target(target, flag) {
            return BetaVerdict::Send;
        }
        let Some(key) = beta_capability_key(flag) else {
            return BetaVerdict::Send;
        };
        match self.override_beta_verdict(target, &key) {
            BetaVerdict::Open => {}
            decided => return decided,
        }
        match self.learned_beta_verdict(target, &key, admissions, now) {
            BetaVerdict::Open => {}
            decided => return decided,
        }
        if self.seed_withholds(target, &key, flag) {
            BetaVerdict::Withhold
        } else {
            BetaVerdict::Send
        }
    }

    fn override_beta_verdict(&self, target: &DispatchTarget, key: &str) -> BetaVerdict {
        match self.override_registry.resolve(
            &target.provider_name,
            target.nickname.as_deref().unwrap_or(""),
            key,
            target.provider_kind.unwrap_or(""),
        ) {
            Some((OverrideVerdict::ForceSupported, _)) => BetaVerdict::Send,
            Some((OverrideVerdict::RouteAway, _)) => BetaVerdict::Withhold,
            None => BetaVerdict::Open,
        }
    }

    /// The learned lane's verdict on `key`. Claiming a lapsed negative's
    /// re-probe slot pushes its admission and sends the flag, so the full
    /// request re-tests it.
    fn learned_beta_verdict(
        &self,
        target: &DispatchTarget,
        key: &str,
        admissions: &mut Vec<ProbeAdmission>,
        now: Instant,
    ) -> BetaVerdict {
        if !self.config.capability.enabled {
            return BetaVerdict::Open;
        }
        let Some(provider_kind) = target.provider_kind else {
            return BetaVerdict::Open;
        };
        let Some(learned_key) = target.learned_key(key) else {
            return BetaVerdict::Open;
        };
        let (decision, generation) =
            self.acting_negative_with_generation(learned_key, key, provider_kind, now);
        match decision {
            RoutingDecision::RouteAway { .. } => BetaVerdict::Withhold,
            RoutingDecision::ProbeAdmitted => {
                self.metrics.incr_probe_attempts();
                admissions.push(ProbeAdmission {
                    state_key: target.state_key.clone(),
                    learned_key: learned_key.to_string(),
                    feature: normalize_capability_key(key, provider_kind),
                    provider_kind,
                    generation,
                });
                BetaVerdict::Send
            }
            RoutingDecision::Allow
                if self.is_verified_working_or_false(learned_key, key, provider_kind, now) =>
            {
                BetaVerdict::Send
            }
            RoutingDecision::Allow => BetaVerdict::Open,
        }
    }

    fn seed_withholds(&self, target: &DispatchTarget, key: &str, flag: &str) -> bool {
        target.provider_kind == Some(crate::beta_seed::BEDROCK_SEED_PROVIDER_KIND)
            && self.beta_seed.contains(&flag)
            && !self.seed_cleared_for_cell(target, key)
    }

    /// Whether the seed has been cleared for this target's (lane, flag) cell.
    /// A target with no lane or no kind has no cell to clear.
    fn seed_cleared_for_cell(&self, target: &DispatchTarget, key: &str) -> bool {
        let (Some(provider_kind), Some(learned_key)) =
            (target.provider_kind, target.learned_key(key))
        else {
            return false;
        };
        self.learned_capabilities
            .seed_cleared(learned_key, key, provider_kind)
    }

    /// Replace the Bedrock beta seed with a fixture list.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn set_beta_seed_for_tests(&mut self, seed: &'static [&'static str]) {
        self.beta_seed = seed;
        self.learned_capabilities
            .set_seed_scope(crate::beta_capability::BetaSeedScope::new(
                crate::beta_seed::BEDROCK_SEED_PROVIDER_KIND,
                seed,
            ));
    }
}

#[cfg(all(test, feature = "bedrock"))]
#[path = "beta_withhold_tests.rs"]
mod beta_withhold_tests;
