//! The one precedence resolution for a `(target, capability)` cell.
//!
//! Every surface that decides or shows what happens to a capability on a
//! target resolves it here, in the settled order
//! `pin > representability > override > learned > seed > prior` held once in
//! [`PRECEDENCE`]: the dispatch stages and the display mode both walk that
//! list, so no caller orders the tiers itself. The resolution is pure: it reads already-gathered inputs and never claims a
//! probe slot, flips `in_flight`, or touches a metric.
//!
//! It is staged so a dispatch caller never runs the claiming learned read
//! for a cell the operator already decided:
//!
//! 1. [`resolve_operator`] applies the operator tiers (the wire pin and the
//!    override registry) and the representability check between them. It
//!    returns [`Stage::Decided`] when they settle both axes, else
//!    [`Stage::Open`] carrying an [`OpenCell`].
//! 2. [`resolve_open`] applies the learned, seed and prior tiers to an
//!    [`OpenCell`]. Only the caller holding an `OpenCell` can reach it, so the
//!    lower tiers can never be resolved without the operator pass.
//!
//! The display mode (doctor, status; [`resolve_display`], in the `display`
//! submodule) gathers every tier from snapshot reads, returns the same winner,
//! names the next tier that had an opinion in [`Resolution::overridden`], and
//! picks the signal a read-only surface shows by walking the same list.
//!
//! A [`Resolution`] has two axes, because the beta path and the feature path
//! act on different ones: [`Wire`] (send, withhold or strip the capability)
//! and [`Routing`] (keep, demote to the tail, or drop the target). A wire pin
//! forces `Wire::Send`: on a beta key that decides the cell outright, and on a
//! feature key the routing is still decided by the tiers below with strip
//! ruled out, because a pinned token is re-added after any strip.
//!
//! The input gatherers live here too, so dispatch and display cannot gather
//! differently: [`operator_pins_capability`] is the one pin predicate and
//! [`CellContext::new`] the one context builder.

use std::collections::BTreeMap;
use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
use routectl_core::wire_check::EgressKind;

use crate::config::{CapabilityConfig, Config, ProviderEntry};
use crate::learned_capability::{BetaLaneReading, LearnedRegistryEntry, negative_is_advisory};
use crate::override_registry::OverrideVerdict;

mod display;

use display::Shown;
pub use display::{DisplayInputs, resolve_display};

/// Action token: an operator route-away override hard-drops the target.
pub const ACTION_DROP: &str = "drop";
/// Action token: the target is demoted to the tail of its chain (an acting
/// learned negative, or a catalog `prior=false`).
pub const ACTION_ROUTE_AWAY: &str = "route_away";
/// Action token: the capability is stripped from the request in place.
pub const ACTION_STRIP: &str = "strip";
/// Action token: the negative's decay window has lapsed, so the next request
/// carrying the capability re-verifies it on this target.
pub const ACTION_REPROBE: &str = "reprobe";
/// Action token: a positive signal; the target serves the capability.
pub const ACTION_ALLOW: &str = "allow";
/// Action token: the target stays in the chain but does not send the beta
/// flag (a beta-key negative, a beta-key override, or the shipped seed).
pub const ACTION_WITHHOLD: &str = "withhold";
/// Action token: no signal acts on routing for this cell.
pub const ACTION_NONE: &str = "none";
/// Action token: the nicknames sharing a lane resolve the cell to different
/// actions (one carries an override or a pinned beta the other does not), so
/// no single action describes it; the per-nickname actions ride alongside.
pub const ACTION_MIXED: &str = "mixed";

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

/// A precedence tier. Tiers order strongest first, by their position in the
/// private `PRECEDENCE` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// An operator wire floor pins the beta token.
    Pin,
    /// The lane cannot carry the beta flag at all, so it is withheld whatever
    /// the tiers below say.
    Representability,
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
            Self::Representability => "representability",
            Self::Override => "override",
            Self::Learned => "learned",
            Self::Seed => "seed",
            Self::Prior => "prior",
        }
    }

    /// Whether the operator stage ([`resolve_operator`]) applies this tier.
    const fn is_operator(self) -> bool {
        matches!(self, Self::Pin | Self::Representability | Self::Override)
    }

    /// This tier's position in [`PRECEDENCE`], strongest first.
    const fn rank(self) -> usize {
        let mut i = 0;
        while i < PRECEDENCE.len() {
            if PRECEDENCE[i] as usize == self as usize {
                return i;
            }
            i += 1;
        }
        panic!("a Layer is missing from PRECEDENCE")
    }
}

impl Ord for Layer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for Layer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The tiers, strongest first: the one ordering every resolution walks.
const PRECEDENCE: [Layer; 6] = [
    Layer::Pin,
    Layer::Representability,
    Layer::Override,
    Layer::Learned,
    Layer::Seed,
    Layer::Prior,
];

/// The staged dispatch path applies every operator tier before any other, so
/// the operator tiers must lead [`PRECEDENCE`] or dispatch and display would
/// rank differently.
const _: () = {
    let mut i = 0;
    let mut open_seen = false;
    while i < PRECEDENCE.len() {
        let operator = PRECEDENCE[i].is_operator();
        assert!(
            !(open_seen && operator),
            "an operator tier follows a lower tier in PRECEDENCE"
        );
        open_seen |= !operator;
        i += 1;
    }
};

/// The learned registry's view of one cell, as the resolution consumes it.
/// `None` stands for a cell with no resident entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnedInput {
    /// An acting negative inside its decay window. A live F3 negative is
    /// advisory: it carries no opinion, so the tiers below decide.
    Acting {
        /// The detection phase that attributed it.
        phase: FailurePhase,
        /// The signal tier of the negative.
        tier: SignalTier,
        /// Where the evidence came from.
        source: EvidenceSource,
    },
    /// A negative whose decay lapsed into a re-probe. A dispatch read that
    /// claimed the re-probe slot maps here.
    Lapsed {
        /// Where the negative's evidence came from.
        source: EvidenceSource,
    },
    /// A verified-working positive.
    Verified {
        /// Where the positive's evidence came from.
        source: EvidenceSource,
    },
    /// A resident negative that does not act yet: an inferred signal still
    /// waiting for corroboration. It has no opinion, but a display surface
    /// still shows it. A dispatch read never produces it.
    Uncorroborated {
        /// Where the evidence came from.
        source: EvidenceSource,
    },
}

impl LearnedInput {
    /// The input a dispatch read yields. `ProbeAdmitted` is a lapsed negative
    /// whose re-probe slot this read claimed; `ProbeFree` one whose free slot
    /// a non-claiming read left for the claiming read to take.
    pub(crate) const fn from_reading(reading: BetaLaneReading) -> Option<Self> {
        match reading {
            BetaLaneReading::ActingNegative {
                phase,
                tier,
                source,
            } => Some(Self::Acting {
                phase,
                tier,
                source,
            }),
            BetaLaneReading::ProbeAdmitted { source } | BetaLaneReading::ProbeFree { source } => {
                Some(Self::Lapsed { source })
            }
            BetaLaneReading::VerifiedPositive { source } => Some(Self::Verified { source }),
            BetaLaneReading::NoVerdict => None,
        }
    }

    /// The input a snapshot entry yields against `now`, for a display
    /// surface, by the rules the dispatch read applies. A live F3 negative is
    /// kept as `Acting` whatever its decay: the dispatch path never admits a
    /// re-probe for it. A positive that does not act (never minted today) has
    /// no input.
    pub fn from_entry(entry: &LearnedRegistryEntry, now: Instant) -> Option<Self> {
        let source = entry.source;
        let acts = crate::learned_capability::signal_acts(entry.signal_tier, entry.observations);
        match entry.verdict {
            Verdict::VerifiedWorking => acts.then_some(Self::Verified { source }),
            Verdict::LearnedBroken(_) if !acts => Some(Self::Uncorroborated { source }),
            Verdict::LearnedBroken(phase) => {
                let lapsed = !negative_is_advisory(phase, source)
                    && crate::learned_capability::negative_lapsed(
                        entry.expires_at,
                        entry.recheck_owned,
                        now,
                    );
                Some(if lapsed {
                    Self::Lapsed { source }
                } else {
                    Self::Acting {
                        phase,
                        tier: entry.signal_tier,
                        source,
                    }
                })
            }
            _ => None,
        }
    }

    pub(crate) const fn source(self) -> EvidenceSource {
        match self {
            Self::Acting { source, .. }
            | Self::Lapsed { source }
            | Self::Verified { source }
            | Self::Uncorroborated { source } => source,
        }
    }
}

/// Whether an input is the advisory live F3 kind, which is shown but routes
/// nothing until a probe settles it.
const fn is_advisory(learned: LearnedInput) -> bool {
    match learned {
        LearnedInput::Acting { phase, source, .. } => negative_is_advisory(phase, source),
        _ => false,
    }
}

/// The facts about a cell every tier reads. Built only by [`Self::new`] or
/// [`Self::for_lane`], so `beta_flag` and `representable` always agree with
/// the capability key and the lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellContext {
    /// The `[capability] enabled` kill switch. Off, the learned and prior
    /// tiers have no opinion; the operator tiers, representability and the
    /// seed still act.
    capability_enabled: bool,
    /// Whether the capability is a beta-flag key. A beta key acts on the wire
    /// only and never leaves `Routing::Keep`.
    beta_flag: bool,
    /// Whether the capability is a droppable the operator has not marked
    /// essential, so an F1 negative on it may be stripped in place.
    droppable: bool,
    /// Whether the lane's wire can carry the capability at all. False only
    /// for a beta flag on a lane that refuses the beta bag key; consulted
    /// after the pin and before every other tier.
    representable: bool,
}

impl CellContext {
    /// The context of `capability_key` on a lane sending with `egress_kind`
    /// to `upstream`, under the operator's `[capability]` table (the kill
    /// switch, and the `essential` list applied to the droppable check).
    pub fn new(
        capability_key: &str,
        capability: &CapabilityConfig,
        egress_kind: Option<EgressKind>,
        upstream: &str,
    ) -> Self {
        let beta_flag = crate::beta_capability::capability_key_is_beta(capability_key);
        Self {
            capability_enabled: capability.enabled,
            beta_flag,
            droppable: matches!(
                crate::capability_strip::effective_action_for(
                    capability_key,
                    &capability.essential
                ),
                crate::capability_strip::CapabilityAction::Strip(_)
            ),
            representable: !(beta_flag
                && crate::beta_capability::refuses_beta_bag_key(egress_kind, upstream)),
        }
    }

    /// [`Self::new`] for a lane named by its provider entry, reading the
    /// egress kind off the entry. An entry absent from `config` has no
    /// egress kind.
    pub fn for_lane(
        config: &Config,
        provider_entry: &str,
        upstream: &str,
        capability_key: &str,
    ) -> Self {
        Self::new(
            capability_key,
            &config.capability,
            config
                .providers
                .get(provider_entry)
                .map(ProviderEntry::egress_kind),
            upstream,
        )
    }

    /// Whether the capability is a beta-flag key.
    pub(crate) const fn beta_flag(self) -> bool {
        self.beta_flag
    }

    /// Whether the lane's wire can carry the capability at all.
    pub(crate) const fn representable(self) -> bool {
        self.representable
    }

    /// Whether the capability is a droppable the operator has not marked
    /// essential.
    pub(crate) const fn droppable(self) -> bool {
        self.droppable
    }

    /// A context with arbitrary facts, including combinations no lane
    /// produces, for exercising each tier in isolation.
    #[cfg(test)]
    #[allow(clippy::fn_params_excessive_bools)]
    pub(crate) const fn from_facts(
        capability_enabled: bool,
        beta_flag: bool,
        droppable: bool,
        representable: bool,
    ) -> Self {
        Self {
            capability_enabled,
            beta_flag,
            droppable,
            representable,
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
    shown: Option<Shown>,
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
            shown: None,
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

    /// The next tier below the winner that had an opinion. Filled by the
    /// display mode only; always `None` on the dispatch path.
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
        matches!(self.learned, Some(LearnedInput::Lapsed { .. }))
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

/// Apply the operator tiers: the wire pin, then representability (a flag
/// the lane cannot carry is withheld unless pinned), then the override
/// registry's verdict (which carries `force_supported` as
/// [`OverrideVerdict::ForceSupported`]). The `ctx` is needed because a
/// route-away override withholds a beta flag but drops a feature target.
pub const fn resolve_operator(
    pinned: bool,
    override_verdict: Option<OverrideVerdict>,
    ctx: CellContext,
) -> Stage {
    let inputs = DisplayInputs {
        pinned,
        override_verdict,
        learned: None,
        seed: None,
        prior: None,
        ctx,
    };
    match first_opinion(inputs, true) {
        Some(decided) => Stage::Decided(decided),
        None => Stage::Open(OpenCell { pinned, ctx }),
    }
}

/// Apply the learned, seed and prior tiers to a cell the operator left open.
pub const fn resolve_open(
    learned: Option<LearnedInput>,
    seed: Option<SeedCell>,
    prior: Option<bool>,
    cell: OpenCell,
) -> Resolution {
    let inputs = DisplayInputs {
        pinned: cell.pinned,
        override_verdict: None,
        learned,
        seed,
        prior,
        ctx: cell.ctx,
    };
    match first_opinion(inputs, false) {
        Some(decided) => decided,
        None => no_opinion(cell.pinned),
    }
}

/// The opinion of the first tier in [`PRECEDENCE`] that has one, among the
/// operator tiers (`operator`) or the lower ones.
const fn first_opinion(inputs: DisplayInputs, operator: bool) -> Option<Resolution> {
    let mut i = 0;
    while i < PRECEDENCE.len() {
        let layer = PRECEDENCE[i];
        if layer.is_operator() == operator
            && let Some(decided) = opinion(layer, inputs)
        {
            return Some(decided);
        }
        i += 1;
    }
    None
}

/// One tier's opinion on a cell; `None` when the tier has none.
const fn opinion(layer: Layer, inputs: DisplayInputs) -> Option<Resolution> {
    let cell = OpenCell {
        pinned: inputs.pinned,
        ctx: inputs.ctx,
    };
    match layer {
        Layer::Pin => pin_opinion(inputs.pinned, inputs.ctx),
        Layer::Representability => representability_opinion(inputs.pinned, inputs.ctx),
        Layer::Override => override_opinion(inputs.pinned, inputs.override_verdict, inputs.ctx),
        Layer::Learned => learned_opinion(inputs.learned, cell),
        Layer::Seed => seed_opinion(inputs.seed, cell),
        Layer::Prior => prior_opinion(inputs.prior, cell),
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

/// A flag the lane cannot carry is withheld. A pin beats it: the operator
/// floor re-adds the flag on the wire whatever the router decides.
const fn representability_opinion(pinned: bool, ctx: CellContext) -> Option<Resolution> {
    if ctx.representable || (pinned && ctx.beta_flag) {
        None
    } else {
        Some(Resolution::new(
            Wire::Withhold,
            Routing::Keep,
            Layer::Representability,
            Some(false),
            pinned,
        ))
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
        LearnedInput::Uncorroborated { .. } => return None,
        LearnedInput::Verified { .. } => (Wire::Send, Routing::Keep, true),
        LearnedInput::Lapsed { .. } => (Wire::Send, Routing::Keep, false),
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
        shown: None,
    }
}

/// Whether an operator wire floor pins `capability_key` on a target of
/// `provider_entry` whose model carries `model_header_extras` (an empty map
/// for a lane no model maps). A beta key is pinned when its flag is; a
/// feature key when any beta token its strip removes is, because a pinned
/// token is re-added on the wire after the strip. A key with no beta surface
/// is never pinned.
pub fn operator_pins_capability(
    config: &Config,
    provider_entry: &str,
    model_header_extras: &BTreeMap<String, String>,
    capability_key: &str,
) -> bool {
    match crate::beta_capability::beta_flag_of(capability_key) {
        Some(flag) => operator_pins_flag(config, provider_entry, model_header_extras, flag),
        None => crate::capability_strip::strip_beta_tokens(capability_key)
            .iter()
            .any(|token| operator_pins_flag(config, provider_entry, model_header_extras, token)),
    }
}

/// Whether the operator floor pins the beta `flag`: the provider entry's
/// `anthropic_beta` floor, or a provider or model `header_extras`
/// `anthropic-beta` contribution. The floor walk under
/// [`operator_pins_capability`]; a caller holding a capability key asks
/// that instead. Asking with the flag itself is needed only for a flag
/// outside the beta capability key grammar: the floor can still pin it,
/// because floor entries are not checked against that grammar, but no
/// capability key names it.
pub fn operator_pins_flag(
    config: &Config,
    provider_entry: &str,
    model_header_extras: &BTreeMap<String, String>,
    flag: &str,
) -> bool {
    let entry = config.providers.get(provider_entry);
    entry
        .map_or(&[][..], ProviderEntry::anthropic_beta_floor)
        .iter()
        .any(|pinned| pinned == flag)
        || crate::beta_capability::operator_betas(
            entry.map(ProviderEntry::header_extras),
            model_header_extras,
        )
        .iter()
        .any(|pinned| pinned == flag)
}

#[cfg(test)]
#[path = "capability_precedence_tests.rs"]
mod capability_precedence_tests;
