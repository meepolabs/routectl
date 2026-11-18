//! Tests for the account meter's merge rule, through the store's two entry
//! points, and for the projection placement reads.

use std::time::{Instant, SystemTime};

use super::*;

use crate::quota::curation::{CODEX_PROVIDER_KIND, rows_for};
use crate::quota::freshness::accept_reset;
use crate::quota::key::{SeatKey, seat_key_for_secret_ref};
use crate::quota::reading::Scope;
use crate::quota::store::{ObserveRefusal, QuotaStore};
use crate::quota::window::Utilization;

const FIVE_HOURS: u64 = 5 * 3_600;
const SEVEN_DAYS: u64 = 7 * 86_400;

fn base_wall() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_000_000)
}

/// Stamps `offset` past one base on both clocks, ordered alike.
struct Clock {
    monotonic: Instant,
}

impl Clock {
    fn new() -> Self {
        Self {
            monotonic: Instant::now(),
        }
    }

    fn at(&self, offset: Duration) -> ObservationStamp {
        ObservationStamp::from_parts(base_wall() + offset, self.monotonic + offset)
    }
}

fn mins(n: u64) -> Duration {
    Duration::from_mins(n)
}

fn seat(label: &str) -> SeatKey {
    let secret_ref = routectl_auth::SecretRef::OAuth {
        provider: "anthropic".to_string(),
        label: Some(label.to_string()),
    };
    seat_key_for_secret_ref(Some(&secret_ref)).expect("an oauth ref yields a key")
}

fn store() -> QuotaStore {
    let store = QuotaStore::default();
    store.admit_seats([seat("a")]);
    store
}

fn key(length_secs: u64, scope: Option<&str>) -> WindowKey {
    let scope = scope.map(|raw| Scope::new(raw).expect("a canonical scope"));
    WindowKey::new(length_secs, scope).expect("a classifiable length")
}

/// A running window at `used`, resetting `reset` past the base wall clock, so
/// two observations name the same reset by passing the same offset.
fn running(key: WindowKey, used: f64, reset: Duration, at: &ObservationStamp) -> WindowReading {
    WindowReading::new(key, used, Some(base_wall() + reset), Started::Yes, at)
        .expect("a valid window")
}

fn observation(
    source: ReadingSource,
    at: ObservationStamp,
    report: ObservationReport,
) -> AccountObservation {
    AccountObservation::new(source, at, report).expect("one stamp throughout")
}

fn partial(
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

fn used_of(store: &QuotaStore, key: &WindowKey) -> Option<f64> {
    store
        .account(&seat("a"))?
        .window(key)
        .map(|w| w.reading().used().fraction())
}

/// A header snapshot carrying a FAST reading at `used`, resetting `reset`
/// past the base wall clock.
fn header(at: ObservationStamp, used: f64, reset: Duration) -> QuotaSnapshot {
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

/// A complete report that omits the five-hour window drops a running one,
/// and the projection then has no FAST evidence: unknown, never room.
#[test]
fn a_complete_report_omitting_the_five_hour_window_projects_fast_unknown() {
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
        assert_eq!(
            store.reading_for(&seat("a"), &t1).expect("an entry").fast,
            QuotaWindow::Unknown,
            "row: {name}"
        );
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
        let fast = store.reading_for(&seat("a"), &s).expect("an entry").fast;
        assert_eq!(
            matches!(fast, QuotaWindow::Known { .. }),
            five_used.is_some(),
            "row: {name}: the projection follows the stored window"
        );
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
        let before = store.account(&seat("a")).expect("an entry");

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
                assert_eq!(after, before, "row: {name}: stored state identical");
                assert_eq!(
                    store.rejection_totals().contradictory,
                    1,
                    "row: {name}: counted once"
                );
            }
        }
    }
}

#[test]
fn a_drop_with_a_different_reset_is_a_new_window_not_a_contradiction() {
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

    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t1,
                vec![running(five.clone(), 0.05, mins(60) + mins(6), &t1)],
            ),
        )
        .expect("a reset outside the tolerance is a different window");

    assert_eq!(used_of(&store, &five), Some(0.05));
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

fn unstarted(key: WindowKey, reset: Option<Duration>, at: &ObservationStamp) -> WindowReading {
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
        let held_before = store.account(&seat("a"));

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
        let fast = store.reading_for(&seat("a"), &t1).expect("an entry").fast;
        if accepted {
            assert_eq!(result, Ok(()), "row: {name}");
            assert_eq!(used_of(&store, &five), Some(0.0), "row: {name}");
            let QuotaWindow::Known {
                utilization,
                reset_at,
            } = fast
            else {
                panic!("row: {name}: a fresh unstarted FAST window is known: {fast:?}");
            };
            assert_eq!(utilization.fraction(), 0.0, "row: {name}");
            assert_eq!(
                reset_at.at(),
                base_wall() + mins(minute) + crate::quota::view::STALE_AFTER,
                "row: {name}: a provisional reset is not a running window's expiry"
            );
            assert_eq!(store.rejection_totals().contradictory, 0, "row: {name}");
        } else {
            assert_eq!(
                result,
                Err(ObserveRefusal::Rejected(RejectionReason::Contradictory)),
                "row: {name}"
            );
            assert_eq!(
                store.account(&seat("a")),
                held_before,
                "row: {name}: nothing advances"
            );
            let QuotaWindow::Known { utilization, .. } = fast else {
                panic!("row: {name}: the running window stands: {fast:?}");
            };
            assert_eq!(utilization.fraction(), 0.95, "row: {name}");
            assert_eq!(store.rejection_totals().contradictory, 1, "row: {name}");
        }
    }
}

/// What follows a complete report that dropped a running five-hour window.
enum AfterDrop {
    Unstarted,
    Running,
}

/// One drop case: name, whether the dropping report carries a weekly window,
/// what arrives next, the minute it arrives, and whether it is taken.
type DropRow = (&'static str, bool, AfterDrop, u64, bool);

/// A complete report that omits a running window still in effect does not
/// clear the way for an unstarted reading: the window's reset is remembered,
/// and an unstarted reading before it is a contradiction. After that reset,
/// or once a running reading for the key is taken, it is not.
#[test]
fn an_unstarted_reading_after_a_complete_report_dropped_a_running_window() {
    let rows: [DropRow; 5] = [
        (
            "no windows, then idle",
            false,
            AfterDrop::Unstarted,
            2,
            false,
        ),
        (
            "weekly only, then idle",
            true,
            AfterDrop::Unstarted,
            2,
            false,
        ),
        (
            "no windows, then idle after the reset",
            false,
            AfterDrop::Unstarted,
            181,
            true,
        ),
        (
            "weekly only, then idle after the reset",
            true,
            AfterDrop::Unstarted,
            181,
            true,
        ),
        (
            "no windows, then running",
            false,
            AfterDrop::Running,
            2,
            true,
        ),
    ];
    for (name, reports_weekly, next, minute, accepted) in rows {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let five = key(FIVE_HOURS, None);
        let (t0, t1, t2) = (clock.at(mins(0)), clock.at(mins(1)), clock.at(mins(minute)));
        seed(&store, &Before::PollRunning, t0);
        let dropping = if reports_weekly {
            ObservationReport::Complete(
                WindowSet::new(vec![running(key(SEVEN_DAYS, None), 0.20, mins(3_000), &t1)])
                    .expect("a set"),
            )
        } else {
            ObservationReport::NoWindows
        };
        store
            .observe_account(&seat("a"), &observation(ReadingSource::Poll, t1, dropping))
            .expect("the dropping report is accepted");
        let held_before = store.account(&seat("a"));
        let reading = match next {
            AfterDrop::Unstarted => unstarted(five.clone(), None, &t2),
            AfterDrop::Running => running(five.clone(), 0.97, mins(180), &t2),
        };

        // Act
        let result = store.observe_account(
            &seat("a"),
            &observation(
                ReadingSource::Poll,
                t2,
                ObservationReport::Complete(WindowSet::new(vec![reading]).expect("a set")),
            ),
        );

        // Assert
        let fast = store.reading_for(&seat("a"), &t2).expect("an entry").fast;
        let meter = store.account(&seat("a")).expect("an entry");
        if !accepted {
            assert_eq!(
                result,
                Err(ObserveRefusal::Rejected(RejectionReason::Contradictory)),
                "row: {name}"
            );
            assert_eq!(Some(meter), held_before, "row: {name}: nothing advances");
            assert_eq!(fast, QuotaWindow::Unknown, "row: {name}: never room");
            assert_eq!(store.rejection_totals().contradictory, 1, "row: {name}");
            continue;
        }
        assert_eq!(result, Ok(()), "row: {name}");
        assert!(
            !meter.running_until.contains_key(&five),
            "row: {name}: the entry clears"
        );
        assert_eq!(store.rejection_totals().contradictory, 0, "row: {name}");
        let QuotaWindow::Known { utilization, .. } = fast else {
            panic!("row: {name}: the taken reading is known: {fast:?}");
        };
        let expected = match next {
            AfterDrop::Unstarted => 0.0,
            AfterDrop::Running => 0.97,
        };
        assert_eq!(utilization.fraction(), expected, "row: {name}");
    }
}

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
    let fast = store.reading_for(&seat("a"), &t1).expect("an entry").fast;
    let QuotaWindow::Known { utilization, .. } = fast else {
        panic!("the stored FAST reading stands");
    };
    assert_eq!(utilization.fraction(), 0.50);
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

// ---- Projection ----

/// FAST comes only from the account-wide window of the curated five-hour
/// length: a scoped five-hour window or another short length never feeds it.
#[test]
fn the_projection_takes_fast_only_from_the_account_wide_five_hour_window() {
    let clock = Clock::new();
    let rows: [(&str, WindowKey, bool); 3] = [
        ("account-wide 5 h", key(FIVE_HOURS, None), true),
        ("scoped 5 h", key(FIVE_HOURS, Some("opus")), false),
        ("account-wide 4 h", key(4 * 3_600, None), false),
    ];
    for (name, window_key, feeds_fast) in rows {
        let store = store();
        let t0 = clock.at(mins(0));
        store
            .observe_account(
                &seat("a"),
                &partial(
                    ReadingSource::Poll,
                    t0,
                    vec![running(window_key, 0.10, mins(60), &t0)],
                ),
            )
            .expect("accepted");

        let fast = store.reading_for(&seat("a"), &t0).expect("an entry").fast;

        assert_eq!(
            matches!(fast, QuotaWindow::Known { .. }),
            feeds_fast,
            "row: {name}"
        );
    }
}

#[test]
fn a_poll_window_feeds_the_projection_until_its_reset() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(mins(0));
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t0,
                vec![running(key(FIVE_HOURS, None), 0.70, mins(60), &t0)],
            ),
        )
        .expect("accepted");

    let before = store
        .reading_for(&seat("a"), &clock.at(mins(59)))
        .expect("an entry");
    let after = store
        .reading_for(&seat("a"), &clock.at(mins(60)))
        .expect("an entry");

    assert!(matches!(before.fast, QuotaWindow::Known { .. }));
    assert_eq!(after.fast, QuotaWindow::Unknown);
}

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
