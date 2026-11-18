//! Tests for the account meter's reading model: length classification at its
//! boundaries, the started-state rules, utilization bounds, scope layout, and
//! the window-set invariants.

use super::*;

use std::time::Instant;

const FIVE_HOURS_SECS: u64 = 5 * 3_600;
const SEVEN_DAYS_SECS: u64 = 7 * 86_400;

fn observed() -> ObservationStamp {
    ObservationStamp::from_parts(
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_000_000),
        Instant::now(),
    )
}

fn key(length_secs: u64, scope: Option<&str>) -> WindowKey {
    let scope = scope.map(|raw| Scope::new(raw).expect("a canonical scope"));
    WindowKey::new(length_secs, scope).expect("a classifiable length")
}

/// A started window at `used`, resetting one hour after `observed`.
fn started(
    key: WindowKey,
    used: f64,
    observed: &ObservationStamp,
) -> Result<WindowReading, RejectionReason> {
    let reset = observed.wall() + Duration::from_hours(1);
    WindowReading::new(key, used, Some(reset), Started::Yes, observed)
}

fn window(length_secs: u64, scope: Option<&str>) -> WindowReading {
    started(key(length_secs, scope), 0.25, &observed()).expect("a valid window")
}

// ---- Length classification ----

#[test]
fn window_length_classifies_at_both_boundaries() {
    let rows: [(&str, u64, Result<LengthClass, RejectionReason>); 6] = [
        ("one second", 1, Ok(LengthClass::Short)),
        ("exactly 24 h", 86_400, Ok(LengthClass::Short)),
        ("24 h + 1 s", 86_401, Ok(LengthClass::Long)),
        ("exactly 31 d", 31 * 86_400, Ok(LengthClass::Long)),
        ("zero", 0, Err(RejectionReason::UnknownLength)),
        (
            "31 d + 1 s",
            31 * 86_400 + 1,
            Err(RejectionReason::UnknownLength),
        ),
    ];

    for (name, length_secs, expected) in rows {
        assert_eq!(LengthClass::of(length_secs), expected, "row: {name}");
        assert_eq!(
            WindowKey::new(length_secs, None).map(|k| k.class()),
            expected,
            "row (key): {name}"
        );
    }
}

// ---- Started state ----

#[test]
fn an_unstarted_window_with_usage_is_refused() {
    let refused = WindowReading::new(
        key(FIVE_HOURS_SECS, None),
        0.01,
        None,
        Started::No,
        &observed(),
    );

    assert_eq!(refused, Err(RejectionReason::InvalidStarted));
}

#[test]
fn an_unstarted_window_at_zero_without_a_reset_is_accepted() {
    let reading = WindowReading::new(
        key(FIVE_HOURS_SECS, None),
        0.0,
        None,
        Started::No,
        &observed(),
    )
    .expect("the unstarted shape is valid");

    assert_eq!(reading.started(), Started::No);
    assert_eq!(reading.used().fraction(), 0.0);
    assert_eq!(reading.reset(), None);
}

/// The shape a source reports for a window whose clock starts on first use:
/// the reset sits exactly one window length after the observation.
#[test]
fn an_unstarted_window_with_a_reset_one_length_out_is_accepted_and_stays_unstarted() {
    let observed = observed();
    let reset = observed.wall() + Duration::from_secs(SEVEN_DAYS_SECS);

    let reading = WindowReading::new(
        key(SEVEN_DAYS_SECS, None),
        0.0,
        Some(reset),
        Started::No,
        &observed,
    )
    .expect("the sliding unstarted shape is valid");

    assert_eq!(reading.started(), Started::No);
    assert_eq!(reading.used().fraction(), 0.0);
    assert_eq!(reading.reset().map(ValidatedReset::at), Some(reset));
}

/// An unstarted reset goes through the same bound as a running one, so a
/// unit error cannot ride in on the unstarted path.
#[test]
fn an_unstarted_reset_is_bounded_by_its_own_window_length() {
    let observed = observed();
    let length = Duration::from_secs(FIVE_HOURS_SECS);
    let rows: [(&str, Duration, Result<(), RejectionReason>); 4] = [
        ("at length + tolerance", length + RESET_TOLERANCE, Ok(())),
        (
            "one second past length + tolerance",
            length + RESET_TOLERANCE + Duration::from_secs(1),
            Err(RejectionReason::ImplausibleReset),
        ),
        (
            "seven days on a five-hour window",
            Duration::from_secs(SEVEN_DAYS_SECS),
            Err(RejectionReason::ImplausibleReset),
        ),
        (
            "equal to the observation",
            Duration::ZERO,
            Err(RejectionReason::ExpiredReset),
        ),
    ];

    for (name, offset, expected) in rows {
        let reset = observed.wall() + offset;
        let result = WindowReading::new(
            key(FIVE_HOURS_SECS, None),
            0.0,
            Some(reset),
            Started::No,
            &observed,
        );

        assert_eq!(
            result.map(|r| assert_eq!(r.reset().map(ValidatedReset::at), Some(reset))),
            expected,
            "row: {name}"
        );
    }
}

#[test]
fn an_unstarted_reset_whose_bound_overflows_is_refused() {
    let edge = SystemTime::UNIX_EPOCH + Duration::from_secs(i64::MAX as u64 - 10);
    let observed = ObservationStamp::from_parts(edge, Instant::now());

    let refused = WindowReading::new(
        key(FIVE_HOURS_SECS, None),
        0.0,
        Some(edge + Duration::from_secs(1)),
        Started::No,
        &observed,
    );

    assert_eq!(refused, Err(RejectionReason::Overflow));
}

#[test]
fn an_unstarted_window_with_usage_and_a_reset_is_refused() {
    let observed = observed();
    let reset = observed.wall() + Duration::from_secs(FIVE_HOURS_SECS);

    let refused = WindowReading::new(
        key(FIVE_HOURS_SECS, None),
        0.01,
        Some(reset),
        Started::No,
        &observed,
    );

    assert_eq!(refused, Err(RejectionReason::InvalidStarted));
}

#[test]
fn a_running_window_without_a_reset_is_refused() {
    for started in [Started::Yes, Started::Unknown] {
        let refused =
            WindowReading::new(key(FIVE_HOURS_SECS, None), 0.4, None, started, &observed());

        assert_eq!(refused, Err(RejectionReason::InvalidStarted), "{started:?}");
    }
}

/// A seven-day reset is plausible for a seven-day window and implausible for a
/// five-hour one, so this pins that the bound is the window's OWN length.
#[test]
fn a_started_reset_is_bounded_by_its_own_window_length() {
    let observed = observed();
    let reset = observed.wall() + Duration::from_hours(6 * 24);

    let long = WindowReading::new(
        key(SEVEN_DAYS_SECS, None),
        0.4,
        Some(reset),
        Started::Yes,
        &observed,
    );
    let short = WindowReading::new(
        key(FIVE_HOURS_SECS, None),
        0.4,
        Some(reset),
        Started::Yes,
        &observed,
    );

    assert_eq!(
        long.map(|r| r.reset().map(ValidatedReset::at)),
        Ok(Some(reset))
    );
    assert_eq!(short, Err(RejectionReason::ImplausibleReset));
}

#[test]
fn a_started_reset_already_passed_is_refused() {
    let observed = observed();

    let refused = WindowReading::new(
        key(FIVE_HOURS_SECS, None),
        0.4,
        Some(observed.wall()),
        Started::Yes,
        &observed,
    );

    assert_eq!(refused, Err(RejectionReason::ExpiredReset));
}

// ---- Used fraction ----

#[test]
fn used_above_one_saturates_to_exhausted() {
    let reading = started(key(FIVE_HOURS_SECS, None), 1.37, &observed()).expect("saturates");

    assert_eq!(reading.used().fraction(), 1.0);
}

#[test]
fn used_that_is_not_a_reading_is_refused() {
    let rows = [
        ("NaN", f64::NAN),
        ("negative", -0.01),
        ("infinite", f64::INFINITY),
    ];

    for (name, used) in rows {
        assert_eq!(
            started(key(FIVE_HOURS_SECS, None), used, &observed()),
            Err(RejectionReason::InvalidUtilization),
            "row: {name}"
        );
    }
}

// ---- Scope ----

#[test]
fn scope_accepts_only_the_canonical_layout() {
    let at_bound = "a".repeat(MAX_SCOPE_BYTES);
    let over_bound = "a".repeat(MAX_SCOPE_BYTES + 1);
    let rows: [(&str, &str, bool); 9] = [
        ("lowercase tag", "opus", true),
        ("digits and underscore", "seven_day_4", true),
        ("at the byte bound", &at_bound, true),
        ("empty", "", false),
        ("over the byte bound", &over_bound, false),
        ("uppercase", "Opus", false),
        ("hyphen", "seven-day", false),
        ("space", "seven day", false),
        ("non-ASCII", "s\u{e9}ven", false),
    ];

    for (name, raw, accepted) in rows {
        let result = Scope::new(raw);
        if accepted {
            assert_eq!(result.as_ref().map(Scope::as_str), Ok(raw), "row: {name}");
        } else {
            assert_eq!(result, Err(RejectionReason::MalformedBody), "row: {name}");
        }
    }
}

// ---- Window set ----

#[test]
fn two_same_length_windows_under_different_scopes_are_both_accepted() {
    let set = WindowSet::new(vec![
        window(SEVEN_DAYS_SECS, None),
        window(SEVEN_DAYS_SECS, Some("opus")),
    ])
    .expect("distinct keys");

    assert_eq!(set.as_slice().len(), 2);
    assert_ne!(set.as_slice()[0].key(), set.as_slice()[1].key());
}

#[test]
fn a_duplicate_window_key_is_refused() {
    let rows = [("account-wide", None), ("scoped", Some("opus"))];

    for (name, scope) in rows {
        let refused = WindowSet::new(vec![
            window(FIVE_HOURS_SECS, scope),
            window(SEVEN_DAYS_SECS, Some("other")),
            window(FIVE_HOURS_SECS, scope),
        ]);

        assert_eq!(refused, Err(RejectionReason::MalformedBody), "row: {name}");
    }
}

#[test]
fn window_count_is_bounded() {
    let windows = |n: u64| -> Vec<WindowReading> {
        (1..=n).map(|hours| window(hours * 3_600, None)).collect()
    };
    let max = MAX_WINDOWS_PER_OBSERVATION as u64;

    assert!(WindowSet::new(windows(max)).is_ok(), "at the bound");
    assert_eq!(
        WindowSet::new(windows(max + 1)),
        Err(RejectionReason::MalformedBody)
    );
}

#[test]
fn an_empty_window_set_is_refused() {
    assert_eq!(
        WindowSet::new(Vec::new()),
        Err(RejectionReason::MalformedBody)
    );
}

// ---- Observation stamp ----

fn later(stamp: &ObservationStamp, wall: Duration, monotonic: Duration) -> ObservationStamp {
    ObservationStamp::from_parts(stamp.wall() + wall, stamp.monotonic() + monotonic)
}

fn set_at(stamps: &[ObservationStamp]) -> WindowSet {
    let windows = stamps
        .iter()
        .zip(1..)
        .map(|(stamp, hours)| started(key(hours * 3_600, None), 0.25, stamp).expect("valid"))
        .collect();
    WindowSet::new(windows).expect("distinct keys")
}

#[test]
fn an_observation_whose_windows_share_its_stamp_is_accepted() {
    let observed = observed();
    let reports = [
        (
            "partial",
            ObservationReport::Partial(set_at(&[observed, observed])),
        ),
        (
            "complete",
            ObservationReport::Complete(set_at(&[observed, observed])),
        ),
        ("no windows", ObservationReport::NoWindows),
    ];

    for (name, report) in reports {
        let observation = AccountObservation::new(ReadingSource::Poll, observed, report.clone())
            .unwrap_or_else(|e| panic!("row {name}: {e:?}"));

        assert_eq!(observation.report(), &report, "row: {name}");
    }
}

/// A window validated against a later stamp was bounded from a later instant,
/// so attaching it to an earlier observation would stretch its reset.
#[test]
fn an_observation_carrying_a_window_from_another_stamp_is_refused() {
    let observed = observed();
    let rows = [
        (
            "later on both clocks",
            later(&observed, Duration::from_mins(1), Duration::from_mins(1)),
        ),
        (
            "same wall, later monotonic",
            later(&observed, Duration::ZERO, Duration::from_secs(1)),
        ),
    ];

    for (name, other) in rows {
        for (shape, stamps) in [
            ("only window", vec![other]),
            ("second window", vec![observed, other]),
        ] {
            let reports = [
                ObservationReport::Partial(set_at(&stamps)),
                ObservationReport::Complete(set_at(&stamps)),
            ];
            for report in reports {
                let refused = AccountObservation::new(ReadingSource::Response, observed, report);

                assert_eq!(
                    refused.map(|_| ()),
                    Err(RejectionReason::MalformedBody),
                    "row: {name}, {shape}"
                );
            }
        }
    }
}

// ---- Derive discipline ----

/// Runtime answers to "does `T` implement this trait?", by autoref method
/// resolution: the inherent `found` on a probe is a candidate only when `T`
/// implements the trait; otherwise resolution falls through to the trait impl
/// on `&Probe<T>`, which answers false. The derive rule in `quota/mod.rs` is a
/// guard, and a guard nobody re-checks is not one.
mod trait_probe {
    use std::marker::PhantomData;

    pub struct DefaultProbe<T>(pub PhantomData<T>);
    pub struct OrdProbe<T>(pub PhantomData<T>);
    pub struct SerializeProbe<T>(pub PhantomData<T>);

    pub trait DefaultAbsent {
        fn found(&self) -> bool {
            false
        }
    }
    pub trait OrdAbsent {
        fn found(&self) -> bool {
            false
        }
    }
    pub trait SerializeAbsent {
        fn found(&self) -> bool {
            false
        }
    }
    impl<T> DefaultAbsent for &DefaultProbe<T> {}
    impl<T> OrdAbsent for &OrdProbe<T> {}
    impl<T> SerializeAbsent for &SerializeProbe<T> {}

    impl<T: Default> DefaultProbe<T> {
        pub const fn found(&self) -> bool {
            true
        }
    }
    impl<T: Ord> OrdProbe<T> {
        pub const fn found(&self) -> bool {
            true
        }
    }
    impl<T: serde::Serialize> SerializeProbe<T> {
        pub const fn found(&self) -> bool {
            true
        }
    }
}

macro_rules! implements {
    ($probe:ident, $t:ty) => {{ (&trait_probe::$probe::<$t>(std::marker::PhantomData)).found() }};
}

macro_rules! assert_minimal_derives {
    ($($t:ty),+ $(,)?) => {$(
        assert!(!implements!(DefaultProbe, $t), "{} derives Default", stringify!($t));
        assert!(!implements!(OrdProbe, $t), "{} derives Ord", stringify!($t));
        assert!(!implements!(SerializeProbe, $t), "{} derives Serialize", stringify!($t));
    )+};
}

#[test]
fn no_reading_type_derives_default_ord_or_serialize() {
    use trait_probe::{DefaultAbsent as _, OrdAbsent as _, SerializeAbsent as _};

    assert!(
        implements!(DefaultProbe, u64),
        "the Default probe detects an impl"
    );
    assert!(implements!(OrdProbe, u64), "the Ord probe detects an impl");
    assert!(
        implements!(SerializeProbe, u64),
        "the Serialize probe detects an impl"
    );

    assert_minimal_derives!(
        LengthClass,
        Scope,
        WindowKey,
        Started,
        ReadingSource,
        WindowReading,
        WindowSet,
        ObservationReport,
        AccountObservation,
    );
}
