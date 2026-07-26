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

use routectl_core::capability::{EvidenceSource, Verdict};

use crate::override_registry::{OverrideProvenance, OverrideVerdict};

/// Display verdict token for an operator route-away override cell. A
/// PANEL-ONLY token, distinct from the core [`Verdict`] vocabulary: an
/// override is an operator assertion, not a learned or catalog signal,
/// and the core verdict enum is a forever ledger contract that must not
/// grow display-only states.
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
/// tokens ([`FORCED_SUPPORTED`] / [`FORCED_UNSUPPORTED`]). `supported`
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

#[cfg(test)]
#[path = "capability_display_tests.rs"]
mod capability_display_tests;
