//! The display mode of the capability precedence resolution.
//!
//! [`resolve_display`] resolves one cell for a read-only surface (the doctor
//! capability matrix, the status surface) through the same tier opinions the
//! dispatch path uses, then names the next tier that had an opinion and the
//! signal the surface renders. It is pure: it admits no probe, flips no
//! `in_flight` flag, and touches no metric.

use routectl_core::capability::{EvidenceSource, FailurePhase, Verdict};

use crate::capability_precedence::{
    DisplayInputs, Layer, LearnedInput, Resolution, SeedCell, Shown, no_opinion, opinions,
};
use crate::override_registry::OverrideVerdict;

/// Display verdict token for an operator route-away override cell. A
/// PANEL-ONLY token, distinct from the core [`Verdict`] vocabulary: an
/// override is an operator assertion, not a learned or catalog signal,
/// and the core verdict enum is a versioned, internal ledger contract that
/// must not grow display-only states.
pub const FORCED_UNSUPPORTED: &str = "forced_unsupported";

/// Display verdict token for an operator force-supported override cell.
/// PANEL-ONLY -- see [`FORCED_UNSUPPORTED`].
pub const FORCED_SUPPORTED: &str = "forced_supported";

/// Display verdict token for a matrix cell whose nicknames resolve to
/// different verdicts (one carries a nickname-scoped override the other does
/// not), so no lane-wide verdict, source or layer describes it. PANEL-ONLY,
/// like the override tokens: never part of the core verdict vocabulary.
pub const VERDICT_MIXED: &str = "mixed";

/// Source tag: an operator override decided the cell.
pub const SOURCE_OVERRIDE: &str = "override";
/// Source tag: a learned observation from live traffic.
pub const SOURCE_LIVE: &str = "live";
/// Source tag: a learned observation from an out-of-band probe.
pub const SOURCE_PROBE: &str = "probe";
/// Source tag: a catalog capability prior.
pub const SOURCE_PRIOR: &str = "prior";
/// Source tag: the shipped beta seed.
pub const SOURCE_SEED: &str = "seed";

/// Display verdict token for a beta flag the lane cannot carry at all.
/// PANEL-ONLY, like the override tokens.
pub const VERDICT_UNREPRESENTABLE: &str = "unrepresentable";

/// Source tag: an operator wire floor pins the capability.
pub const SOURCE_PIN: &str = "pin";
/// Source tag: the lane cannot represent the capability on its wire.
pub const SOURCE_REPRESENTABILITY: &str = "representability";

/// Resolve a cell from snapshot reads and name the next tier below the
/// winner that had an opinion. The winner is the staged resolution's. The
/// display projections ([`Resolution::display_layer`] and its siblings) are
/// filled here too. Gather `pinned` with
/// [`operator_pins_capability`](crate::capability_precedence::operator_pins_capability),
/// `learned` with [`LearnedInput::from_entry`] and `ctx` with
/// [`CellContext::for_lane`](crate::capability_precedence::CellContext::for_lane).
pub fn resolve_display(inputs: DisplayInputs) -> Resolution {
    let mut ranked = opinions(inputs).into_iter().flatten();
    let winner = ranked.next().unwrap_or_else(|| no_opinion(inputs.pinned));
    let overridden = ranked.next().and_then(|next| next.layer());
    winner.with_display(overridden, shown_signal(inputs))
}

/// The strongest tier holding a signal on the cell, in precedence order, but
/// ignoring whether the signal acts: a resident learned entry and a prior are
/// shown with the kill switch off, and an advisory or uncorroborated learned
/// negative is shown over the seed and the prior.
const fn shown_signal(inputs: DisplayInputs) -> Option<Shown> {
    let ctx = inputs.ctx;
    if inputs.pinned && ctx.beta_flag() {
        return Some(shown(Layer::Pin, FORCED_SUPPORTED, true, SOURCE_PIN));
    }
    if !ctx.representable() {
        return Some(shown(
            Layer::Representability,
            VERDICT_UNREPRESENTABLE,
            false,
            SOURCE_REPRESENTABILITY,
        ));
    }
    match inputs.override_verdict {
        Some(OverrideVerdict::RouteAway) => {
            return Some(shown(
                Layer::Override,
                FORCED_UNSUPPORTED,
                false,
                SOURCE_OVERRIDE,
            ));
        }
        Some(OverrideVerdict::ForceSupported) => {
            return Some(shown(
                Layer::Override,
                FORCED_SUPPORTED,
                true,
                SOURCE_OVERRIDE,
            ));
        }
        None => {}
    }
    if let Some(learned) = inputs.learned {
        let source = match learned.source() {
            EvidenceSource::Live => SOURCE_LIVE,
            EvidenceSource::Probe => SOURCE_PROBE,
        };
        return Some(match learned {
            LearnedInput::Verified { .. } => shown(
                Layer::Learned,
                Verdict::VerifiedWorking.as_str(),
                true,
                source,
            ),
            _ => shown(Layer::Learned, BROKEN, false, source),
        });
    }
    match inputs.seed {
        Some(SeedCell::Withhold) => return Some(shown(Layer::Seed, BROKEN, false, SOURCE_SEED)),
        Some(SeedCell::Cleared) => {
            return Some(shown(
                Layer::Seed,
                Verdict::Cleared.as_str(),
                true,
                SOURCE_SEED,
            ));
        }
        None => {}
    }
    match inputs.prior {
        Some(supported) => Some(shown(
            Layer::Prior,
            Verdict::Assumed(supported).as_str(),
            supported,
            SOURCE_PRIOR,
        )),
        None => None,
    }
}

/// The display verdict token of a negative signal.
const BROKEN: &str = Verdict::LearnedBroken(FailurePhase::F1).as_str();

const fn shown(
    layer: Layer,
    verdict: &'static str,
    supported: bool,
    source: &'static str,
) -> Shown {
    Shown {
        layer,
        verdict,
        supported,
        source,
    }
}

#[cfg(test)]
#[path = "capability_display_tests.rs"]
mod capability_display_tests;
