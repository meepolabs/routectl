//! The contradiction rule corrects itself: a lower reading over a window in
//! effect is refused until enough consecutive readings agree on it.

use super::meter_tests::{
    Clock, FIVE_HOURS, SEVEN_DAYS, base_wall, header, key, mins, observation, partial, running,
    seat, store, unstarted,
};
use super::*;

use crate::quota::freshness::accept_reset;
use crate::quota::store::{ObserveRefusal, QuotaStore};
use crate::quota::window::Utilization;

const REFUSED: Result<(), ObserveRefusal> =
    Err(ObserveRefusal::Rejected(RejectionReason::Contradictory));
const TAKEN: Result<(), ObserveRefusal> = Ok(());

/// One five-hour reading fed after the seed.
#[derive(Clone, Copy)]
enum Next {
    /// A running poll window at this used fraction, resetting this many
    /// minutes past the base.
    Poll(f64, u64),
    /// A response header at this used fraction and reset minute.
    Header(f64, u64),
    /// An unstarted poll window.
    Idle,
}

/// Store a running five-hour poll window at 0.90 resetting at `reset_min`,
/// observed at minute 0.
fn seeded(clock: &Clock, reset_min: u64) -> QuotaStore {
    seeded_on(clock, None, reset_min)
}

/// [`seeded`], on the five-hour window of `scope`.
fn seeded_on(clock: &Clock, scope: Option<&str>, reset_min: u64) -> QuotaStore {
    let store = store();
    let t0 = clock.at(mins(0));
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t0,
                vec![running(key(FIVE_HOURS, scope), 0.90, mins(reset_min), &t0)],
            ),
        )
        .expect("the seed is accepted");
    store
}

fn feed(store: &QuotaStore, at: ObservationStamp, next: Next) -> Result<(), ObserveRefusal> {
    feed_on(store, at, next, None)
}

/// [`feed`], with poll readings on the five-hour window of `scope`.
fn feed_on(
    store: &QuotaStore,
    at: ObservationStamp,
    next: Next,
    scope: Option<&str>,
) -> Result<(), ObserveRefusal> {
    let five = key(FIVE_HOURS, scope);
    let reading = match next {
        Next::Header(used, reset_min) => {
            return if store.observe(&seat("a"), header(at, used, mins(reset_min))) {
                TAKEN
            } else {
                REFUSED
            };
        }
        Next::Poll(used, reset_min) => running(five, used, mins(reset_min), &at),
        Next::Idle => unstarted(five, None, &at),
    };
    store.observe_account(&seat("a"), &partial(ReadingSource::Poll, at, vec![reading]))
}

/// Feed `steps` (minute, reading) in order and collect each outcome.
fn run(
    store: &QuotaStore,
    clock: &Clock,
    steps: &[(u64, Next)],
) -> Vec<Result<(), ObserveRefusal>> {
    steps
        .iter()
        .map(|(minute, next)| feed(store, clock.at(mins(*minute)), *next))
        .collect()
}

/// Each step one minute after the last, starting at minute 1.
fn consecutive(readings: &[Next]) -> Vec<(u64, Next)> {
    (1..).zip(readings.iter().copied()).collect()
}

fn stored_five(store: &QuotaStore) -> WindowReading {
    stored_five_on(store, None)
}

/// [`stored_five`], for the five-hour window of `scope`.
fn stored_five_on(store: &QuotaStore, scope: Option<&str>) -> WindowReading {
    store
        .account(&seat("a"))
        .expect("an entry")
        .window(&key(FIVE_HOURS, scope))
        .expect("a five-hour window")
        .reading()
        .clone()
}

/// The window the lower readings contradict.
#[derive(Clone, Copy)]
enum Binding {
    /// Stored at minute 0.
    Stored,
    /// Stored at minute 0, then dropped by a complete report at minute 1 and
    /// kept while in effect.
    KeptAfterDrop,
}

/// One confirmation case: the window the readings contradict and its scope,
/// the three lower readings (fed at minutes 2, 3, 4), and the used fraction,
/// reset minute and started state the third one leaves stored.
struct ConfirmRow {
    name: &'static str,
    binding: Binding,
    scope: Option<&'static str>,
    readings: [Next; 3],
    used: f64,
    reset_min: Option<u64>,
    started: Started,
}

/// The seeded store with `binding` arranged over its five-hour window.
fn bound(clock: &Clock, binding: Binding, scope: Option<&str>) -> QuotaStore {
    let store = seeded_on(clock, scope, 180);
    if matches!(binding, Binding::KeptAfterDrop) {
        let t1 = clock.at(mins(1));
        store
            .observe_account(
                &seat("a"),
                &observation(ReadingSource::Poll, t1, ObservationReport::NoWindows),
            )
            .expect("the dropping report is accepted");
        assert!(
            store
                .account(&seat("a"))
                .expect("an entry")
                .kept_windows
                .dropped_fast
                .is_some(),
            "the control: the dropped window is kept"
        );
    }
    store
}

fn confirm_rows() -> [ConfirmRow; 10] {
    let running = |used, reset_min| ConfirmRow {
        name: "",
        binding: Binding::Stored,
        scope: None,
        readings: [Next::Poll(0.30, reset_min); 3],
        used,
        reset_min: Some(reset_min),
        started: Started::Yes,
    };
    [
        ConfirmRow {
            name: "running lower on the same reset",
            readings: [
                Next::Poll(0.30, 180),
                Next::Poll(0.30, 181),
                Next::Poll(0.31, 182),
            ],
            reset_min: Some(182),
            ..running(0.31, 180)
        },
        ConfirmRow {
            name: "unstarted over a running window",
            readings: [Next::Idle; 3],
            reset_min: None,
            started: Started::No,
            ..running(0.0, 180)
        },
        ConfirmRow {
            name: "running lower, resetting earlier than the tolerance",
            ..running(0.30, 120)
        },
        ConfirmRow {
            name: "a scoped window, resetting earlier than the tolerance",
            scope: Some("opus"),
            ..running(0.30, 120)
        },
        ConfirmRow {
            name: "the kept dropped five-hour window",
            binding: Binding::KeptAfterDrop,
            ..running(0.30, 180)
        },
        ConfirmRow {
            name: "unstarted over the kept dropped five-hour window",
            binding: Binding::KeptAfterDrop,
            readings: [Next::Idle; 3],
            reset_min: None,
            started: Started::No,
            ..running(0.0, 180)
        },
        ConfirmRow {
            name: "higher used on an earlier reset, from polls",
            readings: [Next::Poll(0.95, 120); 3],
            ..running(0.95, 120)
        },
        ConfirmRow {
            name: "higher used on an earlier reset, from headers",
            readings: [Next::Header(0.95, 120); 3],
            started: Started::Unknown,
            ..running(0.95, 120)
        },
        ConfirmRow {
            name: "higher used on an earlier reset over the kept dropped five-hour window",
            binding: Binding::KeptAfterDrop,
            readings: [Next::Poll(0.95, 120); 3],
            ..running(0.95, 120)
        },
        ConfirmRow {
            name: "each agreeing with the last, drifting past the slack overall",
            readings: [
                Next::Poll(0.30, 180),
                Next::Poll(0.32, 180),
                Next::Poll(0.34, 180),
            ],
            ..running(0.34, 180)
        },
    ]
}

/// The reading taken on confirmation is the third one as reported: its own
/// used fraction, reset and started state, never a blend with the stored one.
#[test]
fn a_lower_reading_is_refused_twice_and_taken_on_the_third_agreeing_reading() {
    for row in confirm_rows() {
        let ConfirmRow {
            name,
            binding,
            scope,
            readings,
            used,
            reset_min,
            started,
        } = row;
        // Arrange
        let clock = Clock::new();
        let store = bound(&clock, binding, scope);

        // Act
        let outcomes: Vec<_> = (2..)
            .zip(readings)
            .map(|(minute, next)| feed_on(&store, clock.at(mins(minute)), next, scope))
            .collect();

        // Assert
        assert_eq!(outcomes, [REFUSED, REFUSED, TAKEN], "row: {name}");
        assert_eq!(store.rejection_totals().contradictory, 2, "row: {name}");
        let stored = stored_five_on(&store, scope);
        assert_eq!(stored.used().fraction(), used, "row: {name}");
        assert_eq!(
            stored.reset().map(|r| r.at()),
            reset_min.map(|m| base_wall() + mins(m)),
            "row: {name}"
        );
        assert_eq!(stored.started(), started, "row: {name}");
    }
}

/// Where a reading for the early-reset rows comes from.
#[derive(Clone, Copy)]
enum Source {
    Header,
    Poll,
}

/// A running reading for the `length_secs` window at `used`, resetting
/// `reset_min` minutes past the base, fed from `source`.
fn feed_window(
    store: &QuotaStore,
    at: ObservationStamp,
    source: Source,
    length_secs: u64,
    used: f64,
    reset_min: u64,
) -> Result<(), ObserveRefusal> {
    match source {
        Source::Header => {
            let known = QuotaWindow::Known {
                utilization: Utilization::new(used).expect("a valid fraction"),
                reset_at: accept_reset(
                    base_wall() + mins(reset_min),
                    &at,
                    Duration::from_secs(length_secs),
                    RESET_TOLERANCE,
                )
                .expect("a plausible reset"),
            };
            let (fast, slow) = if length_secs == FIVE_HOURS {
                (known, QuotaWindow::Unknown)
            } else {
                (QuotaWindow::Unknown, known)
            };
            let snapshot = QuotaSnapshot {
                observed: at,
                fast,
                slow,
                billing: Billing::Unknown,
            };
            if store.observe(&seat("a"), snapshot) {
                TAKEN
            } else {
                REFUSED
            }
        }
        Source::Poll => store.observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                at,
                vec![running(key(length_secs, None), used, mins(reset_min), &at)],
            ),
        ),
    }
}

/// A running window cannot end early: a reading that moves its reset
/// earlier, even at higher used, is refused, so once that near reset passes
/// a low reading on the real reset still meets the window it contradicts.
#[test]
fn a_reading_resetting_earlier_than_a_running_window_is_lower() {
    let rows: [(&str, Source, u64, u64); 4] = [
        ("five-hour, from headers", Source::Header, FIVE_HOURS, 180),
        ("five-hour, from polls", Source::Poll, FIVE_HOURS, 180),
        ("weekly, from headers", Source::Header, SEVEN_DAYS, 4_320),
        ("weekly, from polls", Source::Poll, SEVEN_DAYS, 4_320),
    ];
    for (name, source, length_secs, reset_min) in rows {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let t0 = clock.at(mins(0));
        store
            .observe_account(
                &seat("a"),
                &partial(
                    ReadingSource::Poll,
                    t0,
                    vec![running(key(length_secs, None), 0.90, mins(reset_min), &t0)],
                ),
            )
            .expect("the seed is accepted");

        // Act
        let near = feed_window(&store, clock.at(mins(1)), source, length_secs, 0.96, 2);
        let low = feed_window(
            &store,
            clock.at(mins(3)),
            source,
            length_secs,
            0.05,
            reset_min,
        );

        // Assert
        assert_eq!([near, low], [REFUSED, REFUSED], "row: {name}");
        assert_eq!(store.rejection_totals().contradictory, 2, "row: {name}");
        let stored = store
            .account(&seat("a"))
            .expect("an entry")
            .window(&key(length_secs, None))
            .expect("the window is held")
            .reading()
            .clone();
        assert_eq!(stored.used().fraction(), 0.90, "row: {name}");
        assert_eq!(
            stored.reset().map(|r| r.at()),
            Some(base_wall() + mins(reset_min)),
            "row: {name}"
        );
    }
}

/// A reading on a reset no earlier than the stored one, beyond the tolerance
/// or later, with equal or higher used is taken as reported at once.
#[test]
fn a_reading_on_a_reset_no_earlier_with_no_lower_used_is_taken_at_once() {
    let rows: [(&str, Next); 4] = [
        (
            "a later reset, higher used, from polls",
            Next::Poll(0.95, 240),
        ),
        (
            "a later reset, higher used, from headers",
            Next::Header(0.95, 240),
        ),
        ("a later reset, equal used", Next::Poll(0.90, 240)),
        (
            "an earlier reset within the tolerance",
            Next::Poll(0.95, 177),
        ),
    ];
    for (name, next) in rows {
        // Arrange
        let clock = Clock::new();
        let store = seeded(&clock, 180);
        let (Next::Poll(used, reset_min) | Next::Header(used, reset_min)) = next else {
            unreachable!("every row is running")
        };

        // Act
        let outcome = feed(&store, clock.at(mins(1)), next);

        // Assert
        assert_eq!(outcome, TAKEN, "row: {name}");
        let stored = stored_five(&store);
        assert_eq!(stored.used().fraction(), used, "row: {name}");
        assert_eq!(
            stored.reset().map(|r| r.at()),
            Some(base_wall() + mins(reset_min)),
            "row: {name}"
        );
        assert_eq!(store.rejection_totals().contradictory, 0, "row: {name}");
    }
}

#[test]
fn agreeing_header_and_poll_readings_advance_one_streak() {
    // Arrange
    let clock = Clock::new();
    let store = seeded(&clock, 180);
    let readings = [
        Next::Header(0.30, 180),
        Next::Poll(0.30, 181),
        Next::Header(0.31, 181),
    ];

    // Act
    let outcomes = run(&store, &clock, &consecutive(&readings));

    // Assert
    assert_eq!(outcomes, [REFUSED, REFUSED, TAKEN]);
    assert_eq!(stored_five(&store).used().fraction(), 0.31);
}

/// Persistent disagreement between sources keeps the higher stored value.
#[test]
fn alternating_disagreeing_sources_are_never_taken() {
    // Arrange
    let clock = Clock::new();
    let store = seeded(&clock, 180);
    let readings = [Next::Header(0.60, 180), Next::Poll(0.30, 180)].repeat(3);

    // Act
    let outcomes = run(&store, &clock, &consecutive(&readings));

    // Assert
    assert_eq!(outcomes, [REFUSED; 6]);
    assert_eq!(store.rejection_totals().contradictory, 6);
    assert_eq!(stored_five(&store).used().fraction(), 0.90);
}

#[test]
fn a_disagreeing_lower_reading_restarts_the_streak_at_one() {
    let rows: [(&str, Next, Next); 4] = [
        (
            "used further apart than the slack",
            Next::Poll(0.30, 180),
            Next::Poll(0.60, 180),
        ),
        (
            "resets further apart than the tolerance",
            Next::Poll(0.30, 180),
            Next::Poll(0.30, 120),
        ),
        ("unstarted after running", Next::Poll(0.30, 180), Next::Idle),
        ("running after unstarted", Next::Idle, Next::Poll(0.30, 180)),
    ];
    for (name, first, then) in rows {
        // Arrange
        let clock = Clock::new();
        let store = seeded(&clock, 180);
        let readings = [first, first, then, then, then];

        // Act
        let outcomes = run(&store, &clock, &consecutive(&readings));

        // Assert
        assert_eq!(
            outcomes,
            [REFUSED, REFUSED, REFUSED, REFUSED, TAKEN],
            "row: {name}"
        );
    }
}

#[test]
fn a_duplicate_or_older_stamp_does_not_advance_the_streak() {
    let lower = Next::Poll(0.30, 180);
    let rows: [(&str, [u64; 4]); 3] = [
        ("a duplicate stamp", [1, 1, 2, 3]),
        ("an older stamp", [2, 1, 3, 4]),
        ("the stamp of the window it contradicts", [0, 1, 2, 3]),
    ];
    for (name, minutes) in rows {
        // Arrange
        let clock = Clock::new();
        let store = seeded(&clock, 180);
        let steps: Vec<(u64, Next)> = minutes.iter().map(|m| (*m, lower)).collect();

        // Act
        let outcomes = run(&store, &clock, &steps);

        // Assert
        assert_eq!(outcomes, [REFUSED, REFUSED, REFUSED, TAKEN], "row: {name}");
    }
}

#[test]
fn a_lower_reading_over_a_window_no_longer_in_effect_is_taken_at_once() {
    // Arrange
    let clock = Clock::new();
    let store = seeded(&clock, 60);

    // Act
    let outcome = feed(&store, clock.at(mins(61)), Next::Poll(0.10, 360));

    // Assert
    assert_eq!(outcome, TAKEN);
    assert_eq!(stored_five(&store).used().fraction(), 0.10);
    assert_eq!(store.rejection_totals().contradictory, 0);
}

/// A multi-window observation is applied whole or not at all: a window that
/// has confirmed waits for every other lower window in it to confirm.
#[test]
fn a_multi_window_observation_applies_nothing_until_every_lower_window_confirms() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let five = key(FIVE_HOURS, None);
    let seven = key(SEVEN_DAYS, None);
    let t0 = clock.at(mins(0));
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t0,
                vec![
                    running(five.clone(), 0.90, mins(180), &t0),
                    running(seven.clone(), 0.40, mins(3_000), &t0),
                ],
            ),
        )
        .expect("the seed is accepted");
    let five_only = |at: ObservationStamp| {
        partial(
            ReadingSource::Poll,
            at,
            vec![running(five.clone(), 0.30, mins(180), &at)],
        )
    };
    let both = |at: ObservationStamp| {
        partial(
            ReadingSource::Poll,
            at,
            vec![
                running(five.clone(), 0.30, mins(180), &at),
                running(seven.clone(), 0.10, mins(3_000), &at),
            ],
        )
    };

    // Act
    let mut outcomes = Vec::new();
    for minute in 1..=2 {
        outcomes.push(store.observe_account(&seat("a"), &five_only(clock.at(mins(minute)))));
    }
    let used_while_unconfirmed = |store: &QuotaStore| {
        let meter = store.account(&seat("a")).expect("an entry");
        [&five, &seven].map(|k| meter.window(k).expect("held").reading().used().fraction())
    };
    let mut held = Vec::new();
    for minute in 3..=5 {
        outcomes.push(store.observe_account(&seat("a"), &both(clock.at(mins(minute)))));
        held.push(used_while_unconfirmed(&store));
    }

    // Assert
    assert_eq!(outcomes, [REFUSED, REFUSED, REFUSED, REFUSED, TAKEN]);
    assert_eq!(
        held,
        [[0.90, 0.40], [0.90, 0.40], [0.30, 0.10]],
        "the confirmed five-hour window waits for the weekly one"
    );
}

/// What happens between the second refused lower reading and the next one.
#[derive(Clone, Copy)]
enum Clearing {
    Higher,
    Lifted,
    CompleteReportDropsIt,
    Forget,
    /// A replacement for a stored window that has reached its reset.
    ReplacedAfterLapse,
}

/// Each event that clears a streak sends the next agreeing lower reading back
/// to the start: refused twice more before it is taken. Each row names the
/// stored window's reset minute and the lower reading that primes the streak
/// and follows the clearing event; an unstarted reading agrees with any
/// unstarted streak, so it would confirm on an uncleared one.
#[test]
fn each_clearing_event_restarts_the_streak() {
    let lower = Next::Poll(0.30, 180);
    let rows = [
        ("an accepted higher reading", Clearing::Higher, 180, lower),
        (
            "an accepted reading lifted within the slack",
            Clearing::Lifted,
            180,
            lower,
        ),
        (
            "a complete report dropping the window",
            Clearing::CompleteReportDropsIt,
            180,
            lower,
        ),
        ("forgetting the account", Clearing::Forget, 180, lower),
        (
            "a replacement accepted after the stored window lapsed",
            Clearing::ReplacedAfterLapse,
            30,
            Next::Idle,
        ),
    ];
    for (name, clearing, reset_min, lower) in rows {
        // Arrange
        let clock = Clock::new();
        let store = seeded(&clock, reset_min);
        let primed = run(&store, &clock, &[(1, lower), (2, lower)]);
        assert_eq!(primed, [REFUSED, REFUSED], "row: {name}: primed");
        let cleared_at = clock.at(mins(31));
        match clearing {
            Clearing::Higher => assert_eq!(feed(&store, cleared_at, Next::Poll(0.95, 180)), TAKEN),
            Clearing::Lifted => assert_eq!(feed(&store, cleared_at, Next::Poll(0.89, 180)), TAKEN),
            Clearing::ReplacedAfterLapse => {
                assert_eq!(feed(&store, cleared_at, Next::Poll(0.80, 330)), TAKEN);
            }
            Clearing::CompleteReportDropsIt => store
                .observe_account(
                    &seat("a"),
                    &observation(
                        ReadingSource::Poll,
                        cleared_at,
                        ObservationReport::Complete(
                            WindowSet::new(vec![running(
                                key(SEVEN_DAYS, None),
                                0.20,
                                mins(3_000),
                                &cleared_at,
                            )])
                            .expect("a set"),
                        ),
                    ),
                )
                .expect("the dropping report is accepted"),
            Clearing::Forget => {
                store.forget(&seat("a"));
                store
                    .observe_account(
                        &seat("a"),
                        &partial(
                            ReadingSource::Poll,
                            cleared_at,
                            vec![running(key(FIVE_HOURS, None), 0.90, mins(180), &cleared_at)],
                        ),
                    )
                    .expect("the re-seed is accepted");
            }
        }

        // Act
        let outcomes = run(&store, &clock, &[(32, lower), (33, lower), (34, lower)]);

        // Assert
        assert_eq!(outcomes, [REFUSED, REFUSED, TAKEN], "row: {name}");
    }
}

/// Once the stored window's reset passes, any accepted observation drops the
/// streak for it, so a streak never outlives the window it would correct.
#[test]
fn a_streak_clears_once_its_stored_window_leaves_effect() {
    // Arrange
    let clock = Clock::new();
    let store = seeded(&clock, 30);
    let primed = run(
        &store,
        &clock,
        &[(1, Next::Poll(0.30, 30)), (2, Next::Poll(0.30, 30))],
    );
    assert_eq!(primed, [REFUSED, REFUSED]);
    let streaks_before = store.account(&seat("a")).expect("an entry").streaks.len();
    let t31 = clock.at(mins(31));

    // Act
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t31,
                vec![running(key(SEVEN_DAYS, None), 0.20, mins(3_000), &t31)],
            ),
        )
        .expect("accepted");

    // Assert
    assert_eq!(streaks_before, 1, "the control: a streak was held");
    assert!(
        store
            .account(&seat("a"))
            .expect("an entry")
            .streaks
            .is_empty()
    );
}

/// A seeded store, the kept windows' omission counts it holds, and the
/// refused feed that follows it.
type RefusalRow = (
    &'static str,
    &'static [u32],
    fn(&Clock) -> QuotaStore,
    fn(&QuotaStore, &Clock) -> Result<(), ObserveRefusal>,
);

/// The seeded five-hour window plus an overage header at minute 1.
fn seeded_with_overage(clock: &Clock) -> QuotaStore {
    let store = seeded(clock, 180);
    let overage = QuotaSnapshot {
        billing: Billing::Overage,
        ..header(clock.at(mins(1)), 0.91, mins(180))
    };
    assert!(store.observe(&seat("a"), overage));
    store
}

/// A five-hour window at 0.90 beside a running weekly, then a complete
/// report at minute 1 that omits the weekly, so it is kept with one
/// omission counted.
fn seeded_with_omitted_weekly(clock: &Clock) -> QuotaStore {
    let store = store();
    let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
    let five = |at: &ObservationStamp| running(key(FIVE_HOURS, None), 0.90, mins(180), at);
    let complete = |at, windows| {
        observation(
            ReadingSource::Poll,
            at,
            ObservationReport::Complete(WindowSet::new(windows).expect("a set")),
        )
    };
    store
        .observe_account(
            &seat("a"),
            &complete(
                t0,
                vec![
                    five(&t0),
                    running(key(SEVEN_DAYS, None), 0.95, mins(4_320), &t0),
                ],
            ),
        )
        .expect("the seed is accepted");
    store
        .observe_account(&seat("a"), &complete(t1, vec![five(&t1)]))
        .expect("the omitting report is accepted");
    store
}

fn refusal_rows() -> [RefusalRow; 2] {
    [
        (
            "a partial poll after an overage header",
            &[],
            seeded_with_overage,
            |store, clock| feed(store, clock.at(mins(2)), Next::Poll(0.30, 180)),
        ),
        (
            "a complete poll beside a kept weekly it omits again",
            &[1],
            seeded_with_omitted_weekly,
            |store, clock| {
                let t2 = clock.at(mins(2));
                store.observe_account(
                    &seat("a"),
                    &observation(
                        ReadingSource::Poll,
                        t2,
                        ObservationReport::Complete(
                            WindowSet::new(vec![running(
                                key(FIVE_HOURS, None),
                                0.30,
                                mins(180),
                                &t2,
                            )])
                            .expect("a set"),
                        ),
                    ),
                )
            },
        ),
    ]
}

/// A refusal changes only the refused key's streak: every accepted field,
/// billing, ordering marks and kept windows' omission counts included, stays
/// exactly as it was.
#[test]
fn a_refusal_leaves_everything_but_the_refused_keys_streak_unchanged() {
    for (name, kept_omissions, seed, refused_feed) in refusal_rows() {
        // Arrange
        let clock = Clock::new();
        let store = seed(&clock);
        let before = store.account(&seat("a")).expect("an entry");
        let omissions = |meter: &AccountMeter| -> Vec<u32> {
            meter
                .kept_windows
                .omitted
                .values()
                .map(|kept| kept.omissions)
                .collect()
        };

        // Act
        let outcome = refused_feed(&store, &clock);

        // Assert
        assert_eq!(outcome, REFUSED, "row: {name}");
        let after = store.account(&seat("a")).expect("an entry");
        assert_eq!(
            after.streaks.keys().collect::<Vec<_>>(),
            [&key(FIVE_HOURS, None)],
            "row: {name}"
        );
        assert_eq!(omissions(&after), omissions(&before), "row: {name}");
        assert_eq!(
            omissions(&before),
            kept_omissions,
            "row: {name}: the control"
        );
        let mut without_streaks = after.clone();
        without_streaks.streaks.clear();
        assert_eq!(without_streaks, before, "row: {name}");
    }
}

#[test]
fn a_slack_sized_same_reset_drop_is_lifted_and_starts_no_streak() {
    // Arrange
    let clock = Clock::new();
    let store = seeded(&clock, 180);

    // Act
    let outcome = feed(&store, clock.at(mins(1)), Next::Poll(0.88, 181));

    // Assert
    assert_eq!(outcome, TAKEN);
    assert_eq!(stored_five(&store).used().fraction(), 0.90);
    assert!(
        store
            .account(&seat("a"))
            .expect("an entry")
            .streaks
            .is_empty()
    );
}

#[test]
fn a_confirmed_correction_logs_once_and_counts_once() {
    // Arrange
    let clock = Clock::new();
    let store = seeded(&clock, 180);
    let lower = Next::Poll(0.30, 180);
    let primed = run(&store, &clock, &[(1, lower), (2, lower)]);
    assert_eq!(primed, [REFUSED, REFUSED]);
    let counted_before = store.readings(&clock.at(mins(2))).corrections_confirmed;

    // Act
    let mut outcome = None;
    let events = routectl_testkit::capture_events(|| {
        outcome = Some(feed(&store, clock.at(mins(3)), lower));
    });

    // Assert
    assert_eq!(outcome, Some(TAKEN));
    let lines: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("quota_correction_confirmed"))
        .collect();
    assert_eq!(lines.len(), 1);
    let line = lines[0];
    assert_eq!(line.field("seat"), Some("anthropic#a"));
    assert_eq!(line.field("window_secs"), Some("18000"));
    assert_eq!(line.field("scope"), Some(""));
    assert_eq!(line.field("old_used"), Some("0.9"));
    assert_eq!(line.field("new_used"), Some("0.3"));
    assert_eq!(counted_before, 0);
    assert_eq!(store.readings(&clock.at(mins(3))).corrections_confirmed, 1);
}
