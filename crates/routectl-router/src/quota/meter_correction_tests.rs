//! The contradiction rule corrects itself: a lower reading over a window in
//! effect is refused until enough consecutive readings agree on it.

use super::meter_tests::{
    Clock, FIVE_HOURS, SEVEN_DAYS, base_wall, header, key, mins, observation, partial, running,
    seat, store, unstarted,
};
use super::*;

use crate::quota::store::{ObserveRefusal, QuotaStore};

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
    let store = store();
    let t0 = clock.at(mins(0));
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t0,
                vec![running(key(FIVE_HOURS, None), 0.90, mins(reset_min), &t0)],
            ),
        )
        .expect("the seed is accepted");
    store
}

fn feed(store: &QuotaStore, at: ObservationStamp, next: Next) -> Result<(), ObserveRefusal> {
    let five = key(FIVE_HOURS, None);
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
    store
        .account(&seat("a"))
        .expect("an entry")
        .window(&key(FIVE_HOURS, None))
        .expect("a five-hour window")
        .reading()
        .clone()
}

/// One confirmation case: name, the three lower readings, and the used
/// fraction, reset minute and started state the third one leaves stored.
type ConfirmRow = (&'static str, [Next; 3], f64, Option<u64>, Started);

/// The reading taken on confirmation is the third one as reported: its own
/// used fraction, reset and started state, never a blend with the stored one.
#[test]
fn a_lower_reading_is_refused_twice_and_taken_on_the_third_agreeing_reading() {
    let rows: [ConfirmRow; 2] = [
        (
            "running lower on the same reset",
            [
                Next::Poll(0.30, 180),
                Next::Poll(0.30, 181),
                Next::Poll(0.31, 182),
            ],
            0.31,
            Some(182),
            Started::Yes,
        ),
        (
            "unstarted over a running window",
            [Next::Idle, Next::Idle, Next::Idle],
            0.0,
            None,
            Started::No,
        ),
    ];
    for (name, readings, used, reset_min, started) in rows {
        // Arrange
        let clock = Clock::new();
        let store = seeded(&clock, 180);

        // Act
        let outcomes = run(&store, &clock, &consecutive(&readings));

        // Assert
        assert_eq!(outcomes, [REFUSED, REFUSED, TAKEN], "row: {name}");
        assert_eq!(store.rejection_totals().contradictory, 2, "row: {name}");
        let stored = stored_five(&store);
        assert_eq!(stored.used().fraction(), used, "row: {name}");
        assert_eq!(
            stored.reset().map(|r| r.at()),
            reset_min.map(|m| base_wall() + mins(m)),
            "row: {name}"
        );
        assert_eq!(stored.started(), started, "row: {name}");
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
    let rows: [(&str, [u64; 4]); 2] = [
        ("a duplicate stamp", [1, 1, 2, 3]),
        ("an older stamp", [2, 1, 3, 4]),
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
}

/// Each event that clears a streak sends the next agreeing lower reading back
/// to the start: refused twice more before it is taken.
#[test]
fn each_clearing_event_restarts_the_streak() {
    let rows = [
        ("an accepted higher reading", Clearing::Higher),
        (
            "an accepted reading lifted within the slack",
            Clearing::Lifted,
        ),
        (
            "a complete report dropping the window",
            Clearing::CompleteReportDropsIt,
        ),
        ("forgetting the account", Clearing::Forget),
    ];
    let lower = Next::Poll(0.30, 180);
    for (name, clearing) in rows {
        // Arrange
        let clock = Clock::new();
        let store = seeded(&clock, 180);
        let primed = run(&store, &clock, &[(1, lower), (2, lower)]);
        assert_eq!(primed, [REFUSED, REFUSED], "row: {name}: primed");
        let t3 = clock.at(mins(3));
        match clearing {
            Clearing::Higher => assert_eq!(feed(&store, t3, Next::Poll(0.95, 180)), TAKEN),
            Clearing::Lifted => assert_eq!(feed(&store, t3, Next::Poll(0.89, 180)), TAKEN),
            Clearing::CompleteReportDropsIt => store
                .observe_account(
                    &seat("a"),
                    &observation(
                        ReadingSource::Poll,
                        t3,
                        ObservationReport::Complete(
                            WindowSet::new(vec![running(
                                key(SEVEN_DAYS, None),
                                0.20,
                                mins(3_000),
                                &t3,
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
                            t3,
                            vec![running(key(FIVE_HOURS, None), 0.90, mins(180), &t3)],
                        ),
                    )
                    .expect("the re-seed is accepted");
            }
        }

        // Act
        let outcomes = run(&store, &clock, &[(4, lower), (5, lower), (6, lower)]);

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

/// A refusal changes only the refused key's streak: every accepted field,
/// billing and ordering mark included, stays exactly as it was.
#[test]
fn a_refusal_leaves_everything_but_the_refused_keys_streak_unchanged() {
    // Arrange
    let clock = Clock::new();
    let store = seeded(&clock, 180);
    let t1 = clock.at(mins(1));
    let overage = QuotaSnapshot {
        billing: Billing::Overage,
        ..header(t1, 0.91, mins(180))
    };
    assert!(store.observe(&seat("a"), overage));
    let before = store.account(&seat("a")).expect("an entry");

    // Act
    let outcome = feed(&store, clock.at(mins(2)), Next::Poll(0.30, 180));

    // Assert
    assert_eq!(outcome, REFUSED);
    let after = store.account(&seat("a")).expect("an entry");
    assert_eq!(
        after.streaks.keys().collect::<Vec<_>>(),
        [&key(FIVE_HOURS, None)]
    );
    let mut without_streaks = after.clone();
    without_streaks.streaks.clear();
    assert_eq!(without_streaks, before);
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
