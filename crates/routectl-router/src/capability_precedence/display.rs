//! The display mode of the precedence resolution.
//!
//! [`resolve_display`] resolves one cell for a read-only surface (the doctor
//! capability matrix, the status surface) by walking the same tier list the
//! dispatch stages walk, then names the next tier that had an opinion and the
//! signal the surface renders. It is pure: it admits no probe, flips no
//! `in_flight` flag, and touches no metric.

use routectl_core::capability::{EvidenceSource, FailurePhase, Verdict};

use super::{
    CellContext, Layer, LearnedInput, PRECEDENCE, Resolution, SeedCell, no_opinion, opinion,
};
use crate::capability_display::{
    FORCED_SUPPORTED, FORCED_UNSUPPORTED, SOURCE_LIVE, SOURCE_OVERRIDE, SOURCE_PIN, SOURCE_PRIOR,
    SOURCE_PROBE, SOURCE_REPRESENTABILITY, SOURCE_SEED, VERDICT_UNREPRESENTABLE,
};
use crate::override_registry::OverrideVerdict;

/// Every tier's inputs for one cell, for the display mode
/// ([`resolve_display`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInputs {
    /// Whether an operator wire floor pins the capability
    /// ([`operator_pins_capability`](super::operator_pins_capability)).
    pub pinned: bool,
    /// The override registry's verdict for the cell.
    pub override_verdict: Option<OverrideVerdict>,
    /// The learned registry's snapshot view of the cell
    /// ([`LearnedInput::from_entry`]).
    pub learned: Option<LearnedInput>,
    /// The shipped beta seed's state on the cell.
    pub seed: Option<SeedCell>,
    /// The catalog prior for the cell.
    pub prior: Option<bool>,
    /// The cell's context ([`CellContext::new`] or [`CellContext::for_lane`]).
    pub ctx: CellContext,
}

/// What a display surface renders for a cell: the strongest tier holding a
/// signal, whether or not that signal acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Shown {
    pub(super) layer: Layer,
    pub(super) verdict: &'static str,
    pub(super) supported: bool,
    pub(super) source: &'static str,
}

/// Resolve a cell from snapshot reads and name the next tier below the
/// winner that had an opinion. The winner is the staged resolution's. The
/// display projections ([`Resolution::display_layer`] and its siblings) are
/// filled here too. Gather `pinned` with
/// [`operator_pins_capability`](super::operator_pins_capability),
/// `learned` with [`LearnedInput::from_entry`] and `ctx` with
/// [`CellContext::for_lane`].
pub const fn resolve_display(inputs: DisplayInputs) -> Resolution {
    let mut winner = None;
    let mut overridden = None;
    let mut shown = None;
    let mut i = 0;
    while i < PRECEDENCE.len() {
        let layer = PRECEDENCE[i];
        if shown.is_none() {
            shown = shown_signal(layer, inputs);
        }
        if overridden.is_none()
            && let Some(decided) = opinion(layer, inputs)
        {
            if winner.is_none() {
                winner = Some(decided);
            } else {
                overridden = Some(layer);
            }
        }
        i += 1;
    }
    let winner = match winner {
        Some(winner) => winner,
        None => no_opinion(inputs.pinned),
    };
    winner.with_display(overridden, shown)
}

/// The display-mode facts of a resolution. Filled by [`resolve_display`]
/// only; the dispatch stages leave them empty.
impl Resolution {
    /// This resolution with its display-mode facts: the next tier that had
    /// an opinion and the signal a display surface renders.
    const fn with_display(mut self, overridden: Option<Layer>, shown: Option<Shown>) -> Self {
        self.overridden = overridden;
        self.shown = shown;
        self
    }

    /// The layer a display surface renders: the strongest tier holding a
    /// signal on the cell, whether or not it acts. It differs from
    /// [`Self::layer`] where a signal is present but has no opinion: an
    /// advisory live F3 negative, an uncorroborated inferred one, or any
    /// learned or prior signal with the kill switch off all show `learned`
    /// (or `prior`) while the action comes from the tier that decided.
    /// Filled by the display mode only; `None` on the dispatch path and for a
    /// cell with no signal.
    pub const fn display_layer(&self) -> Option<Layer> {
        match self.shown {
            Some(shown) => Some(shown.layer),
            None => None,
        }
    }

    /// The display verdict token of [`Self::display_layer`]'s signal: the
    /// core verdict vocabulary for a learned, seed or prior signal, the
    /// panel-only override tokens (`forced_supported` / `forced_unsupported`),
    /// `forced_supported` for a pin and `unrepresentable` for a flag the lane
    /// cannot carry; `unknown` with no signal.
    pub const fn display_verdict(&self) -> &'static str {
        match self.shown {
            Some(shown) => shown.verdict,
            None => Verdict::Unknown.as_str(),
        }
    }

    /// The support polarity of [`Self::display_layer`]'s signal; `None` with
    /// no signal.
    pub const fn display_supported(&self) -> Option<bool> {
        match self.shown {
            Some(shown) => Some(shown.supported),
            None => None,
        }
    }

    /// The source tag of [`Self::display_layer`]'s signal: `pin`,
    /// `representability`, `override`, `live`, `probe`, `seed` or `prior`;
    /// `None` with no signal.
    pub const fn display_source(&self) -> Option<&'static str> {
        match self.shown {
            Some(shown) => Some(shown.source),
            None => None,
        }
    }
}

/// The signal `layer` holds on the cell, whether or not it acts: a resident
/// learned entry and a prior are shown with the kill switch off, and an
/// advisory or uncorroborated learned negative is shown like an acting one.
const fn shown_signal(layer: Layer, inputs: DisplayInputs) -> Option<Shown> {
    let ctx = inputs.ctx;
    match layer {
        Layer::Pin if inputs.pinned && ctx.beta_flag() => {
            Some(shown(Layer::Pin, FORCED_SUPPORTED, true, SOURCE_PIN))
        }
        Layer::Representability if !ctx.representable() => Some(shown(
            Layer::Representability,
            VERDICT_UNREPRESENTABLE,
            false,
            SOURCE_REPRESENTABILITY,
        )),
        Layer::Override => match inputs.override_verdict {
            Some(OverrideVerdict::RouteAway) => Some(shown(
                Layer::Override,
                FORCED_UNSUPPORTED,
                false,
                SOURCE_OVERRIDE,
            )),
            Some(OverrideVerdict::ForceSupported) => Some(shown(
                Layer::Override,
                FORCED_SUPPORTED,
                true,
                SOURCE_OVERRIDE,
            )),
            None => None,
        },
        Layer::Learned => match inputs.learned {
            Some(learned) => Some(shown_learned(learned)),
            None => None,
        },
        Layer::Seed => match inputs.seed {
            Some(SeedCell::Withhold) => Some(shown(Layer::Seed, BROKEN, false, SOURCE_SEED)),
            Some(SeedCell::Cleared) => Some(shown(
                Layer::Seed,
                Verdict::Cleared.as_str(),
                true,
                SOURCE_SEED,
            )),
            None => None,
        },
        Layer::Prior => match inputs.prior {
            Some(supported) => Some(shown(
                Layer::Prior,
                Verdict::Assumed(supported).as_str(),
                supported,
                SOURCE_PRIOR,
            )),
            None => None,
        },
        Layer::Pin | Layer::Representability => None,
    }
}

const fn shown_learned(learned: LearnedInput) -> Shown {
    let source = match learned.source() {
        EvidenceSource::Live => SOURCE_LIVE,
        EvidenceSource::Probe => SOURCE_PROBE,
    };
    match learned {
        LearnedInput::Verified { .. } => shown(
            Layer::Learned,
            Verdict::VerifiedWorking.as_str(),
            true,
            source,
        ),
        _ => shown(Layer::Learned, BROKEN, false, source),
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
