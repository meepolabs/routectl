//! Reduction of raw per-request evidence into one per-lane correction.
//!
//! # Direction, stated once
//!
//! The factor is `actual / estimate` in integer permille, and it multiplies
//! the estimate: `corrected = raw * permille / 1000`.
//!
//! - A factor ABOVE [`IDENTITY_PERMILLE`] means the estimator UNDER-counts on
//!   this lane, so the corrected estimate is LARGER than the raw one and the
//!   window gate becomes more willing to skip a small-window target.
//! - A factor BELOW [`IDENTITY_PERMILLE`] means the estimator OVER-counts, so
//!   the corrected estimate is SMALLER and the gate becomes more willing to
//!   admit a target it would otherwise have skipped.
//!
//! The second direction is the dangerous one -- it admits requests the
//! uncorrected estimate judged too large -- which is why a reduced ratio
//! outside [`MIN_SANE_PERMILLE`]..=[`MAX_SANE_PERMILLE`] is refused outright
//! instead of being clamped to the bound.
//!
//! # Why a median of cohort medians
//!
//! A plain median over all retained samples is a median over REQUESTS, and
//! request volume is not evenly distributed across callers: one long-running
//! conversation can contribute most of a lane's samples and therefore define
//! its factor by itself. Reducing per cohort first and then taking the median
//! of the cohort medians gives every cohort one vote regardless of its
//! volume. The cohort is a grouping dimension only; it is never part of the
//! lane key.
//!
//! The median IS the outlier rejection. There is deliberately no second
//! trimming or deviation pass on top of it.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use super::store::LaneSamples;

/// The identity factor: multiplying by this leaves the estimate unchanged.
/// Removing this whole module is therefore equivalent to pinning every lane
/// here, which is what makes the correction removable without touching the
/// gate's own arithmetic.
pub const IDENTITY_PERMILLE: u32 = 1_000;

/// Minimum retained fresh samples before a lane can produce a factor. Matches
/// the cache-reuse estimator's trial floor: below it, a handful of unusual
/// requests would define the lane.
const MIN_SAMPLES: usize = 8;

/// Minimum distinct cohorts before a lane can produce a factor. Three is the
/// smallest count at which the outer median is not simply one cohort's own
/// median (with two cohorts it is their midpoint, which one cohort can still
/// drag half the distance).
const MIN_COHORTS: usize = 3;

/// How old a sample may be and still count. Beyond this the request mix that
/// produced it is no longer evidence about the request mix arriving now --
/// prompt shape, tool payloads and system prefixes all drift.
///
/// Also bounds how far back the boot warm rebuild reads: a sample older than
/// this cannot survive the reduction, so loading one is pure waste.
pub const MAX_SAMPLE_AGE: Duration = Duration::from_hours(24);

/// Lower end of the sane band. The estimator counts serialized bytes over
/// four, so a real tokenizer disagreeing with it by more than a factor of two
/// in either direction is evidence of a mis-keyed or garbage-fed lane rather
/// than of a genuinely extreme correction.
const MIN_SANE_PERMILLE: u32 = 500;

/// Upper end of the sane band. See [`MIN_SANE_PERMILLE`].
const MAX_SANE_PERMILLE: u32 = 2_000;

/// A validated per-lane correction, guaranteed to sit inside the sane band.
///
/// Constructible only through [`reduce`], so no call site can conjure a
/// factor the band would have refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Factor {
    permille: u32,
}

impl Factor {
    /// The correction as integer permille of the raw estimate. The gate reads
    /// the corrected number, not the ratio, so this is a test read surface;
    /// ungate it if a diagnostic ever needs to surface the factor.
    #[cfg(test)]
    pub const fn permille(self) -> u32 {
        self.permille
    }

    /// Correct `raw` by this factor.
    ///
    /// Integer arithmetic throughout: a routing decision must not hinge on a
    /// float, which is the same constraint the window gate's own margin ratio
    /// is written to honor. The multiply saturates rather than wrapping, so a
    /// pathological input can only ever produce a LARGER corrected estimate
    /// (more willing to skip), never a wrapped small one.
    pub const fn apply(self, raw: u64) -> u64 {
        raw.saturating_mul(self.permille as u64) / IDENTITY_PERMILLE as u64
    }
}

/// Reduce one lane's retained evidence into a correction, or refuse.
///
/// `None` -- meaning the caller uses the raw estimate, exactly as it would
/// without any calibration -- for every one of: too few fresh samples, too
/// few distinct cohorts, and a reduced ratio outside the sane band. Refusing
/// rather than clamping is deliberate: a ratio outside the band is evidence
/// about the lane's plumbing, not about its tokenizer.
pub fn reduce(samples: &LaneSamples, now: SystemTime) -> Option<Factor> {
    let mut per_cohort: BTreeMap<u64, Vec<u32>> = BTreeMap::new();
    let mut fresh = 0_usize;
    for sample in samples.iter() {
        if is_stale(sample.ts, now) {
            continue;
        }
        fresh += 1;
        per_cohort
            .entry(sample.cohort)
            .or_default()
            .push(sample.permille);
    }
    if fresh < MIN_SAMPLES || per_cohort.len() < MIN_COHORTS {
        return None;
    }
    let mut cohort_medians: Vec<u32> = per_cohort
        .into_values()
        .map(|mut ratios| {
            ratios.sort_unstable();
            median_of_sorted(&ratios)
        })
        .collect();
    cohort_medians.sort_unstable();
    let permille = median_of_sorted(&cohort_medians);
    if !(MIN_SANE_PERMILLE..=MAX_SANE_PERMILLE).contains(&permille) {
        return None;
    }
    Some(Factor { permille })
}

/// Whether a sample recorded at `ts` is older than [`MAX_SAMPLE_AGE`]. A
/// sample stamped in the future (a clock that stepped backwards) reads as
/// fresh, which is the direction that keeps evidence rather than silently
/// discarding a whole lane on a clock adjustment.
fn is_stale(ts: SystemTime, now: SystemTime) -> bool {
    now.duration_since(ts).is_ok_and(|age| age > MAX_SAMPLE_AGE)
}

/// Median of an ascending slice, averaging the two middles on an even count.
/// Panics on an empty slice, which `reduce` cannot produce: every group it
/// builds is created by pushing a sample into it.
fn median_of_sorted(sorted: &[u32]) -> u32 {
    let len = sorted.len();
    assert!(len > 0, "median of an empty group");
    let mid = len / 2;
    if len % 2 == 1 {
        sorted[mid]
    } else {
        // Widened before the add so two large ratios cannot overflow.
        let sum = u64::from(sorted[mid - 1]) + u64::from(sorted[mid]);
        u32::try_from(sum / 2).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
#[path = "factor_tests.rs"]
mod factor_tests;
