//! Tests for the normalized window value: the structural distinctness of
//! unknown from a known zero, the utilization constructor's refusals, and the
//! DIRECTION of its saturation.
//!
//! The saturation-direction test is the load-bearing one. Every other bound
//! here fails loudly if it regresses; saturating the wrong way would invent
//! headroom on an exhausted seat and read as a perfectly healthy value.

use super::*;

use std::time::{Duration, Instant, SystemTime};

use super::super::freshness::{ObservationStamp, accept_reset};

/// A reset that has genuinely passed `accept_reset`, since that is now the
/// only way to name one -- the tests cannot fabricate a trusted window either.
fn reset_at() -> ValidatedReset {
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let observed = ObservationStamp::from_parts(base, Instant::now());
    accept_reset(
        base + Duration::from_mins(1),
        &observed,
        Duration::from_hours(5),
        Duration::from_mins(1),
    )
    .expect("a reset one minute into a five hour window is plausible")
}

fn known(raw: f64) -> QuotaWindow {
    QuotaWindow::Known {
        utilization: Utilization::new(raw).expect("raw fraction is in range"),
        reset_at: reset_at(),
    }
}

#[test]
fn unknown_is_not_equal_to_a_known_zero_reading() {
    let unobserved = QuotaWindow::Unknown;

    let observed_empty = known(0.0);

    assert_ne!(
        unobserved, observed_empty,
        "an unobserved window and a reported empty window are different facts"
    );
}

#[test]
fn a_zero_reading_is_a_valid_known_observation() {
    let window = known(0.0);

    let QuotaWindow::Known { utilization, .. } = window else {
        panic!("a zero fraction must stay Known, not collapse to Unknown");
    };

    assert_eq!(utilization.fraction(), 0.0);
}

#[test]
fn utilization_accepts_both_ends_of_the_range() {
    let empty = Utilization::new(0.0).expect("0.0 is in range");
    let exhausted = Utilization::new(1.0).expect("1.0 is in range");

    assert_eq!(empty.fraction(), 0.0);
    assert_eq!(exhausted.fraction(), 1.0);
}

#[test]
fn utilization_refuses_a_negative_fraction() {
    assert!(Utilization::new(-0.01).is_none());
    assert!(Utilization::new(-1.0).is_none());
}

#[test]
fn utilization_refuses_non_finite_input() {
    assert!(Utilization::new(f64::NAN).is_none());
    assert!(Utilization::new(f64::INFINITY).is_none());
    assert!(Utilization::new(f64::NEG_INFINITY).is_none());
}

#[test]
fn utilization_above_one_saturates_to_exhausted_and_never_to_empty() {
    let over_reported =
        Utilization::new(1.4).expect("over-limit input saturates rather than fails");

    assert_eq!(
        over_reported.fraction(),
        1.0,
        "an upstream reporting over its own limit means exhausted"
    );
    assert_ne!(
        over_reported.fraction(),
        0.0,
        "saturating to zero would invent headroom on a seat that reported none"
    );
}

#[test]
fn utilization_saturation_holds_for_a_wildly_over_range_value() {
    let percent_mistaken_for_fraction =
        Utilization::new(87.0).expect("an out-of-scale finite input saturates");

    assert_eq!(percent_mistaken_for_fraction.fraction(), 1.0);
}

/// Runtime answer to "does `T` implement `Default`?".
///
/// Rust cannot express a negative trait bound, so absence of an impl is
/// detected through autoref method resolution: the inherent method on
/// `Probe<T>` is only a candidate when `T: Default`, and otherwise resolution
/// falls through to the trait impl on `&Probe<T>`. Written out rather than
/// left to review because "no `Default`" is the guard that stops a future call
/// site from constructing "known 0%" by accident, and a guard nobody re-checks
/// is not a guard.
mod default_probe {
    use std::marker::PhantomData;

    pub struct Probe<T>(pub PhantomData<T>);

    pub trait DefaultAbsent {
        fn probe_has_default(&self) -> bool;
    }

    impl<T> DefaultAbsent for &Probe<T> {
        fn probe_has_default(&self) -> bool {
            false
        }
    }

    impl<T: Default> Probe<T> {
        pub fn probe_has_default(&self) -> bool {
            true
        }
    }
}

macro_rules! has_default {
    ($t:ty) => {{ (&default_probe::Probe::<$t>(std::marker::PhantomData)).probe_has_default() }};
}

#[test]
fn no_quota_value_type_implements_default() {
    use default_probe::DefaultAbsent as _;

    assert!(
        has_default!(f64),
        "the probe must detect a type that does implement Default"
    );

    assert!(!has_default!(Utilization));
    assert!(!has_default!(QuotaWindow));
    assert!(!has_default!(WindowRole));
    assert!(!has_default!(Billing));
}

#[test]
fn billing_distinguishes_unknown_from_known_benign() {
    assert_ne!(
        Billing::Unknown,
        Billing::Included,
        "a missing claim is not evidence of included billing"
    );
    assert_ne!(Billing::Included, Billing::Overage);
    assert_ne!(Billing::Unknown, Billing::Overage);
}

#[test]
fn the_two_window_roles_are_distinct() {
    assert_ne!(WindowRole::Fast, WindowRole::Slow);
}
