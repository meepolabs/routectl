//! Per-target withheld client beta flags, decided once at chain resolution.
//!
//! Each target gets the set of client `anthropic-beta` flags it must not send.
//! Each (target, flag) cell is decided by the capability precedence seam
//! ([`resolve_operator`], then [`resolve_open`]); this module only gathers the
//! cell's inputs (the operator floor pin, whether the lane can carry the beta
//! bag key at all, the override, the learned reading and the shipped seed)
//! and maps the seam's wire axis onto send or withhold.
//!
//! The learned reading is claiming: a lapsed negative whose re-probe slot this
//! pass claims sends the flag on an inference surface so the request re-tests
//! it. A token count carries no capability test, so there the lapsed negative
//! stays withheld and the slot stays free.
//!
//! A flag that mints no capability key reaches no keyed tier: only the floor
//! pin (matched by spelling) and the lane's bag-key refusal decide it.
//!
//! A forwarded-credential target withholds nothing: the client owns that
//! credential and its beta choices.

use std::time::Instant;

use routectl_core::ChatRequest;
use routectl_core::capability::normalize_capability_key;

use crate::beta_capability::beta_capability_key;
use crate::capability_precedence::{
    CellContext, LearnedInput, OpenCell, SeedCell, Stage, Wire, resolve_open, resolve_operator,
};
use crate::learned_capability::{BetaLaneReading, ProbeClaim};

use super::class_observe::DispatchSurface;
use super::{DispatchTarget, ProbeAdmission, Router};

/// What happens to one client flag on one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BetaVerdict {
    Send,
    Withhold,
}

impl BetaVerdict {
    /// The seam's wire axis as a beta decision: anything but a send keeps
    /// the flag off the wire.
    const fn from_wire(wire: Wire) -> Self {
        match wire {
            Wire::Send => Self::Send,
            Wire::Withhold | Wire::Strip => Self::Withhold,
        }
    }
}

impl Router {
    /// Attach each target's withheld client beta set. Admissions for lapsed
    /// learned negatives this pass claims are appended to `admissions`; every
    /// one must be settled by the dispatch path or its re-probe slot latches.
    pub(super) fn withhold_betas_on_chain(
        &self,
        chain: Vec<DispatchTarget>,
        req: &ChatRequest,
        surface: DispatchSurface,
        admissions: &mut Vec<ProbeAdmission>,
    ) -> Vec<DispatchTarget> {
        if req.anthropic_beta.is_empty() {
            return chain;
        }
        let claim = match surface {
            DispatchSurface::Complete | DispatchSurface::Stream => ProbeClaim::Claim,
            DispatchSurface::CountTokens => ProbeClaim::Forgo,
        };
        let now = Instant::now();
        chain
            .into_iter()
            .map(|target| {
                let withheld = self.withheld_betas_for_target(&target, req, claim, admissions, now);
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

    /// The client beta entries `target` must not send, trimmed as the
    /// dispatch overlay ships them, in request order and without duplicates.
    ///
    /// Each comma-separated piece of an entry is judged on its own; an entry
    /// is withheld whole when any piece is, since the egress drops entries,
    /// not pieces. A multi-piece entry never claims a re-probe slot.
    pub(super) fn withheld_betas_for_target(
        &self,
        target: &DispatchTarget,
        req: &ChatRequest,
        claim: ProbeClaim,
        admissions: &mut Vec<ProbeAdmission>,
        now: Instant,
    ) -> Vec<String> {
        if target.use_forwarded_credential {
            return Vec::new();
        }
        let mut withheld: Vec<String> = Vec::new();
        for entry in &req.anthropic_beta {
            let entry = entry.trim();
            if entry.is_empty() || withheld.iter().any(|w| w == entry) {
                continue;
            }
            let pieces: Vec<&str> = entry
                .split(',')
                .map(str::trim)
                .filter(|piece| !piece.is_empty())
                .collect();
            // A sibling piece can withhold the entry after a lapsed negative
            // claimed its re-probe slot, and that probe would never be sent.
            let claim = if pieces.len() > 1 {
                ProbeClaim::Forgo
            } else {
                claim
            };
            let withholds = pieces.iter().any(|piece| {
                self.beta_verdict(target, piece, claim, admissions, now) == BetaVerdict::Withhold
            });
            if withholds {
                withheld.push(entry.to_string());
            }
        }
        withheld
    }

    /// Gather one flag's inputs and resolve them through the precedence
    /// seam. The claiming learned read runs only for a cell the operator
    /// tiers left open.
    fn beta_verdict(
        &self,
        target: &DispatchTarget,
        flag: &str,
        claim: ProbeClaim,
        admissions: &mut Vec<ProbeAdmission>,
        now: Instant,
    ) -> BetaVerdict {
        let Some(key) = beta_capability_key(flag) else {
            return self.unkeyed_flag_verdict(target, flag);
        };
        let override_verdict = self.override_registry.resolve(
            &target.provider_name,
            target.nickname.as_deref().unwrap_or(""),
            &key,
            target.provider_kind.unwrap_or(""),
        );
        let ctx = CellContext::new(
            &key,
            &self.config.capability,
            target.egress_kind,
            &target.upstream,
        );
        let resolution = match resolve_operator(
            self.beta_pinned_for_target(target, &key),
            override_verdict,
            ctx,
        ) {
            Stage::Decided(decided) => decided,
            Stage::Open(cell) => {
                let learned = self.learned_beta_input(target, &key, cell, claim, admissions, now);
                resolve_open(learned, self.seed_cell(target, &key), None, cell)
            }
        };
        BetaVerdict::from_wire(resolution.wire())
    }

    /// A flag outside the key grammar has no override, learned or seed cell,
    /// but the operator floor can still pin it (floor entries are not checked
    /// against that grammar, so it is matched by spelling), and a lane that
    /// refuses the beta bag key cannot carry it.
    fn unkeyed_flag_verdict(&self, target: &DispatchTarget, flag: &str) -> BetaVerdict {
        let pinned = crate::capability_precedence::operator_pins_flag(
            &self.config,
            &target.provider_name,
            &target.model.header_extras,
            flag,
        );
        if pinned
            || !crate::beta_capability::refuses_beta_bag_key(target.egress_kind, &target.upstream)
        {
            BetaVerdict::Send
        } else {
            BetaVerdict::Withhold
        }
    }

    /// The claiming learned read for an open beta cell, or `None` when the
    /// learned tier cannot speak (kill switch off, no provider kind, no
    /// lane). Claiming a lapsed negative's re-probe slot pushes its
    /// admission; under [`ProbeClaim::Forgo`] the read reports the lapsed
    /// negative as acting and leaves the slot free.
    fn learned_beta_input(
        &self,
        target: &DispatchTarget,
        key: &str,
        cell: OpenCell,
        claim: ProbeClaim,
        admissions: &mut Vec<ProbeAdmission>,
        now: Instant,
    ) -> Option<LearnedInput> {
        if !cell.reads_learned() {
            return None;
        }
        let provider_kind = target.provider_kind?;
        let learned_key = target.learned_key(key)?;
        let (reading, generation) =
            self.beta_reading_with_generation(learned_key, key, provider_kind, claim, now);
        if let BetaLaneReading::ProbeAdmitted { .. } = reading {
            self.metrics.incr_probe_attempts();
            admissions.push(ProbeAdmission {
                state_key: target.state_key.clone(),
                learned_key: learned_key.to_string(),
                feature: normalize_capability_key(key, provider_kind),
                provider_kind,
                generation,
            });
        }
        LearnedInput::from_reading(reading)
    }

    /// The seed's state on this target's `(lane, key)` cell, under the
    /// router's seed list.
    fn seed_cell(&self, target: &DispatchTarget, key: &str) -> Option<SeedCell> {
        let scope = crate::beta_capability::BetaSeedScope::new(
            crate::beta_seed::BEDROCK_SEED_PROVIDER_KIND,
            self.beta_seed,
        );
        crate::beta_seed::seed_state_for_key(scope, target.provider_kind.unwrap_or(""), key, || {
            self.seed_cleared_for_cell(target, key)
        })
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

#[cfg(all(test, feature = "bedrock"))]
#[path = "beta_withhold_converse_tests.rs"]
mod beta_withhold_converse_tests;
