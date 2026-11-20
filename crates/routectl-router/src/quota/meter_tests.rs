//! Tests for the account meter's merge rule, through the store's two entry
//! points, and for the placement-safe view placement reads.

use std::time::{Instant, SystemTime};

use super::*;

use crate::quota::curation::{CODEX_PROVIDER_KIND, rows_for};
use crate::quota::freshness::accept_reset;
use crate::quota::key::{SeatKey, seat_key_for_secret_ref};
use crate::quota::reading::Scope;
use crate::quota::store::{ObserveRefusal, QuotaStore};
use crate::quota::window::Utilization;

pub(super) const FIVE_HOURS: u64 = 5 * 3_600;
pub(super) const SEVEN_DAYS: u64 = 7 * 86_400;

pub(super) fn base_wall() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_000_000)
}

/// Stamps `offset` past one base on both clocks, ordered alike.
pub(super) struct Clock {
    monotonic: Instant,
}

impl Clock {
    pub(super) fn new() -> Self {
        Self {
            monotonic: Instant::now(),
        }
    }

    pub(super) fn at(&self, offset: Duration) -> ObservationStamp {
        ObservationStamp::from_parts(base_wall() + offset, self.monotonic + offset)
    }
}

pub(super) fn mins(n: u64) -> Duration {
    Duration::from_mins(n)
}

pub(super) fn seat(label: &str) -> SeatKey {
    let secret_ref = routectl_auth::SecretRef::OAuth {
        provider: "anthropic".to_string(),
        label: Some(label.to_string()),
    };
    seat_key_for_secret_ref(Some(&secret_ref)).expect("an oauth ref yields a key")
}

pub(super) fn store() -> QuotaStore {
    let store = QuotaStore::default();
    store.admit_seats([seat("a")]);
    store
}

pub(super) fn key(length_secs: u64, scope: Option<&str>) -> WindowKey {
    let scope = scope.map(|raw| Scope::new(raw).expect("a canonical scope"));
    WindowKey::new(length_secs, scope).expect("a classifiable length")
}

/// A running window at `used`, resetting `reset` past the base wall clock, so
/// two observations name the same reset by passing the same offset.
pub(super) fn running(
    key: WindowKey,
    used: f64,
    reset: Duration,
    at: &ObservationStamp,
) -> WindowReading {
    WindowReading::new(key, used, Some(base_wall() + reset), Started::Yes, at)
        .expect("a valid window")
}

pub(super) fn observation(
    source: ReadingSource,
    at: ObservationStamp,
    report: ObservationReport,
) -> AccountObservation {
    AccountObservation::new(source, at, report).expect("one stamp throughout")
}

pub(super) fn partial(
    source: ReadingSource,
    at: ObservationStamp,
    windows: Vec<WindowReading>,
) -> AccountObservation {
    observation(
        source,
        at,
        ObservationReport::Partial(WindowSet::new(windows).expect("a valid set")),
    )
}

/// `meter` with its correction streaks cleared: the accepted state a refusal
/// must leave unchanged.
pub(super) fn accepted_state(meter: Option<AccountMeter>) -> Option<AccountMeter> {
    let mut meter = meter?;
    meter.streaks.clear();
    Some(meter)
}

fn used_of(store: &QuotaStore, key: &WindowKey) -> Option<f64> {
    store
        .account(&seat("a"))?
        .window(key)
        .map(|w| w.reading().used().fraction())
}

/// A header snapshot carrying a FAST reading at `used`, resetting `reset`
/// past the base wall clock.
pub(super) fn header(at: ObservationStamp, used: f64, reset: Duration) -> QuotaSnapshot {
    QuotaSnapshot {
        observed: at,
        fast: QuotaWindow::Known {
            utilization: Utilization::new(used).expect("a valid fraction"),
            reset_at: accept_reset(
                base_wall() + reset,
                &at,
                Duration::from_secs(FIVE_HOURS),
                RESET_TOLERANCE,
            )
            .expect("a plausible reset"),
        },
        slow: QuotaWindow::Unknown,
        billing: Billing::Unknown,
    }
}

// ---- Persistence across observations ----

#[test]
fn windows_from_different_observations_persist_and_a_partial_never_erases() {
    let clock = Clock::new();
    let store = store();
    let (t0, t1, t2) = (clock.at(mins(0)), clock.at(mins(1)), clock.at(mins(2)));
    let five = key(FIVE_HOURS, None);
    let seven = key(SEVEN_DAYS, None);
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t0,
                vec![running(five.clone(), 0.10, mins(60), &t0)],
            ),
        )
        .expect("accepted");
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t1,
                vec![running(seven.clone(), 0.30, mins(600), &t1)],
            ),
        )
        .expect("accepted");

    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Response,
                t2,
                vec![running(five.clone(), 0.20, mins(60), &t2)],
            ),
        )
        .expect("accepted");

    assert_eq!(used_of(&store, &five), Some(0.20));
    assert_eq!(
        used_of(&store, &seven),
        Some(0.30),
        "a window the later partial omitted is unchanged"
    );
    let seven_stored = store.account(&seat("a")).expect("an entry");
    let seven_stored = seven_stored.window(&seven).expect("still held");
    assert_eq!(
        *seven_stored.observed(),
        t1,
        "the omitted window keeps aging on its own stamp"
    );
    assert_eq!(seven_stored.source(), ReadingSource::Poll);
}

// ---- Report state ----

#[test]
fn report_state_moves_from_no_reading_to_no_windows_to_windows() {
    let clock = Clock::new();
    let store = store();
    let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
    assert_eq!(store.report_for(&seat("a")), ReportState::NoReading);

    store
        .observe_account(
            &seat("a"),
            &observation(ReadingSource::Poll, t0, ObservationReport::NoWindows),
        )
        .expect("accepted");
    let meter = store.account(&seat("a")).expect("an entry");
    assert_eq!(meter.report(), ReportState::NoWindows);
    assert_eq!(meter.windows().count(), 0);

    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Response,
                t1,
                vec![running(key(FIVE_HOURS, None), 0.10, mins(60), &t1)],
            ),
        )
        .expect("accepted");
    assert_eq!(store.report_for(&seat("a")), ReportState::Windows);
}

#[test]
fn a_complete_report_drops_older_omitted_windows_and_bars_late_ones() {
    let clock = Clock::new();
    let store = store();
    let (t0, t1, t2) = (clock.at(mins(0)), clock.at(mins(1)), clock.at(mins(2)));
    let five = key(FIVE_HOURS, None);
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Response,
                t0,
                vec![running(five.clone(), 0.10, mins(60), &t0)],
            ),
        )
        .expect("accepted");

    store
        .observe_account(
            &seat("a"),
            &observation(ReadingSource::Poll, t2, ObservationReport::NoWindows),
        )
        .expect("accepted");
    assert_eq!(
        used_of(&store, &five),
        None,
        "the complete report says it no longer applies"
    );

    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Response,
                t1,
                vec![running(five.clone(), 0.15, mins(60), &t1)],
            ),
        )
        .expect("a late arrival is not a refusal");
    assert_eq!(
        store.report_for(&seat("a")),
        ReportState::NoWindows,
        "a window observed before the complete report must not come back"
    );
}

/// A complete report that omits the five-hour window drops a running one: no
/// five-hour evidence is left, never room.
#[test]
fn a_complete_report_omitting_the_five_hour_window_drops_it() {
    let rows: [(&str, bool); 2] = [("weekly only", true), ("no windows", false)];
    for (name, reports_weekly) in rows {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
        seed(&store, &Before::PollRunning, t0);
        let report = if reports_weekly {
            ObservationReport::Complete(
                WindowSet::new(vec![running(key(SEVEN_DAYS, None), 0.20, mins(3_000), &t1)])
                    .expect("a set"),
            )
        } else {
            ObservationReport::NoWindows
        };

        // Act
        store
            .observe_account(&seat("a"), &observation(ReadingSource::Poll, t1, report))
            .expect("accepted");

        // Assert
        assert_eq!(used_of(&store, &key(FIVE_HOURS, None)), None, "row: {name}");
    }
}

/// One step of a tie scenario: a partial or complete report at a stamp.
enum Step {
    Partial(ObservationStamp, Vec<(WindowKey, f64)>),
    Complete(ObservationStamp, Vec<(WindowKey, f64)>),
    NoWindows(ObservationStamp),
}

fn apply(store: &QuotaStore, step: &Step) {
    let readings = |at: &ObservationStamp, windows: &[(WindowKey, f64)]| {
        windows
            .iter()
            .map(|(k, used)| running(k.clone(), *used, mins(60), at))
            .collect::<Vec<_>>()
    };
    let observation = match step {
        Step::Partial(at, windows) => partial(ReadingSource::Response, *at, readings(at, windows)),
        Step::Complete(at, windows) => observation(
            ReadingSource::Poll,
            *at,
            ObservationReport::Complete(WindowSet::new(readings(at, windows)).expect("a set")),
        ),
        Step::NoWindows(at) => observation(ReadingSource::Poll, *at, ObservationReport::NoWindows),
    };
    store
        .observe_account(&seat("a"), &observation)
        .expect("an ordering decision is never a refusal");
}

/// A tie scenario: name, steps, then the expected report state and the used
/// fractions held for the five-hour and seven-day keys.
type TieRow = (
    &'static str,
    Vec<Step>,
    ReportState,
    Option<f64>,
    Option<f64>,
);

/// Stamps are not unique, so a tie needs a rule: a complete report wins
/// against every window stamped equal to it (an omitted one is dropped, a
/// partial arriving after it is barred), while its own windows stay; between
/// two complete reports the later merged one wins. Equality
/// is on the monotonic clock alone; the wall clock orders nothing.
#[test]
fn an_equal_stamp_tie_goes_to_the_complete_report() {
    let clock = Clock::new();
    let s = clock.at(mins(5));
    let same_tick_other_wall = ObservationStamp::from_parts(s.wall() + mins(1), s.monotonic());
    let earlier = clock.at(mins(4));
    let five = || key(FIVE_HOURS, None);
    let seven = || key(SEVEN_DAYS, None);
    let rows: Vec<TieRow> = vec![
        (
            "partial then NoWindows",
            vec![Step::Partial(s, vec![(five(), 0.10)]), Step::NoWindows(s)],
            ReportState::NoWindows,
            None,
            None,
        ),
        (
            "NoWindows then partial",
            vec![Step::NoWindows(s), Step::Partial(s, vec![(five(), 0.10)])],
            ReportState::NoWindows,
            None,
            None,
        ),
        (
            "NoWindows then partial on the same tick with another wall clock",
            vec![
                Step::NoWindows(s),
                Step::Partial(same_tick_other_wall, vec![(five(), 0.10)]),
            ],
            ReportState::NoWindows,
            None,
            None,
        ),
        (
            "partial then complete omitting it",
            vec![
                Step::Partial(s, vec![(five(), 0.10)]),
                Step::Complete(s, vec![(seven(), 0.30)]),
            ],
            ReportState::Windows,
            None,
            Some(0.30),
        ),
        (
            "complete then partial it omitted",
            vec![
                Step::Complete(s, vec![(seven(), 0.30)]),
                Step::Partial(s, vec![(five(), 0.10)]),
            ],
            ReportState::Windows,
            None,
            Some(0.30),
        ),
        (
            "partial then complete reporting the same key",
            vec![
                Step::Partial(s, vec![(five(), 0.10)]),
                Step::Complete(s, vec![(five(), 0.30)]),
            ],
            ReportState::Windows,
            Some(0.30),
            None,
        ),
        (
            "complete then partial for a key it reported",
            vec![
                Step::Complete(s, vec![(five(), 0.30)]),
                Step::Partial(s, vec![(five(), 0.40)]),
            ],
            ReportState::Windows,
            Some(0.30),
            None,
        ),
        (
            "NoWindows then complete with windows",
            vec![Step::NoWindows(s), Step::Complete(s, vec![(five(), 0.10)])],
            ReportState::Windows,
            Some(0.10),
            None,
        ),
        (
            "complete with windows then NoWindows",
            vec![Step::Complete(s, vec![(five(), 0.10)]), Step::NoWindows(s)],
            ReportState::NoWindows,
            None,
            None,
        ),
        (
            "complete with windows drops an older omitted one",
            vec![
                Step::Partial(earlier, vec![(five(), 0.10)]),
                Step::Complete(s, vec![(seven(), 0.30)]),
            ],
            ReportState::Windows,
            None,
            Some(0.30),
        ),
    ];
    for (name, steps, report, five_used, seven_used) in rows {
        let store = store();

        for step in &steps {
            apply(&store, step);
        }

        assert_eq!(store.report_for(&seat("a")), report, "row: {name}");
        assert_eq!(used_of(&store, &five()), five_used, "row: {name}");
        assert_eq!(used_of(&store, &seven()), seven_used, "row: {name}");
    }
}

// ---- Ordering after an unknown header ----

/// A header naming a window unknown after the stored one expired (or with
/// none stored) speaks for that window at its own stamp: a reading observed
/// before it, from any source and for either curated role, never lands after
/// it. A reading observed after it still does.
#[test]
fn an_unknown_header_bars_older_readings_of_an_ineffective_window() {
    let clock = Clock::new();
    let (t0, t2, t3, t4) = (
        clock.at(mins(0)),
        clock.at(mins(90)),
        clock.at(mins(120)),
        clock.at(mins(130)),
    );
    let all_unknown = |at| QuotaSnapshot {
        observed: at,
        fast: QuotaWindow::Unknown,
        slow: QuotaWindow::Unknown,
        billing: Billing::Unknown,
    };
    let rows = [
        ("expired FAST, delayed poll", FIVE_HOURS, true, t2, None),
        ("expired SLOW, delayed poll", SEVEN_DAYS, true, t2, None),
        ("nothing stored, delayed poll", FIVE_HOURS, false, t2, None),
        ("expired FAST, newer poll", FIVE_HOURS, true, t4, Some(0.10)),
    ];
    for (name, length, seeded, delayed_at, expected) in rows {
        let store = store();
        let window = key(length, None);
        if seeded {
            apply_seed(&store, &window, t0);
        }
        assert!(store.observe(&seat("a"), all_unknown(t3)), "row: {name}");

        store
            .observe_account(
                &seat("a"),
                &partial(
                    ReadingSource::Poll,
                    delayed_at,
                    vec![running(window.clone(), 0.10, mins(240), &delayed_at)],
                ),
            )
            .expect("a late arrival is not a refusal");

        let held = store
            .account(&seat("a"))
            .and_then(|m| m.window(&window).cloned())
            .filter(|w| *w.observed() == delayed_at)
            .map(|w| w.reading().used().fraction());
        assert_eq!(held, expected, "row: {name}");
    }
}

/// A window stored at `at` running 0.70 and resetting 60 minutes past the
/// base, so it has expired by the time the tests' unknown header arrives.
fn apply_seed(store: &QuotaStore, window: &WindowKey, at: ObservationStamp) {
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                at,
                vec![running(window.clone(), 0.70, mins(60), &at)],
            ),
        )
        .expect("accepted");
}

/// An unknown header leaves no mark while the stored window is still in
/// effect, exactly as before: a delayed reading newer than the stored one
/// still lands.
#[test]
fn an_unknown_header_over_an_effective_window_leaves_ordering_alone() {
    let clock = Clock::new();
    let (t0, t1, t2) = (clock.at(mins(0)), clock.at(mins(10)), clock.at(mins(20)));
    let store = store();
    assert!(store.observe(&seat("a"), header(t0, 0.50, mins(60))));
    assert!(store.observe(
        &seat("a"),
        QuotaSnapshot {
            observed: t2,
            fast: QuotaWindow::Unknown,
            slow: QuotaWindow::Unknown,
            billing: Billing::Unknown,
        }
    ));

    assert!(store.observe(&seat("a"), header(t1, 0.60, mins(61))));

    assert_eq!(used_of(&store, &key(FIVE_HOURS, None)), Some(0.60));
}

// ---- Contradiction rule ----

#[test]
fn a_same_reset_drop_is_accepted_within_the_slack_and_refused_beyond_it() {
    let rows = [
        ("1 point lower", 0.46, Some(0.47)),
        ("2 points lower", 0.45, Some(0.47)),
        ("3 points lower", 0.44, None),
        ("higher", 0.60, Some(0.60)),
    ];
    for (name, incoming, expected) in rows {
        let clock = Clock::new();
        let store = store();
        let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
        let five = key(FIVE_HOURS, None);
        store
            .observe_account(
                &seat("a"),
                &partial(
                    ReadingSource::Poll,
                    t0,
                    vec![running(five.clone(), 0.47, mins(60), &t0)],
                ),
            )
            .expect("accepted");
        let before = accepted_state(store.account(&seat("a")));

        let result = store.observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Response,
                t1,
                vec![running(five.clone(), incoming, mins(61), &t1)],
            ),
        );

        let after = store.account(&seat("a")).expect("an entry");
        match expected {
            Some(used) => {
                assert_eq!(result, Ok(()), "row: {name}");
                assert_eq!(used_of(&store, &five), Some(used), "row: {name}");
                let last = after.last_accepted().expect("an accepted reading");
                assert_eq!(last.observed, t1, "row: {name}: accepted time advances");
                assert_eq!(last.source, ReadingSource::Response, "row: {name}");
                assert_eq!(store.rejection_totals().contradictory, 0, "row: {name}");
            }
            None => {
                assert_eq!(
                    result,
                    Err(ObserveRefusal::Rejected(RejectionReason::Contradictory)),
                    "row: {name}"
                );
                assert_eq!(
                    accepted_state(Some(after)),
                    before,
                    "row: {name}: accepted state identical"
                );
                assert_eq!(
                    store.rejection_totals().contradictory,
                    1,
                    "row: {name}: counted once"
                );
            }
        }
    }
}

/// A reset outside the tolerance does not make a lower reading a new window
/// while the stored one is still in effect: it is refused like a same-reset
/// drop until it confirms.
#[test]
fn a_drop_with_a_different_reset_over_a_window_in_effect_is_a_contradiction() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
    let five = key(FIVE_HOURS, None);
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t0,
                vec![running(five.clone(), 0.90, mins(60), &t0)],
            ),
        )
        .expect("accepted");
    let before = accepted_state(store.account(&seat("a")));

    // Act
    let result = store.observe_account(
        &seat("a"),
        &partial(
            ReadingSource::Poll,
            t1,
            vec![running(five.clone(), 0.05, mins(60) + mins(6), &t1)],
        ),
    );

    // Assert
    assert_eq!(
        result,
        Err(ObserveRefusal::Rejected(RejectionReason::Contradictory))
    );
    assert_eq!(used_of(&store, &five), Some(0.90));
    assert_eq!(store.rejection_totals().contradictory, 1);
    assert_eq!(accepted_state(store.account(&seat("a"))), before);
}

/// What is stored before an unstarted five-hour reading arrives.
enum Before {
    Nothing,
    PollRunning,
    HeaderRunning,
    Unstarted,
}

fn seed(store: &QuotaStore, before: &Before, at: ObservationStamp) {
    let five = key(FIVE_HOURS, None);
    match before {
        Before::Nothing => {}
        Before::PollRunning => store
            .observe_account(
                &seat("a"),
                &partial(
                    ReadingSource::Poll,
                    at,
                    vec![running(five, 0.95, mins(180), &at)],
                ),
            )
            .expect("accepted"),
        Before::HeaderRunning => assert!(store.observe(&seat("a"), header(at, 0.95, mins(180)))),
        Before::Unstarted => store
            .observe_account(
                &seat("a"),
                &partial(ReadingSource::Poll, at, vec![unstarted(five, None, &at)]),
            )
            .expect("accepted"),
    }
}

pub(super) fn unstarted(
    key: WindowKey,
    reset: Option<Duration>,
    at: &ObservationStamp,
) -> WindowReading {
    WindowReading::new(key, 0.0, reset.map(|r| base_wall() + r), Started::No, at)
        .expect("a valid unstarted window")
}

/// One unstarted-reading case: name, what is stored at minute 0, the minute
/// the unstarted reading arrives, its provisional reset, and whether it is
/// taken.
type UnstartedRow = (&'static str, Before, u64, Option<Duration>, bool);

/// A window cannot stop before its reset, so an unstarted reading over a
/// running window still in effect is a contradiction and never makes room;
/// once that window's reset has passed, a new window may be unstarted. An
/// unstarted reading's own reset is provisional and is never compared as a
/// running window's expiry.
#[test]
fn an_unstarted_reading_is_refused_only_over_a_running_window_still_in_effect() {
    let rows: [UnstartedRow; 7] = [
        (
            "over a polled running window",
            Before::PollRunning,
            1,
            None,
            false,
        ),
        (
            "with a provisional reset over a polled running window",
            Before::PollRunning,
            1,
            Some(mins(1) + Duration::from_secs(FIVE_HOURS)),
            false,
        ),
        (
            "over a header running window",
            Before::HeaderRunning,
            1,
            None,
            false,
        ),
        (
            "over a running window whose reset has passed",
            Before::PollRunning,
            181,
            None,
            true,
        ),
        ("over an unstarted window", Before::Unstarted, 1, None, true),
        ("over nothing", Before::Nothing, 1, None, true),
        (
            "with a provisional reset over nothing",
            Before::Nothing,
            1,
            Some(mins(1) + Duration::from_secs(FIVE_HOURS)),
            true,
        ),
    ];
    for (name, before, minute, reset, accepted) in rows {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let five = key(FIVE_HOURS, None);
        let (t0, t1) = (clock.at(mins(0)), clock.at(mins(minute)));
        seed(&store, &before, t0);
        let held_before = accepted_state(store.account(&seat("a")));

        // Act
        let result = store.observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t1,
                vec![unstarted(five.clone(), reset, &t1)],
            ),
        );

        // Assert
        if accepted {
            assert_eq!(result, Ok(()), "row: {name}");
            assert_eq!(used_of(&store, &five), Some(0.0), "row: {name}");
            assert_eq!(
                store
                    .account(&seat("a"))
                    .and_then(|meter| meter.window(&five).map(|w| w.reading().started())),
                Some(Started::No),
                "row: {name}: the held window is the unstarted one"
            );
            assert_eq!(store.rejection_totals().contradictory, 0, "row: {name}");
        } else {
            assert_eq!(
                result,
                Err(ObserveRefusal::Rejected(RejectionReason::Contradictory)),
                "row: {name}"
            );
            assert_eq!(
                accepted_state(store.account(&seat("a"))),
                held_before,
                "row: {name}: no accepted state advances"
            );
            assert_eq!(
                used_of(&store, &five),
                Some(0.95),
                "row: {name}: the running window stands"
            );
            assert_eq!(store.rejection_totals().contradictory, 1, "row: {name}");
        }
    }
}

#[path = "meter_dropped_tests.rs"]
mod meter_dropped_tests;

/// The contradiction rule also governs header-only traffic.
#[test]
fn a_header_snapshot_more_than_the_slack_lower_on_the_same_reset_is_refused() {
    let clock = Clock::new();
    let store = store();
    let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
    assert!(store.observe(&seat("a"), header(t0, 0.50, mins(60))));

    let merged = store.observe(&seat("a"), header(t1, 0.40, mins(60)));

    assert!(!merged);
    assert_eq!(store.rejection_totals().contradictory, 1);
    assert_eq!(
        used_of(&store, &key(FIVE_HOURS, None)),
        Some(0.50),
        "the stored reading stands"
    );
}

// ---- Ordering across sources ----

#[test]
fn an_older_response_arriving_after_a_newer_poll_never_replaces_it() {
    let clock = Clock::new();
    let store = store();
    let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t1,
                vec![running(key(FIVE_HOURS, None), 0.30, mins(60), &t1)],
            ),
        )
        .expect("accepted");

    assert!(store.observe(&seat("a"), header(t0, 0.90, mins(90))));

    let meter = store.account(&seat("a")).expect("an entry");
    let window = meter.window(&key(FIVE_HOURS, None)).expect("held");
    assert_eq!(window.reading().used().fraction(), 0.30);
    assert_eq!(window.source(), ReadingSource::Poll);
    assert_eq!(meter.last_accepted().expect("accepted").observed, t1);
}

// ---- Admission ----

#[test]
fn a_non_admitted_account_is_refused_counted_and_creates_no_entry() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(mins(0));

    let result = store.observe_account(
        &seat("retired"),
        &partial(
            ReadingSource::Poll,
            t0,
            vec![running(key(FIVE_HOURS, None), 0.10, mins(60), &t0)],
        ),
    );

    assert_eq!(result, Err(ObserveRefusal::NotAdmitted));
    assert_eq!(store.refused_by_admission_total(), 1);
    assert!(store.account(&seat("retired")).is_none());
    assert!(store.is_empty());
}

// ---- Header keys ----

/// The header adapter maps a role onto one key, so every curated row in a role
/// must share the length that key carries.
#[test]
fn every_curated_row_in_a_role_has_the_length_its_header_key_carries() {
    for role in [WindowRole::Fast, WindowRole::Slow] {
        let key = role_key(&role).expect("a key for each role");
        for kind in [ANTHROPIC_PROVIDER_KIND, CODEX_PROVIDER_KIND] {
            for row in rows_for(kind).filter(|row| row.role == role) {
                assert_eq!(
                    row.duration.as_secs(),
                    key.length_secs(),
                    "{kind} {}",
                    row.source_id
                );
            }
        }
    }
}
