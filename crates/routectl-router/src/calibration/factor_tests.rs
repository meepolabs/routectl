//! Tests for the evidence reduction: the sample and cohort floors, the age
//! bound, cohort balance, the out-of-range refusal, and the DIRECTION of the
//! correction.
//!
//! Every assertion is written against the ratio rather than a token count, so
//! nothing here pins the byte-length estimator's own granularity.

use super::*;

use std::time::Duration;

use crate::calibration::store::Sample;

/// Well inside the age bound.
const RECENT: Duration = Duration::from_mins(1);

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
}

fn sample(permille: u32, cohort: u64, age: Duration) -> Sample {
    Sample {
        ts: now() - age,
        permille,
        cohort,
    }
}

/// Build a lane whose samples are `(permille, cohort)` pairs, all fresh.
fn lane(pairs: &[(u32, u64)]) -> LaneSamples {
    let mut samples = LaneSamples::default();
    for &(permille, cohort) in pairs {
        samples.push(sample(permille, cohort, RECENT));
    }
    samples
}

/// Spread `permille` across three cohorts with enough samples to clear both
/// floors, so a test can isolate one variable at a time.
fn uniform_lane(permille: u32) -> LaneSamples {
    let pairs: Vec<(u32, u64)> = (0..9).map(|i| (permille, i % 3)).collect();
    lane(&pairs)
}

#[test]
fn a_factor_above_identity_grows_the_estimate_and_below_it_shrinks() {
    // THE DIRECTION, pinned on the arithmetic itself. The factor is
    // actual/estimate: a lane whose real token count runs ABOVE the estimate
    // must produce a LARGER corrected figure, never a smaller one.
    let under_counting = reduce(&uniform_lane(1_500), now()).expect("a healthy lane");
    assert_eq!(under_counting.permille(), 1_500);
    assert_eq!(under_counting.apply(1_000), 1_500);

    let over_counting = reduce(&uniform_lane(800), now()).expect("a healthy lane");
    assert_eq!(over_counting.apply(1_000), 800);
}

#[test]
fn the_identity_factor_leaves_the_estimate_byte_identical() {
    // The deletion contract: pinning a lane at identity is the same as having
    // no correction at all, which is what makes removing the multiply a
    // behavior-preserving change.
    let factor = reduce(&uniform_lane(IDENTITY_PERMILLE), now()).expect("a healthy lane");
    for raw in [0_u64, 1, 999, 200_000, u64::from(u32::MAX)] {
        assert_eq!(factor.apply(raw), raw, "identity must not move {raw}");
    }
}

#[test]
fn the_correction_is_the_ratio_applied_forward_not_its_reciprocal() {
    // NEGATIVE CONTROL on the direction. `apply` must multiply by the ratio;
    // the plausible-looking inversion (dividing by it, i.e. multiplying by
    // 1000/permille) produces a materially different number, and this pins
    // which of the two the code does. Applied inverted, an under-counting
    // lane would SHRINK the estimate and admit requests the uncorrected gate
    // had correctly judged too large.
    let raw = 120_000_u64;
    let factor = reduce(&uniform_lane(1_250), now()).expect("a healthy lane");

    let forward = raw * 1_250 / u64::from(IDENTITY_PERMILLE);
    let inverted = raw * u64::from(IDENTITY_PERMILLE) / 1_250;

    assert_eq!(
        factor.apply(raw),
        forward,
        "the factor multiplies the estimate"
    );
    assert_ne!(
        factor.apply(raw),
        inverted,
        "applying the reciprocal would shrink an under-counting lane's estimate",
    );
    assert!(
        factor.apply(raw) > raw,
        "an under-counting lane must GROW the estimate; got {} from {raw}",
        factor.apply(raw),
    );
}

#[test]
fn a_lane_below_the_sample_floor_is_refused() {
    // Three cohorts, so the cohort floor is satisfied and the sample count is
    // the only thing under test.
    let pairs: Vec<(u32, u64)> = (0..MIN_SAMPLES - 1)
        .map(|i| (1_100, (i % 3) as u64))
        .collect();
    assert_eq!(reduce(&lane(&pairs), now()), None);

    let pairs: Vec<(u32, u64)> = (0..MIN_SAMPLES).map(|i| (1_100, (i % 3) as u64)).collect();
    assert!(
        reduce(&lane(&pairs), now()).is_some(),
        "the floor itself is usable"
    );
}

#[test]
fn a_lane_below_the_cohort_floor_is_refused() {
    // Plenty of samples, but drawn from too few callers: exactly the shape
    // the cohort floor exists to refuse.
    let pairs: Vec<(u32, u64)> = (0..12)
        .map(|i| (1_100, (i % (MIN_COHORTS - 1)) as u64))
        .collect();
    assert_eq!(reduce(&lane(&pairs), now()), None);

    let pairs: Vec<(u32, u64)> = (0..12).map(|i| (1_100, (i % MIN_COHORTS) as u64)).collect();
    assert!(reduce(&lane(&pairs), now()).is_some());
}

#[test]
fn samples_past_the_age_bound_do_not_count_toward_the_floors() {
    // Arrange: a lane whose evidence would clear both floors if age were
    // ignored, but every sample is older than the bound.
    let mut samples = LaneSamples::default();
    for i in 0..12 {
        samples.push(sample(
            1_100,
            i % 3,
            MAX_SAMPLE_AGE + Duration::from_secs(1),
        ));
    }

    // Act + Assert: stale evidence is no evidence.
    assert_eq!(reduce(&samples, now()), None);

    // A single fresh sample cannot rescue it either -- the floors count only
    // fresh samples, so the lane stays refused.
    samples.push(sample(1_100, 9, RECENT));
    assert_eq!(reduce(&samples, now()), None);
}

#[test]
fn stale_samples_are_excluded_from_the_reduced_ratio() {
    // Arrange: fresh evidence at one ratio, expired evidence at a wildly
    // different one. If the age bound leaked, the reduced ratio would move.
    let mut samples = LaneSamples::default();
    for i in 0..9 {
        samples.push(sample(1_200, i % 3, RECENT));
    }
    for i in 0..9 {
        samples.push(sample(
            600,
            10 + (i % 3),
            MAX_SAMPLE_AGE + Duration::from_mins(1),
        ));
    }

    // Act
    let factor = reduce(&samples, now()).expect("the fresh evidence clears both floors");

    // Assert: only the fresh cohorts voted.
    assert_eq!(factor.permille(), 1_200);
}

#[test]
fn one_high_volume_cohort_cannot_define_the_lane() {
    // THE COHORT-BALANCE PROPERTY. One caller floods the lane at an extreme
    // ratio while two others sit near identity. A median over REQUESTS would
    // return the flooder's ratio; a median over COHORT MEDIANS gives each
    // caller one vote.
    let mut pairs: Vec<(u32, u64)> = (0..40).map(|_| (1_900, 1)).collect();
    pairs.extend((0..3).map(|_| (1_000, 2)));
    pairs.extend((0..3).map(|_| (1_020, 3)));

    let factor = reduce(&lane(&pairs), now()).expect("three cohorts, enough samples");

    assert_eq!(
        factor.permille(),
        1_020,
        "the outer median is over cohort medians, so the flood is one vote",
    );
    // The request-weighted median would have been the flooder's own ratio.
    assert_ne!(factor.permille(), 1_900);
}

#[test]
fn an_out_of_range_ratio_is_refused_rather_than_clamped() {
    // Both ends. A reduced ratio outside the sane band is evidence about the
    // lane's plumbing (mis-keyed, or fed garbage), so it must yield NO
    // correction. Clamping to the bound would let such a lane still move a
    // routing decision.
    let too_low = uniform_lane(MIN_SANE_PERMILLE - 1);
    assert_eq!(
        reduce(&too_low, now()),
        None,
        "a below-band ratio must refuse, not clamp up to the floor",
    );

    let too_high = uniform_lane(MAX_SANE_PERMILLE + 1);
    assert_eq!(
        reduce(&too_high, now()),
        None,
        "an above-band ratio must refuse, not clamp down to the ceiling",
    );

    // The bounds themselves are inside the band.
    assert!(reduce(&uniform_lane(MIN_SANE_PERMILLE), now()).is_some());
    assert!(reduce(&uniform_lane(MAX_SANE_PERMILLE), now()).is_some());
}

#[test]
fn the_reduction_is_a_median_so_one_extreme_cohort_does_not_pull_it() {
    // Median, not mean, and no separate outlier pass: three cohorts at
    // 1000 / 1100 / 1900 reduce to the MIDDLE cohort's ratio. The mean would
    // land near 1333, dragged by the outlier.
    let pairs = [
        (1_000, 1),
        (1_000, 1),
        (1_000, 1),
        (1_100, 2),
        (1_100, 2),
        (1_100, 2),
        (1_900, 3),
        (1_900, 3),
        (1_900, 3),
    ];

    let factor = reduce(&lane(&pairs), now()).expect("three cohorts, enough samples");

    assert_eq!(factor.permille(), 1_100);
}

#[test]
fn a_cohorts_own_ratio_is_its_median_not_its_mean() {
    // The inner reduction is a median too, on the same reasoning: one odd
    // request inside a cohort must not define that cohort's vote.
    let pairs = [
        (1_000, 1),
        (1_010, 1),
        (1_900, 1),
        (1_000, 2),
        (1_010, 2),
        (1_950, 2),
        (1_000, 3),
        (1_010, 3),
        (1_980, 3),
    ];

    let factor = reduce(&lane(&pairs), now()).expect("three cohorts, enough samples");

    assert_eq!(factor.permille(), 1_010);
}

#[test]
fn an_empty_lane_produces_no_factor() {
    assert_eq!(reduce(&LaneSamples::default(), now()), None);
}
