//! Tests for the value-level time primitives: the reset bounds in both
//! directions, the millis-as-seconds class, both freshness bounds, and the
//! not-fresh-on-unanswerable-arithmetic posture.
//!
//! The monotonic clock cannot be constructed from a literal, so every stamp
//! here is derived from one captured base instant. That is what lets a test
//! age a reading by an exact amount without sleeping.

use super::*;

/// The real `anthropic-ratelimit-unified-5h-reset` from a captured envelope,
/// in epoch SECONDS -- the scale the ledger stores. The implausible-reset test
/// multiplies it by a thousand to reproduce a milliseconds misparse on a value
/// that is otherwise genuine.
const CAPTURED_5H_RESET_SECS: u64 = 1_781_001_000;

const FIVE_HOURS: Duration = Duration::from_hours(5);
const TOLERANCE: Duration = Duration::from_mins(5);

fn epoch_secs(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// Observation an hour into the captured 5h window.
fn observed() -> ObservationStamp {
    ObservationStamp::from_parts(epoch_secs(CAPTURED_5H_RESET_SECS - 3_600), Instant::now())
}

/// `base` advanced by `age` on the monotonic clock and by `age` on the wall
/// clock, i.e. a read `age` after the observation with both clocks agreeing.
fn read_after(base: &ObservationStamp, age: Duration) -> ObservationStamp {
    ObservationStamp::from_parts(base.wall + age, base.monotonic + age)
}

#[test]
fn accepts_a_captured_reset_inside_its_own_window() {
    let observed = observed();

    let accepted = accept_reset(
        epoch_secs(CAPTURED_5H_RESET_SECS),
        &observed,
        FIVE_HOURS,
        TOLERANCE,
    );

    assert_eq!(
        accepted.map(ValidatedReset::at),
        Ok(epoch_secs(CAPTURED_5H_RESET_SECS)),
        "an accepted reset round-trips the instant it validated"
    );
}

#[test]
fn rejects_a_reset_already_passed_at_observation() {
    let observed = observed();

    let rejected = accept_reset(
        observed.wall - Duration::from_secs(1),
        &observed,
        FIVE_HOURS,
        TOLERANCE,
    );

    assert_eq!(rejected, Err(ResetRejection::Expired));
}

#[test]
fn rejects_a_reset_equal_to_the_observation_instant() {
    let observed = observed();

    let rejected = accept_reset(observed.wall, &observed, FIVE_HOURS, TOLERANCE);

    assert_eq!(
        rejected,
        Err(ResetRejection::Expired),
        "a window resetting exactly now describes a window that is over"
    );
}

#[test]
fn rejects_a_reset_beyond_the_window_duration_plus_tolerance() {
    let observed = observed();

    let rejected = accept_reset(
        observed.wall + FIVE_HOURS + TOLERANCE + Duration::from_secs(1),
        &observed,
        FIVE_HOURS,
        TOLERANCE,
    );

    assert_eq!(rejected, Err(ResetRejection::Implausible));
}

#[test]
fn accepts_a_reset_exactly_at_the_tolerance_boundary() {
    let observed = observed();

    let accepted = accept_reset(
        observed.wall + FIVE_HOURS + TOLERANCE,
        &observed,
        FIVE_HOURS,
        TOLERANCE,
    );

    assert!(accepted.is_ok(), "the tolerance bound is inclusive");
}

#[test]
fn rejects_a_captured_reset_multiplied_into_milliseconds() {
    let observed = observed();

    let rejected = accept_reset(
        epoch_secs(CAPTURED_5H_RESET_SECS * 1_000),
        &observed,
        FIVE_HOURS,
        TOLERANCE,
    );

    assert_eq!(
        rejected,
        Err(ResetRejection::Implausible),
        "a milliseconds value read as seconds would otherwise read as permanently valid"
    );
}

#[test]
fn reports_overflow_rather_than_accepting_when_the_bound_cannot_be_computed() {
    let observed = observed();
    let far_future = observed.wall + Duration::from_secs(1);

    let unaddable_span = accept_reset(far_future, &observed, Duration::MAX, Duration::from_secs(1));
    let unaddable_bound = accept_reset(far_future, &observed, Duration::MAX, Duration::ZERO);

    assert_eq!(unaddable_span, Err(ResetRejection::Overflow));
    assert_eq!(unaddable_bound, Err(ResetRejection::Overflow));
}

#[test]
fn a_recent_reading_before_its_reset_is_fresh() {
    let observed = observed();
    let now = read_after(&observed, Duration::from_mins(1));

    assert!(is_fresh(
        epoch_secs(CAPTURED_5H_RESET_SECS),
        &observed,
        &now,
        Duration::from_mins(30),
    ));
}

#[test]
fn a_reading_read_at_or_after_its_reset_is_not_fresh() {
    let observed = observed();
    let reset_at = epoch_secs(CAPTURED_5H_RESET_SECS);
    let at_reset = read_after(&observed, Duration::from_hours(1));

    assert!(!is_fresh(
        reset_at,
        &observed,
        &at_reset,
        Duration::from_hours(2)
    ));
}

#[test]
fn a_reading_older_than_the_age_ceiling_is_not_fresh_even_before_its_reset() {
    let observed = observed();
    let much_later = read_after(&observed, Duration::from_mins(45));

    assert!(!is_fresh(
        epoch_secs(CAPTURED_5H_RESET_SECS),
        &observed,
        &much_later,
        Duration::from_mins(30),
    ));
}

#[test]
fn the_monotonic_ceiling_still_ages_a_reading_when_wall_time_moves_backwards() {
    let observed = observed();
    let skewed = ObservationStamp::from_parts(
        observed.wall - Duration::from_hours(1),
        observed.monotonic + Duration::from_mins(45),
    );

    assert!(
        !is_fresh(
            epoch_secs(CAPTURED_5H_RESET_SECS),
            &observed,
            &skewed,
            Duration::from_mins(30),
        ),
        "a backwards wall clock must not restore an aged-out reading"
    );
}

#[test]
fn a_read_preceding_its_own_observation_is_not_fresh_rather_than_panicking() {
    let base = observed();
    let observed_later =
        ObservationStamp::from_parts(base.wall, base.monotonic + Duration::from_mins(10));

    assert!(!is_fresh(
        epoch_secs(CAPTURED_5H_RESET_SECS),
        &observed_later,
        &base,
        Duration::from_mins(30),
    ));
}

#[test]
fn stamping_now_advances_the_monotonic_clock() {
    let first = ObservationStamp::now();

    let second = ObservationStamp::now();

    // Only the monotonic component is asserted. `SystemTime` carries no
    // ordering guarantee across two reads -- a backward NTP or manual
    // adjustment between them is legal -- so asserting wall-clock ordering
    // would be a flake, and it would pass against an implementation that
    // returned a constant wall time anyway.
    assert!(second.monotonic() >= first.monotonic());
}

#[test]
fn a_reset_is_only_obtainable_by_passing_the_plausibility_bound() {
    // The type-level half of the milliseconds defense, and the reason
    // `QuotaWindow::Known` demands a `ValidatedReset` rather than a raw
    // instant: a refused reset yields NO value a trusted window could be
    // built around, so a reducer cannot skip the bound the way it could skip
    // a documented "already validated" convention.
    let observed = observed();

    let refused = accept_reset(
        epoch_secs(CAPTURED_5H_RESET_SECS * 1_000),
        &observed,
        FIVE_HOURS,
        TOLERANCE,
    );

    assert!(
        refused.is_err(),
        "a milliseconds reset yields no reset value"
    );
    // And the accepted path is the ONLY source of one.
    let accepted = accept_reset(
        epoch_secs(CAPTURED_5H_RESET_SECS),
        &observed,
        FIVE_HOURS,
        TOLERANCE,
    )
    .expect("the captured reset is plausible for its own window");
    assert_eq!(accepted.at(), epoch_secs(CAPTURED_5H_RESET_SECS));
}
