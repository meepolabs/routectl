//! The placement partition: which seat a NEW conversation is born on when
//! the pool's subscription budget is known.
//!
//! # Why this partitions instead of scoring
//!
//! The seat chooser already ranks on one number (available RPM headroom), so
//! the obvious move is to blend a quota figure into that number. It does not
//! work, because "unknown" has no neutral NUMBER. Every candidate is wrong in
//! one direction or the other: a low stand-in makes the seat routectl knows
//! LEAST about the most attractive target, and a high one silently removes a
//! seat that may be perfectly healthy. A blend would also need a special case
//! for the all-over-cap pool, which must still place rather than fail.
//!
//! So the eligible seats are PARTITIONED and the ranking runs inside one tier:
//!
//! 1. Any eligible seat with a fresh known FAST reading BELOW its curated cap
//!    -> restrict to those and take the most remaining. Unknown does not
//!    compete at all, so a seat known to be EMPTY beats a seat with no
//!    reading, which is the whole point.
//! 2. Every eligible seat fresh-known and all of them capped -> take the most
//!    remaining. A soft cap never fails a request; it only orders one.
//! 3. Anything else -- every seat unknown, or a MIX of capped-known and
//!    unknown -- decides nothing and falls through to the unchanged headroom
//!    path. Cap-dormant, byte-identical to a build with no quota at all.
//!
//! Case 3 is why the fail-closed rule (an unknown or stale fact never enables
//! more aggressive behavior) is STRUCTURAL here rather than a constant someone
//! has to keep choosing correctly: there is no arm in which an unknown reading
//! moves a placement.
//!
//! # What this deliberately does not read
//!
//! Only the FAST window. The SLOW window's near-exhaustion guard and the
//! billing tri-state are both extracted, curated and stored, and neither is
//! wired here. Actual exhaustion is already handled reactively -- an upstream
//! refusal trips the per-seat breaker and the seat drops out of the
//! dispatchability filter -- and demoting on the long window or on billing
//! would need evidence this deployment cannot yet produce. Wiring them later
//! adds an arm to the partition and changes nothing else.
//!
//! Neither does it read RPM. RPM stays an ELIGIBILITY gate, upstream of
//! everything here, and its `None` = unlimited convention never crosses into
//! quota: an absent RPM limit genuinely means unlimited, while an absent quota
//! reading means no evidence.
//!
//! # What a warm pin is protected from
//!
//! Nothing in this module can move an established conversation. It is
//! consulted for a BIRTH pick only, so a soft cap can never evict or migrate a
//! session off the seat holding its warm prompt cache -- a pinned session over
//! its cap runs to actual exhaustion and is rescued by the reactive path, not
//! by this one.

use super::freshness::ObservationStamp;
use super::key::SeatKey;
use super::store::QuotaStore;
use super::window::{QuotaWindow, WindowRole};

/// One seat's FAST-window standing for a placement decision.
///
/// Three states rather than a number, mirroring the window type one level up:
/// `Unknown` carries no figure, so no caller can rank on a reading that does
/// not exist. `remaining` is the fraction of the window still unspent, which
/// orders the same way for both known tiers -- every seat in one pool is
/// measured against the same curated cap, so ordering on remaining-in-window
/// and remaining-to-cap agree, and remaining-in-window is the figure that
/// still means something if a future pool ever mixes caps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SeatQuota {
    /// No trustworthy FAST reading: never observed, expired at read time, or
    /// refused by the trust rules. Never competes for a placement.
    Unknown,
    /// A fresh known FAST reading below this provider's curated cap.
    BelowCap {
        /// Fraction of the window still unspent.
        remaining: f64,
    },
    /// A fresh known FAST reading at or above the curated cap. Still eligible
    /// -- the cap is soft -- but only against other capped seats.
    AtCap {
        /// Fraction of the window still unspent.
        remaining: f64,
    },
}

/// What the partition did on one pick, for the counters and the diagnostic.
///
/// Deliberately NOT a `SelectionOutcome` variant: those map to a fixed
/// `selection_decision` vocabulary persisted in the usage ledger and
/// documented as operator-facing. Quota changes WHICH seat wins inside the
/// existing birth path and never the outcome vocabulary, so its own reporting
/// lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    /// Quota was not consulted: no tiers were supplied at all, because the
    /// switch is off, the provider curates no FAST window, or the pool is
    /// degenerate. Emits nothing -- an off switch must be silent, or the
    /// diagnostics alone would tell an operator the feature is still running.
    Dormant,
    /// Restricted the pick to fresh-known seats below their cap.
    BelowCapTier,
    /// Every eligible seat was fresh-known and capped, so the pick took the
    /// most remaining and the request was NOT failed.
    AllCappedMostRemaining,
    /// A mix of capped-known and unknown seats: fell through to the unchanged
    /// headroom path rather than preferring either.
    MixedUnknownFallback,
    /// Every eligible seat was unknown: fell through to the unchanged
    /// headroom path.
    AllUnknownFallback,
}

impl QuotaDecision {
    /// Whether this decision actually chose the seat. `false` for the dormant
    /// and fall-through cases, where the unchanged headroom path decided.
    pub const fn placed(self) -> bool {
        matches!(self, Self::BelowCapTier | Self::AllCappedMostRemaining)
    }
}

/// Classify each seat's FAST window for a pool served by `provider_kind`.
///
/// Returns an EMPTY vec -- the dormant answer, which every caller treats as
/// "quota contributed nothing" -- when the provider curates no FAST window.
/// That is how an uncurated provider, and Codex (which reports no short
/// recovering window at all), stay dormant by construction rather than by a
/// branch somebody has to remember.
///
/// Otherwise the result is index-aligned with `keys`: a seat with no account
/// identity, no stored reading, or a reading whose FAST window has lapsed
/// reads `Unknown`. Expiry is not re-implemented here --
/// [`QuotaStore::reading_for`] applies both freshness bounds, so this cannot
/// see a window that has outlived its own reset or its age ceiling.
pub fn seat_tiers(
    store: &QuotaStore,
    keys: &[Option<SeatKey>],
    provider_kind: Option<&str>,
    now: &ObservationStamp,
) -> Vec<SeatQuota> {
    let Some(row) =
        provider_kind.and_then(|kind| super::curation::row_for(kind, &WindowRole::Fast))
    else {
        return Vec::new();
    };
    keys.iter()
        .map(|key| {
            let Some(key) = key.as_ref() else {
                return SeatQuota::Unknown;
            };
            match store.reading_for(key, now).map(|reading| reading.fast) {
                Some(QuotaWindow::Known { utilization, .. }) => {
                    let spent = utilization.fraction();
                    let remaining = 1.0 - spent;
                    if spent < row.threshold {
                        SeatQuota::BelowCap { remaining }
                    } else {
                        SeatQuota::AtCap { remaining }
                    }
                }
                Some(QuotaWindow::Unknown) | None => SeatQuota::Unknown,
            }
        })
        .collect()
}

/// Restrict `eligible` to the tier quota decides on, or `None` to leave the
/// decision to the caller's unchanged ranking.
///
/// `eligible` is the set the dispatchability filter and the health preference
/// have already produced, so this never re-implements either. `quota` is
/// index-aligned with the pool; an empty `quota` (or a short one) reads as
/// unknown throughout and therefore decides nothing.
///
/// The returned set is every seat TIED on the most remaining within the
/// chosen tier, left for the caller's existing anti-herd rotation to break --
/// so a burst of new conversations still spreads instead of herding onto the
/// one emptiest seat.
pub fn restrict_by_quota(
    eligible: &[usize],
    quota: &[SeatQuota],
    decision: &mut QuotaDecision,
) -> Option<Vec<usize>> {
    if quota.is_empty() || eligible.is_empty() {
        *decision = QuotaDecision::Dormant;
        return None;
    }
    // A seat past the end of `quota` reads unknown rather than panicking: a
    // length mismatch is a wiring bug, and falling through to the unchanged
    // path is the direction that cannot make a placement worse.
    let tier = |idx: usize| quota.get(idx).copied().unwrap_or(SeatQuota::Unknown);

    let below: Vec<usize> = eligible
        .iter()
        .copied()
        .filter(|&i| matches!(tier(i), SeatQuota::BelowCap { .. }))
        .collect();
    if !below.is_empty() {
        *decision = QuotaDecision::BelowCapTier;
        return Some(most_remaining(&below, quota));
    }

    if eligible
        .iter()
        .any(|&i| matches!(tier(i), SeatQuota::Unknown))
    {
        *decision = if eligible
            .iter()
            .all(|&i| matches!(tier(i), SeatQuota::Unknown))
        {
            QuotaDecision::AllUnknownFallback
        } else {
            QuotaDecision::MixedUnknownFallback
        };
        return None;
    }

    // Every eligible seat is fresh-known and every one is capped. Place on the
    // most remaining; never fail the request over a soft cap.
    *decision = QuotaDecision::AllCappedMostRemaining;
    Some(most_remaining(eligible, quota))
}

/// The seats of `candidates` tied on the most remaining window.
///
/// Only ever called with candidates whose tier carries a figure, so an
/// unknown reading contributes nothing rather than a stand-in number: it
/// scores below every real reading and can only be returned when the caller
/// already established there is nothing else, which the partition above never
/// does.
fn most_remaining(candidates: &[usize], quota: &[SeatQuota]) -> Vec<usize> {
    let remaining = |idx: usize| match quota.get(idx) {
        Some(SeatQuota::BelowCap { remaining } | SeatQuota::AtCap { remaining }) => *remaining,
        Some(SeatQuota::Unknown) | None => f64::NEG_INFINITY,
    };
    let best = candidates
        .iter()
        .map(|&i| remaining(i))
        .fold(f64::NEG_INFINITY, f64::max);
    candidates
        .iter()
        .copied()
        .filter(|&i| remaining(i) == best)
        .collect()
}

#[cfg(test)]
#[path = "placement_tests.rs"]
mod placement_tests;
