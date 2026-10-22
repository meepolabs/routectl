//! The display mode of the capability precedence resolution, plus the
//! previous display resolvers it replaces.
//!
//! [`resolve_display`] resolves one cell for a read-only surface (the doctor
//! capability matrix, the status surface) through the same tier opinions the
//! dispatch path uses, then names the next tier that had an opinion and the
//! signal the surface renders. It is pure: it admits no probe, flips no
//! `in_flight` flag, and touches no metric.
//!
//! [`resolve_display_verdict`] and [`resolve_display_action`] are the
//! previous display resolvers, kept until every display surface reads
//! [`resolve_display`]; a parity test pins the two to the same answers.

use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, Verdict};

pub use crate::capability_precedence::{
    ACTION_ALLOW, ACTION_DROP, ACTION_NONE, ACTION_REPROBE, ACTION_ROUTE_AWAY, ACTION_STRIP,
    ACTION_WITHHOLD, SeedCell,
};
use crate::capability_precedence::{
    DisplayInputs, Layer, LearnedInput, Resolution, Shown, no_opinion, operator_pins_capability,
    opinions,
};
use crate::config::{Config, ModelEntry};
use crate::learned_capability::LearnedRegistryEntry;
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

/// The resolved display verdict for one capability matrix cell.
///
/// `verdict` is a stable token: the core [`Verdict::as_str`] vocabulary
/// (`verified` / `broken` / `assumed` / `unknown`) for the learned,
/// verified, prior, and no-signal cases, `broken` / `cleared` for a seed
/// cell, plus the two PANEL-ONLY override
/// tokens (`FORCED_SUPPORTED` / `FORCED_UNSUPPORTED`). `supported`
/// carries the polarity the token alone does not for a prior `assumed`
/// cell (the catalog can assert either direction); it is `None` only for
/// the no-signal `unknown` cell. `source` is the winning layer's tag, or
/// `None` for `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayVerdict {
    /// The verdict token (see the type docs).
    pub verdict: &'static str,
    /// Support polarity; `None` only for an `unknown` cell.
    pub supported: Option<bool>,
    /// The winning layer's source tag; `None` only for an `unknown` cell.
    pub source: Option<&'static str>,
}

/// Resolve the display verdict for one `(lane, capability)` cell from the
/// already-gathered signal layers, applying
/// `override > learned > verified-working > seed > prior > unknown`.
///
/// READ-ONLY: it admits no probe, flips no `in_flight` flag, and touches
/// no metric -- the exact contrast with the side-effecting dispatch seam
/// this order is extracted from. Inputs:
///
/// - `override_cell`: the operator override resolution for the cell (the
///   `provider:nickname`-over-`provider` two-tier winner), or `None`.
/// - `learned`: the resident learned entry's `(verdict, evidence source)`
///   -- `VerifiedWorking` for a positive, `LearnedBroken(_)` for a
///   negative -- or `None` when no entry exists. The registry holds ONE
///   entry per cell, so "learned" (a broken negative) and
///   "verified-working" (a positive) are mutually exclusive here; their
///   relative precedence is honored by returning as soon as either is
///   seen, ahead of the seed and the prior.
/// - `seed`: the shipped beta seed's state on the cell, or `None` when the
///   seed does not cover it.
/// - `prior`: the catalog capability prior's truthiness, or `None` when
///   the catalog carries no prior for the cell.
pub const fn resolve_display_verdict(
    override_cell: Option<OverrideVerdict>,
    learned: Option<(Verdict, EvidenceSource)>,
    seed: Option<SeedCell>,
    prior: Option<bool>,
) -> DisplayVerdict {
    if let Some(verdict) = override_cell {
        return match verdict {
            OverrideVerdict::RouteAway => DisplayVerdict {
                verdict: FORCED_UNSUPPORTED,
                supported: Some(false),
                source: Some(SOURCE_OVERRIDE),
            },
            OverrideVerdict::ForceSupported => DisplayVerdict {
                verdict: FORCED_SUPPORTED,
                supported: Some(true),
                source: Some(SOURCE_OVERRIDE),
            },
        };
    }

    if let Some((verdict, evidence)) = learned {
        let source = Some(match evidence {
            EvidenceSource::Live => SOURCE_LIVE,
            EvidenceSource::Probe => SOURCE_PROBE,
        });
        match verdict {
            Verdict::LearnedBroken(_) => {
                return DisplayVerdict {
                    verdict: verdict.as_str(),
                    supported: Some(false),
                    source,
                };
            }
            Verdict::VerifiedWorking => {
                return DisplayVerdict {
                    verdict: verdict.as_str(),
                    supported: Some(true),
                    source,
                };
            }
            // A resident snapshot entry is only ever a negative or a
            // positive; any other verdict carries no acting signal, so it
            // falls through to the seed and the prior rather than masking it.
            _ => {}
        }
    }

    if let Some(seed) = seed {
        return seed_display(seed);
    }

    match prior {
        Some(supported) => DisplayVerdict {
            verdict: Verdict::Assumed(supported).as_str(),
            supported: Some(supported),
            source: Some(SOURCE_PRIOR),
        },
        None => DisplayVerdict {
            verdict: Verdict::Unknown.as_str(),
            supported: None,
            source: None,
        },
    }
}

/// The display verdict of a cell the shipped beta seed decides.
const fn seed_display(seed: SeedCell) -> DisplayVerdict {
    match seed {
        SeedCell::Withhold => DisplayVerdict {
            verdict: Verdict::LearnedBroken(FailurePhase::F1).as_str(),
            supported: Some(false),
            source: Some(SOURCE_SEED),
        },
        SeedCell::Cleared => DisplayVerdict {
            verdict: Verdict::Cleared.as_str(),
            supported: Some(true),
            source: Some(SOURCE_SEED),
        },
    }
}

/// The learned-entry facts the display action needs beyond its verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearnedActing {
    /// Whether the entry is a verified-working positive rather than a
    /// negative.
    pub verified: bool,
    /// Whether the entry acts at all (a self-identifying signal, or a
    /// corroborated inferred one).
    pub acting: bool,
    /// Whether a negative has lapsed into a re-probe, by the same rule the
    /// dispatch path applies.
    pub lapsed: bool,
    /// The detection phase of the entry.
    pub phase: FailurePhase,
    /// Whether the evidence came from live traffic or a probe.
    pub source: EvidenceSource,
}

impl LearnedActing {
    /// The acting facts of one snapshot entry, read against `now`.
    pub fn from_entry(entry: &LearnedRegistryEntry, now: Instant) -> Self {
        let verified = matches!(entry.verdict, Verdict::VerifiedWorking);
        Self {
            verified,
            acting: crate::learned_capability::signal_acts(entry.signal_tier, entry.observations),
            lapsed: !verified
                && crate::learned_capability::negative_lapsed(
                    entry.expires_at,
                    entry.recheck_owned,
                    now,
                ),
            phase: entry.phase,
            source: entry.source,
        }
    }
}

/// The inputs [`resolve_display_action`] reads for one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionInputs {
    /// The resolved display verdict for the cell.
    pub display: DisplayVerdict,
    /// The resident learned entry's acting facts, when one exists.
    pub learned: Option<LearnedActing>,
    /// The catalog prior for the cell, when the catalog carries one.
    pub prior: Option<bool>,
    /// Whether an F1 negative on this capability would be stripped in place
    /// rather than routed away (a droppable the operator neither marks
    /// essential nor pins to the wire).
    pub strip_applies: bool,
    /// The `[capability] enabled` kill switch: off, neither the learned nor
    /// the prior layer acts on routing. The seed acts either way.
    pub capability_enabled: bool,
    /// Whether the capability is a beta-flag key: a negative on it withholds
    /// the flag rather than stripping it or routing the target away.
    pub beta_flag: bool,
    /// The shipped beta seed's state on the cell, when the seed covers it.
    pub seed: Option<SeedCell>,
}

/// The routing action the dispatch filter takes for a cell, as a stable
/// token, mirroring `Router::unsupported_feature_for_target` without its side
/// effects. An override and the seed act regardless of the kill switch; a
/// learned or prior cell acts only while it is on. A learned entry that does
/// not act on routing (an uncorroborated inferred negative, or an advisory
/// live F3 negative) leaves the cell to the seed and then the prior, exactly
/// as the filter and the withheld-beta pass do.
pub fn resolve_display_action(inputs: ActionInputs) -> &'static str {
    let display = inputs.display;
    if display.source == Some(SOURCE_OVERRIDE) {
        return match display.supported {
            Some(false) if inputs.beta_flag => ACTION_WITHHOLD,
            Some(false) => ACTION_DROP,
            _ => ACTION_ALLOW,
        };
    }
    if inputs.capability_enabled
        && let Some(action) = inputs
            .learned
            .and_then(|learned| learned_action(learned, inputs.strip_applies, inputs.beta_flag))
    {
        return action;
    }
    match inputs.seed {
        Some(SeedCell::Withhold) => return ACTION_WITHHOLD,
        Some(SeedCell::Cleared) => return ACTION_ALLOW,
        None => {}
    }
    if !inputs.capability_enabled {
        return ACTION_NONE;
    }
    match inputs.prior {
        Some(false) => ACTION_ROUTE_AWAY,
        _ => ACTION_NONE,
    }
}

/// The action a resident learned entry takes, or `None` when it does not
/// act on routing and the cell falls through to the prior.
const fn learned_action(
    learned: LearnedActing,
    strip_applies: bool,
    beta_flag: bool,
) -> Option<&'static str> {
    if !learned.acting {
        return None;
    }
    if learned.verified {
        return Some(ACTION_ALLOW);
    }
    if crate::learned_capability::negative_is_advisory(learned.phase, learned.source) {
        return None;
    }
    if learned.lapsed {
        return Some(ACTION_REPROBE);
    }
    if beta_flag {
        return Some(ACTION_WITHHOLD);
    }
    if matches!(learned.phase, FailurePhase::F1) && strip_applies {
        Some(ACTION_STRIP)
    } else {
        Some(ACTION_ROUTE_AWAY)
    }
}

/// Whether an acting F1 negative on `capability` is stripped in place for a
/// lane rather than routed away: the capability is a droppable the operator
/// has not marked essential, and no operator beta floor pins it for the
/// provider entry or any mapped model ([`operator_pins_capability`]; a pinned
/// token is re-added after the strip, so the filter routes away instead).
pub fn lane_strips_capability(
    config: &Config,
    provider_entry: &str,
    models: &[&ModelEntry],
    capability: &str,
) -> bool {
    if !matches!(
        crate::capability_strip::effective_action_for(capability, &config.capability.essential),
        crate::capability_strip::CapabilityAction::Strip(_)
    ) {
        return false;
    }
    let empty = std::collections::BTreeMap::new();
    let pins = |headers| operator_pins_capability(config, provider_entry, headers, capability);
    if models.is_empty() {
        !pins(&empty)
    } else {
        !models.iter().any(|model| pins(&model.header_extras))
    }
}

#[cfg(test)]
#[path = "capability_display_tests.rs"]
mod capability_display_tests;
