//! The one precedence resolution for a `(target, capability)` cell.
//!
//! Every surface that decides or shows what happens to a capability on a
//! target resolves it here, in the settled order
//! `pin > override > learned > seed > prior`. The resolution is pure: it
//! reads already-gathered inputs and never claims a probe slot, flips
//! `in_flight`, or touches a metric.
//!
//! It is staged so a dispatch caller never runs the claiming learned read
//! for a cell the operator already decided:
//!
//! 1. [`resolve_operator`] applies the operator tiers (the wire pin and the
//!    override registry). It returns [`Stage::Decided`] when they settle
//!    both axes, else [`Stage::Open`] carrying an [`OpenCell`].
//! 2. [`resolve_open`] applies the learned, seed and prior tiers to an
//!    [`OpenCell`]. Only the caller holding an `OpenCell` can reach it, so the
//!    lower tiers can never be resolved without the operator pass.
//!
//! [`resolve_display`] is the display mode (doctor, status): it gathers every
//! tier from snapshot reads, returns the same winner, and also names the next
//! tier that had an opinion in [`Resolution::overridden`].
//!
//! A [`Resolution`] has two axes, because the beta path and the feature path
//! act on different ones: [`Wire`] (send, withhold or strip the capability)
//! and [`Routing`] (keep, demote to the tail, or drop the target). A wire pin
//! forces `Wire::Send`: on a beta key that decides the cell outright, and on a
//! feature key the routing is still decided by the tiers below with strip
//! ruled out, because a pinned token is re-added after any strip.

use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
use routectl_core::wire_check::EgressKind;
use routectl_providers::anthropic_family::{AnthropicFamily, anthropic_family};

use crate::capability_display::{
    ACTION_ALLOW, ACTION_DROP, ACTION_NONE, ACTION_REPROBE, ACTION_ROUTE_AWAY, ACTION_STRIP,
    ACTION_WITHHOLD,
};
use crate::learned_capability::{BetaLaneReading, LearnedRegistryEntry};
use crate::override_registry::OverrideVerdict;

/// The shipped beta seed's state on one cell it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedCell {
    /// The seed withholds the flag on the lane.
    Withhold,
    /// A seed-clear marker lifts the seed on the lane, so the flag is sent.
    Cleared,
}

/// What happens to the capability on the wire to this target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// The capability is sent as the client asked.
    Send,
    /// The beta flag is not sent; the target stays in the chain.
    Withhold,
    /// The capability is stripped from the request in place.
    Strip,
}

/// What happens to the target in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    /// The target keeps its place.
    Keep,
    /// The target is demoted to the tail of its chain.
    Tail,
    /// The target is dropped from the chain.
    Drop,
}

/// A precedence tier, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// An operator wire floor pins the beta token.
    Pin,
    /// An operator `[capability.overrides]` entry.
    Override,
    /// The learned capability registry.
    Learned,
    /// The shipped beta seed.
    Seed,
    /// A catalog capability prior.
    Prior,
}

impl Layer {
    /// The stable token the doctor matrix and the status surface render.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Override => "override",
            Self::Learned => "learned",
            Self::Seed => "seed",
            Self::Prior => "prior",
        }
    }
}

/// The learned registry's view of one cell, as the resolution consumes it.
///
/// A caller passes `None` for a cell with no entry and for an entry that does
/// not act (an uncorroborated inferred negative); the resolution does not
/// re-derive whether an entry acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnedInput {
    /// An acting negative inside its decay window.
    Acting {
        /// The detection phase that attributed it.
        phase: FailurePhase,
        /// The signal tier of the negative.
        tier: SignalTier,
        /// Where the evidence came from, or `None` when the read that
        /// produced the input already applied the advisory rule for a live
        /// F3 negative and does not carry the source.
        source: Option<EvidenceSource>,
    },
    /// A negative whose decay lapsed into a re-probe. A dispatch read that
    /// claimed the re-probe slot maps here.
    Lapsed,
    /// A verified-working positive.
    Verified,
}

impl LearnedInput {
    /// The input a dispatch read yields. `ProbeAdmitted` is a lapsed negative
    /// whose re-probe slot this read claimed. The registry maps a live F3
    /// negative to `NoVerdict` before the read returns, so an acting reading
    /// never needs its source to be resolved.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn from_reading(reading: BetaLaneReading) -> Option<Self> {
        match reading {
            BetaLaneReading::ActingNegative { phase, tier } => Some(Self::Acting {
                phase,
                tier,
                source: None,
            }),
            BetaLaneReading::ProbeAdmitted => Some(Self::Lapsed),
            BetaLaneReading::VerifiedPositive => Some(Self::Verified),
            BetaLaneReading::NoVerdict => None,
        }
    }

    /// The input a snapshot entry yields against `now`, for a display
    /// surface. `None` for an entry that does not act. A live F3 negative is
    /// kept as `Acting` whatever its decay, so it carries no opinion rather
    /// than reading as a re-probe the dispatch path would never admit.
    pub fn from_entry(entry: &LearnedRegistryEntry, now: Instant) -> Option<Self> {
        if !crate::learned_capability::signal_acts(entry.signal_tier, entry.observations) {
            return None;
        }
        let Verdict::LearnedBroken(phase) = entry.verdict else {
            return matches!(entry.verdict, Verdict::VerifiedWorking).then_some(Self::Verified);
        };
        let acting = Self::Acting {
            phase,
            tier: entry.signal_tier,
            source: Some(entry.source),
        };
        if is_advisory(acting) {
            return Some(acting);
        }
        if crate::learned_capability::negative_lapsed(entry.expires_at, entry.recheck_owned, now) {
            Some(Self::Lapsed)
        } else {
            Some(acting)
        }
    }
}

/// Whether an acting negative is the advisory live F3 kind, which is shown
/// but routes nothing until a probe settles it.
const fn is_advisory(learned: LearnedInput) -> bool {
    matches!(
        learned,
        LearnedInput::Acting {
            phase: FailurePhase::F3,
            source: Some(EvidenceSource::Live),
            ..
        }
    )
}

/// The facts about a cell every tier reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellContext {
    /// The `[capability] enabled` kill switch. Off, the learned and prior
    /// tiers have no opinion; the operator tiers and the seed still act.
    pub capability_enabled: bool,
    /// Whether the capability is a beta-flag key. A beta key acts on the wire
    /// only and never leaves `Routing::Keep`.
    pub beta_flag: bool,
    /// Whether the capability is a droppable the operator has not marked
    /// essential, so an F1 negative on it may be stripped in place.
    pub droppable: bool,
}

impl CellContext {
    /// The context of `capability_key`, with the operator's
    /// `[capability] essential` list applied to the droppable check.
    pub fn new(capability_key: &str, essential: &[String], capability_enabled: bool) -> Self {
        Self {
            capability_enabled,
            beta_flag: crate::beta_capability::capability_key_is_beta(capability_key),
            droppable: matches!(
                crate::capability_strip::effective_action_for(capability_key, essential),
                crate::capability_strip::CapabilityAction::Strip(_)
            ),
        }
    }
}

/// The resolution of one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    wire: Wire,
    routing: Routing,
    layer: Option<Layer>,
    overridden: Option<Layer>,
    supported: Option<bool>,
    learned: Option<LearnedInput>,
    pinned: bool,
}

impl Resolution {
    const fn new(
        wire: Wire,
        routing: Routing,
        layer: Layer,
        supported: Option<bool>,
        pinned: bool,
    ) -> Self {
        Self {
            wire,
            routing,
            layer: Some(layer),
            overridden: None,
            supported,
            learned: None,
            pinned,
        }
    }

    /// The wire axis.
    pub const fn wire(&self) -> Wire {
        self.wire
    }

    /// The routing axis.
    pub const fn routing(&self) -> Routing {
        self.routing
    }

    /// The tier that decided the cell, or `None` when no tier had an opinion.
    /// On a pinned feature key the pin only rules out strip, so the layer is
    /// the tier that decided the routing.
    pub const fn layer(&self) -> Option<Layer> {
        self.layer
    }

    /// The next tier below the winner that had an opinion. Filled by
    /// [`resolve_display`] only; always `None` on the dispatch path.
    pub const fn overridden(&self) -> Option<Layer> {
        self.overridden
    }

    /// Whether the winning tier asserts the capability works on the target;
    /// `None` when no tier had an opinion.
    pub const fn supported(&self) -> Option<bool> {
        self.supported
    }

    /// The learned input that decided the cell, when the learned tier won.
    pub const fn learned(&self) -> Option<LearnedInput> {
        self.learned
    }

    /// Whether an operator wire floor pins the capability on the target.
    pub const fn pinned(&self) -> bool {
        self.pinned
    }

    /// Whether the learned tier admitted a re-probe: the target is kept and
    /// the capability sent so the request re-tests it.
    pub const fn reprobe(&self) -> bool {
        matches!(self.learned, Some(LearnedInput::Lapsed))
    }

    /// The cell as one display action token.
    pub const fn as_str(&self) -> &'static str {
        match (self.routing, self.wire) {
            (Routing::Drop, _) => ACTION_DROP,
            (Routing::Tail, _) => ACTION_ROUTE_AWAY,
            (Routing::Keep, Wire::Strip) => ACTION_STRIP,
            (Routing::Keep, Wire::Withhold) => ACTION_WITHHOLD,
            (Routing::Keep, Wire::Send) if self.reprobe() => ACTION_REPROBE,
            (Routing::Keep, Wire::Send) => match (self.layer, self.supported) {
                (Some(Layer::Prior), _) | (_, None | Some(false)) => ACTION_NONE,
                (_, Some(true)) => ACTION_ALLOW,
            },
        }
    }

    /// Whether this cell counts toward excluding the capability from every
    /// lane: an acting self-identifying F1 or F2 learned negative whose
    /// routing is a tail demotion after the essential tightening, on a cell
    /// the operator does not pin. An operator override (`force_supported`
    /// included) wins at its own layer, so it never counts. A lapsed
    /// negative, an inferred one (even corroborated), and a strip-eligible one
    /// never count.
    pub const fn hard_exclusion_eligible(&self) -> bool {
        matches!(self.layer, Some(Layer::Learned))
            && matches!(
                self.learned,
                Some(LearnedInput::Acting {
                    phase: FailurePhase::F1 | FailurePhase::F2,
                    tier: SignalTier::SelfIdentifying,
                    ..
                })
            )
            && matches!(self.routing, Routing::Tail)
            && !self.pinned
    }
}

/// The lower tiers' view of a cell the operator tiers left open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenCell {
    pinned: bool,
    ctx: CellContext,
}

impl OpenCell {
    /// Whether the learned tier can have an opinion. False with the kill
    /// switch off, so the caller skips the claiming learned read entirely.
    pub const fn reads_learned(&self) -> bool {
        self.ctx.capability_enabled
    }

    /// Whether an operator wire floor pins the capability; strip is then
    /// ruled out for every lower tier.
    pub const fn pinned(&self) -> bool {
        self.pinned
    }
}

/// The result of the operator stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The operator tiers settled both axes; no lower tier is read.
    Decided(Resolution),
    /// The lower tiers decide; pass the cell to [`resolve_open`].
    Open(OpenCell),
}

/// Apply the operator tiers: the wire pin, then the override registry's
/// verdict (which carries `force_supported` as
/// [`OverrideVerdict::ForceSupported`]). The `ctx` is needed because a
/// route-away override withholds a beta flag but drops a feature target.
pub const fn resolve_operator(
    pinned: bool,
    override_verdict: Option<OverrideVerdict>,
    ctx: CellContext,
) -> Stage {
    match pin_opinion(pinned, ctx) {
        Some(decided) => Stage::Decided(decided),
        None => match override_opinion(pinned, override_verdict, ctx) {
            Some(decided) => Stage::Decided(decided),
            None => Stage::Open(OpenCell { pinned, ctx }),
        },
    }
}

/// Apply the learned, seed and prior tiers to a cell the operator left open.
pub const fn resolve_open(
    learned: Option<LearnedInput>,
    seed: Option<SeedCell>,
    prior: Option<bool>,
    cell: OpenCell,
) -> Resolution {
    if let Some(decided) = learned_opinion(learned, cell) {
        return decided;
    }
    if let Some(decided) = seed_opinion(seed, cell) {
        return decided;
    }
    if let Some(decided) = prior_opinion(prior, cell) {
        return decided;
    }
    no_opinion(cell.pinned)
}

/// Every tier's inputs for one cell, for [`resolve_display`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInputs {
    /// Whether an operator wire floor pins the capability.
    pub pinned: bool,
    /// The override registry's verdict for the cell.
    pub override_verdict: Option<OverrideVerdict>,
    /// The learned registry's snapshot view of the cell.
    pub learned: Option<LearnedInput>,
    /// The shipped beta seed's state on the cell.
    pub seed: Option<SeedCell>,
    /// The catalog prior for the cell.
    pub prior: Option<bool>,
    /// The cell's context.
    pub ctx: CellContext,
}

/// Resolve a cell from snapshot reads and name the next tier below the
/// winner that had an opinion. The winner is the staged resolution's.
pub const fn resolve_display(inputs: DisplayInputs) -> Resolution {
    let cell = OpenCell {
        pinned: inputs.pinned,
        ctx: inputs.ctx,
    };
    let opinions = [
        pin_opinion(inputs.pinned, inputs.ctx),
        override_opinion(inputs.pinned, inputs.override_verdict, inputs.ctx),
        learned_opinion(inputs.learned, cell),
        seed_opinion(inputs.seed, cell),
        prior_opinion(inputs.prior, cell),
    ];
    let mut winner: Option<Resolution> = None;
    let mut index = 0;
    while index < opinions.len() {
        if let Some(opinion) = opinions[index] {
            match winner {
                None => winner = Some(opinion),
                Some(mut won) => {
                    won.overridden = opinion.layer;
                    return won;
                }
            }
        }
        index += 1;
    }
    match winner {
        Some(won) => won,
        None => no_opinion(inputs.pinned),
    }
}

/// A pin decides a beta key outright; on a feature key it only rules out
/// strip and has no opinion of its own.
const fn pin_opinion(pinned: bool, ctx: CellContext) -> Option<Resolution> {
    if pinned && ctx.beta_flag {
        Some(Resolution::new(
            Wire::Send,
            Routing::Keep,
            Layer::Pin,
            Some(true),
            true,
        ))
    } else {
        None
    }
}

const fn override_opinion(
    pinned: bool,
    verdict: Option<OverrideVerdict>,
    ctx: CellContext,
) -> Option<Resolution> {
    match verdict {
        None => None,
        Some(OverrideVerdict::ForceSupported) => Some(Resolution::new(
            Wire::Send,
            Routing::Keep,
            Layer::Override,
            Some(true),
            pinned,
        )),
        Some(OverrideVerdict::RouteAway) if ctx.beta_flag => Some(Resolution::new(
            Wire::Withhold,
            Routing::Keep,
            Layer::Override,
            Some(false),
            pinned,
        )),
        Some(OverrideVerdict::RouteAway) => Some(Resolution::new(
            Wire::Send,
            Routing::Drop,
            Layer::Override,
            Some(false),
            pinned,
        )),
    }
}

const fn learned_opinion(learned: Option<LearnedInput>, cell: OpenCell) -> Option<Resolution> {
    let Some(input) = learned else {
        return None;
    };
    if !cell.ctx.capability_enabled || is_advisory(input) {
        return None;
    }
    let (wire, routing, supported) = match input {
        LearnedInput::Verified => (Wire::Send, Routing::Keep, true),
        LearnedInput::Lapsed => (Wire::Send, Routing::Keep, false),
        LearnedInput::Acting { .. } if cell.ctx.beta_flag => (Wire::Withhold, Routing::Keep, false),
        LearnedInput::Acting {
            phase: FailurePhase::F1,
            ..
        } if cell.ctx.droppable && !cell.pinned => (Wire::Strip, Routing::Keep, false),
        LearnedInput::Acting { .. } => (Wire::Send, Routing::Tail, false),
    };
    let mut decided = Resolution::new(wire, routing, Layer::Learned, Some(supported), cell.pinned);
    decided.learned = Some(input);
    Some(decided)
}

const fn seed_opinion(seed: Option<SeedCell>, cell: OpenCell) -> Option<Resolution> {
    match seed {
        None => None,
        Some(SeedCell::Withhold) => Some(Resolution::new(
            Wire::Withhold,
            Routing::Keep,
            Layer::Seed,
            Some(false),
            cell.pinned,
        )),
        Some(SeedCell::Cleared) => Some(Resolution::new(
            Wire::Send,
            Routing::Keep,
            Layer::Seed,
            Some(true),
            cell.pinned,
        )),
    }
}

/// A catalog prior speaks for feature keys only: the dispatch beta path has
/// no prior tier, so a prior on a beta key has no opinion.
const fn prior_opinion(prior: Option<bool>, cell: OpenCell) -> Option<Resolution> {
    if !cell.ctx.capability_enabled || cell.ctx.beta_flag {
        return None;
    }
    match prior {
        None => None,
        Some(false) => Some(Resolution::new(
            Wire::Send,
            Routing::Tail,
            Layer::Prior,
            Some(false),
            cell.pinned,
        )),
        Some(true) => Some(Resolution::new(
            Wire::Send,
            Routing::Keep,
            Layer::Prior,
            Some(true),
            cell.pinned,
        )),
    }
}

const fn no_opinion(pinned: bool) -> Resolution {
    Resolution {
        wire: Wire::Send,
        routing: Routing::Keep,
        layer: None,
        overridden: None,
        supported: None,
        learned: None,
        pinned,
    }
}

/// Whether a lane can carry no client beta flag at all: a Bedrock Converse
/// lane to a model that refuses the `anthropic_beta` bag key. An upstream that
/// proves neither Anthropic nor not-Anthropic (an inference-profile ARN) does
/// not refuse. A representability check, applied before the precedence
/// resolution: no tier can make an unrepresentable flag ride the wire.
pub fn refuses_beta_bag_key(egress_kind: Option<EgressKind>, upstream: &str) -> bool {
    egress_kind == Some(EgressKind::BedrockConverse)
        && anthropic_family(upstream) == AnthropicFamily::No
}

#[cfg(test)]
#[path = "capability_precedence_tests.rs"]
mod capability_precedence_tests;
