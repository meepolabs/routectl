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

/// An unstarted window's reset is provisional (where it would end if it began
/// now); it is never compared as a running window's expiry.
#[test]
fn an_unstarted_reading_never_counts_as_a_contradiction() {
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
                vec![running(five.clone(), 0.40, mins(60), &t0)],
            ),
        )
        .expect("accepted");
    let unstarted = WindowReading::new(
        five.clone(),
        0.0,
        Some(base_wall() + mins(61)),
        Started::No,
        &t1,
    )
    .expect("an unstarted window with a provisional reset");

    store
        .observe_account(
            &seat("a"),
            &partial(ReadingSource::Poll, t1, vec![unstarted]),
        )
        .expect("accepted");

    assert_eq!(used_of(&store, &five), Some(0.0));
    assert_eq!(
        store.reading_for(&seat("a"), &t1).expect("an entry").fast,
        QuotaWindow::Unknown,
        "a provisional reset is not a running window's expiry"
    );
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
