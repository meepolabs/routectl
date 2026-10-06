//! Read-only display resolver for a single capability matrix cell.
//!
//! ONE pure function that pins the within-target precedence order
//! `override > learned > verified-working > prior > unknown` for a
//! DISPLAY surface (the doctor capability matrix panel). It is an
//! EXTRACTION of the order the dispatch-path
//! `Router::unsupported_feature_for_target` enforces, NOT a reuse: that
//! seam is side-effecting (it claims probe slots, flips `in_flight`, and
//! bumps metrics), so it can never run from a read-only diagnostic. This
//! resolver reads three already-gathered inputs and returns a display
//! verdict; a sibling drift test asserts its order agrees with the
//! router's consolidated precedence matrix.

use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, Verdict};

use crate::config::{Config, ModelEntry, ProviderEntry};
use crate::learned_capability::LearnedRegistryEntry;
use crate::override_registry::{OverrideProvenance, OverrideVerdict};

/// Display verdict token for an operator route-away override cell. A
/// PANEL-ONLY token, distinct from the core [`Verdict`] vocabulary: an
/// override is an operator assertion, not a learned or catalog signal,
/// and the core verdict enum is a versioned, internal ledger contract that
/// must not grow display-only states.
pub const FORCED_UNSUPPORTED: &str = "forced_unsupported";

/// Display verdict token for an operator force-supported override cell.
/// PANEL-ONLY -- see [`FORCED_UNSUPPORTED`].
pub const FORCED_SUPPORTED: &str = "forced_supported";

/// Source tag: an operator override decided the cell.
pub const SOURCE_OVERRIDE: &str = "override";
/// Source tag: a learned observation from live traffic.
pub const SOURCE_LIVE: &str = "live";
/// Source tag: a learned observation from an out-of-band probe.
pub const SOURCE_PROBE: &str = "probe";
/// Source tag: a catalog capability prior.
pub const SOURCE_PRIOR: &str = "prior";

/// The resolved display verdict for one capability matrix cell.
///
/// `verdict` is a stable token: the core [`Verdict::as_str`] vocabulary
/// (`verified` / `broken` / `assumed` / `unknown`) for the learned,
/// verified, prior, and no-signal cases, plus the two PANEL-ONLY override
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
/// three already-gathered signal layers, applying
/// `override > learned > verified-working > prior > unknown`.
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
///   seen, ahead of the prior.
/// - `prior`: the catalog capability prior's truthiness, or `None` when
///   the catalog carries no prior for the cell.
pub const fn resolve_display_verdict(
    override_cell: Option<(OverrideVerdict, OverrideProvenance)>,
    learned: Option<(Verdict, EvidenceSource)>,
    prior: Option<bool>,
) -> DisplayVerdict {
    if let Some((verdict, _provenance)) = override_cell {
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
            // falls through to the prior rather than masking it.
            _ => {}
        }
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
/// Action token: no signal acts on routing for this cell.
pub const ACTION_NONE: &str = "none";

/// The learned-entry facts the display action needs beyond its verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearnedActing {
    /// Whether the entry is a verified-working positive rather than a
    /// negative.
    pub verified: bool,
    /// Whether the entry acts at all (a self-identifying signal, or a
    /// corroborated inferred one).
    pub acting: bool,
    /// Whether a negative's decay window has lapsed.
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
            lapsed: !verified && now >= entry.expires_at,
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
    /// the prior layer acts on routing.
    pub capability_enabled: bool,
}

/// The routing action the dispatch filter takes for a cell, as a stable
/// token, mirroring `Router::unsupported_feature_for_target` without its side
/// effects. An override acts regardless of the kill switch; a learned or
/// prior cell acts only while it is on. A learned entry that does not act on
/// routing (an uncorroborated inferred negative, or an advisory live F3
/// negative) leaves the cell to the prior, exactly as the filter does.
pub fn resolve_display_action(inputs: ActionInputs) -> &'static str {
    let display = inputs.display;
    if display.source == Some(SOURCE_OVERRIDE) {
        return match display.supported {
            Some(false) => ACTION_DROP,
            _ => ACTION_ALLOW,
        };
    }
    if !inputs.capability_enabled {
        return ACTION_NONE;
    }
    if let Some(action) = inputs
        .learned
        .and_then(|learned| learned_action(learned, inputs.strip_applies))
    {
        return action;
    }
    match inputs.prior {
        Some(false) => ACTION_ROUTE_AWAY,
        _ => ACTION_NONE,
    }
}

/// The action a resident learned entry takes, or `None` when it does not
/// act on routing and the cell falls through to the prior.
const fn learned_action(learned: LearnedActing, strip_applies: bool) -> Option<&'static str> {
    if !learned.acting {
        return None;
    }
    if learned.verified {
        return Some(ACTION_ALLOW);
    }
    if matches!(
        (learned.phase, learned.source),
        (FailurePhase::F3, EvidenceSource::Live)
    ) {
        return None;
    }
    if learned.lapsed {
        return Some(ACTION_REPROBE);
    }
    if matches!(learned.phase, FailurePhase::F1) && strip_applies {
        Some(ACTION_STRIP)
    } else {
        Some(ACTION_ROUTE_AWAY)
    }
}

/// Whether an acting F1 negative on `capability` is stripped in place for a
/// lane rather than routed away: the capability is a droppable the operator
/// has not marked essential, and no operator beta floor -- the provider
/// entry's own or any mapped model's `header_extras` -- pins its beta token
/// to the wire (a pinned token is re-added after the strip, so the filter
/// routes away instead).
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
    let tokens = crate::capability_strip::strip_beta_tokens(capability);
    if tokens.is_empty() {
        return true;
    }
    let entry = config.providers.get(provider_entry);
    let provider_floor = entry.map_or(&[][..], ProviderEntry::anthropic_beta_floor);
    let provider_headers = entry.map(ProviderEntry::header_extras);
    let empty = std::collections::BTreeMap::new();
    let header_floors: Vec<Vec<String>> = if models.is_empty() {
        vec![crate::router::operator_betas(provider_headers, &empty)]
    } else {
        models
            .iter()
            .map(|model| crate::router::operator_betas(provider_headers, &model.header_extras))
            .collect()
    };
    !tokens.iter().any(|token| {
        provider_floor.iter().any(|pinned| pinned == token)
            || header_floors
                .iter()
                .any(|floor| floor.iter().any(|pinned| pinned == token))
    })
}

#[cfg(test)]
#[path = "capability_display_tests.rs"]
mod capability_display_tests;
