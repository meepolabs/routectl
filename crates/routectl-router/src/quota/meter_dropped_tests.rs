//! A complete report that drops the account-wide five-hour window while it is
//! running and in effect: later readings for the key reconcile against the
//! dropped window until one is accepted or the window stops being in effect.

use super::*;

/// What follows the dropping report.
enum AfterDrop {
    /// An unstarted poll window.
    Unstarted,
    /// A running poll window at this used fraction, on the dropped reset.
    Running(f64),
    /// A response header at this used fraction, on the dropped reset.
    Header(f64),
}

/// What the store holds for FAST after the next reading.
enum Outcome {
    /// Refused as `Contradictory`, nothing advanced.
    Refused,
    /// Accepted, projecting FAST known at this used fraction.
    Known(f64),
}

/// One drop case: name, whether the dropping report carries a weekly window,
/// what arrives next, the minute it arrives, and the outcome.
type DropRow = (&'static str, bool, AfterDrop, u64, Outcome);

fn drop_rows() -> [DropRow; 9] {
    [
        (
            "no windows, then idle",
            false,
            AfterDrop::Unstarted,
            2,
            Outcome::Refused,
        ),
        (
            "weekly only, then idle",
            true,
            AfterDrop::Unstarted,
            2,
            Outcome::Refused,
        ),
        (
            "no windows, then running at zero on the same reset",
            false,
            AfterDrop::Running(0.0),
            2,
            Outcome::Refused,
        ),
        (
            "no windows, then a header at zero on the same reset",
            false,
            AfterDrop::Header(0.0),
            2,
            Outcome::Refused,
        ),
        (
            "no windows, then running one point lower on the same reset",
            false,
            AfterDrop::Running(0.94),
            2,
            Outcome::Known(0.95),
        ),
        (
            "no windows, then running higher",
            false,
            AfterDrop::Running(0.97),
            2,
            Outcome::Known(0.97),
        ),
        (
            "no windows, then idle exactly at the reset",
            false,
            AfterDrop::Unstarted,
            180,
            Outcome::Known(0.0),
        ),
        (
            "no windows, then idle after the reset",
            false,
            AfterDrop::Unstarted,
            181,
            Outcome::Known(0.0),
        ),
        (
            "weekly only, then idle after the reset",
            true,
            AfterDrop::Unstarted,
            181,
            Outcome::Known(0.0),
        ),
    ]
}

fn dropping_report(reports_weekly: bool, at: &ObservationStamp) -> ObservationReport {
    if reports_weekly {
        ObservationReport::Complete(
            WindowSet::new(vec![running(key(SEVEN_DAYS, None), 0.20, mins(3_000), at)])
                .expect("a set"),
        )
    } else {
        ObservationReport::NoWindows
    }
}

/// Feeds `next` at `at`; `Ok(())` when it merged.
fn feed_after_drop(
    store: &QuotaStore,
    next: &AfterDrop,
    at: ObservationStamp,
) -> Result<(), ObserveRefusal> {
    let five = key(FIVE_HOURS, None);
    let reading = match next {
        AfterDrop::Header(used) => {
            return if store.observe(&seat("a"), header(at, *used, mins(180))) {
                Ok(())
            } else {
                Err(ObserveRefusal::Rejected(RejectionReason::Contradictory))
            };
        }
        AfterDrop::Unstarted => unstarted(five, None, &at),
        AfterDrop::Running(used) => running(five, *used, mins(180), &at),
    };
    store.observe_account(
        &seat("a"),
        &observation(
            ReadingSource::Poll,
            at,
            ObservationReport::Complete(WindowSet::new(vec![reading]).expect("a set")),
        ),
    )
}

/// A dropped running window still binds its key: an unstarted reading, or a
/// same-reset reading further below it than the slack, is a contradiction;
/// a same-reset reading within the slack is taken at the dropped fraction.
/// Once its reset is reached, or a reading is accepted, it no longer binds.
#[test]
fn a_reading_after_a_complete_report_dropped_a_running_five_hour_window() {
    for (name, reports_weekly, next, minute, outcome) in drop_rows() {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let (t0, t1, t2) = (clock.at(mins(0)), clock.at(mins(1)), clock.at(mins(minute)));
        seed(&store, &Before::PollRunning, t0);
        store
            .observe_account(
                &seat("a"),
                &observation(
                    ReadingSource::Poll,
                    t1,
                    dropping_report(reports_weekly, &t1),
                ),
            )
            .expect("the dropping report is accepted");
        let held_before = accepted_state(store.account(&seat("a")));

        // Act
        let result = feed_after_drop(&store, &next, t2);

        // Assert
        let fast = store.reading_for(&seat("a"), &t2).expect("an entry").fast;
        let meter = store.account(&seat("a")).expect("an entry");
        let Outcome::Known(expected) = outcome else {
            assert_eq!(
                result,
                Err(ObserveRefusal::Rejected(RejectionReason::Contradictory)),
                "row: {name}"
            );
            assert_eq!(
                accepted_state(Some(meter)),
                held_before,
                "row: {name}: no accepted state advances"
            );
            assert_eq!(fast, QuotaWindow::Unknown, "row: {name}: never room");
            assert_eq!(store.rejection_totals().contradictory, 1, "row: {name}");
            continue;
        };
        assert_eq!(result, Ok(()), "row: {name}");
        assert!(
            meter.dropped_fast.is_none(),
            "row: {name}: the kept window clears"
        );
        assert_eq!(store.rejection_totals().contradictory, 0, "row: {name}");
        let QuotaWindow::Known { utilization, .. } = fast else {
            panic!("row: {name}: the taken reading is known: {fast:?}");
        };
        assert_eq!(utilization.fraction(), expected, "row: {name}");
    }
}

/// Only the five-hour window is kept: a dropped running weekly window leaves
/// nothing behind, so a later unstarted weekly reading does not hold up the
/// account's five-hour updates.
#[test]
fn a_dropped_weekly_window_is_not_kept() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let five = key(FIVE_HOURS, None);
    let seven = key(SEVEN_DAYS, None);
    let (t0, t1, t2) = (clock.at(mins(0)), clock.at(mins(1)), clock.at(mins(2)));
    let complete = |at: ObservationStamp, windows: Vec<WindowReading>| {
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
                    running(seven.clone(), 0.50, mins(6 * 1_440), &t0),
                    unstarted(five.clone(), None, &t0),
                ],
            ),
        )
        .expect("accepted");
    store
        .observe_account(
            &seat("a"),
            &complete(t1, vec![unstarted(five.clone(), None, &t1)]),
        )
        .expect("the report dropping the weekly window is accepted");

    // Act
    let result = store.observe_account(
        &seat("a"),
        &complete(
            t2,
            vec![unstarted(seven, None, &t2), unstarted(five, None, &t2)],
        ),
    );

    // Assert
    assert_eq!(result, Ok(()));
    assert_eq!(store.rejection_totals().contradictory, 0);
    let fast = store.reading_for(&seat("a"), &t2).expect("an entry").fast;
    let QuotaWindow::Known { utilization, .. } = fast else {
        panic!("the idle five-hour window is known: {fast:?}");
    };
    assert_eq!(utilization.fraction(), 0.0);
}

/// An unstarted window's reset is provisional, not an expiry: dropping one
/// keeps nothing, and the next idle reading is taken.
#[test]
fn a_dropped_unstarted_window_with_a_provisional_reset_is_not_kept() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let five = key(FIVE_HOURS, None);
    let (t0, t1, t2) = (clock.at(mins(0)), clock.at(mins(1)), clock.at(mins(2)));
    store
        .observe_account(
            &seat("a"),
            &partial(
                ReadingSource::Poll,
                t0,
                vec![unstarted(five.clone(), Some(mins(300)), &t0)],
            ),
        )
        .expect("accepted");
    store
        .observe_account(
            &seat("a"),
            &observation(ReadingSource::Poll, t1, ObservationReport::NoWindows),
        )
        .expect("the dropping report is accepted");
    let kept = store.account(&seat("a")).expect("an entry").dropped_fast;

    // Act
    let result = store.observe_account(
        &seat("a"),
        &partial(
            ReadingSource::Poll,
            t2,
            vec![unstarted(five, Some(mins(302)), &t2)],
        ),
    );

    // Assert
    assert_eq!(kept, None, "nothing is kept for an unstarted window");
    assert_eq!(result, Ok(()));
    let fast = store.reading_for(&seat("a"), &t2).expect("an entry").fast;
    let QuotaWindow::Known { utilization, .. } = fast else {
        panic!("the idle five-hour window is known: {fast:?}");
    };
    assert_eq!(utilization.fraction(), 0.0);
}

/// A kept window ages on the monotonic ceiling like a stored one: after the
/// wall clock steps back, it stops binding once its monotonic age passes the
/// ceiling, even though its reset is still ahead on the wall clock.
#[test]
fn a_kept_window_stops_binding_past_the_monotonic_ceiling() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let five = key(FIVE_HOURS, None);
    let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
    seed(&store, &Before::PollRunning, t0);
    store
        .observe_account(
            &seat("a"),
            &observation(ReadingSource::Poll, t1, ObservationReport::NoWindows),
        )
        .expect("the dropping report is accepted");
    let past_ceiling = MAX_OBSERVATION_AGE + mins(2);
    let stepped_back =
        ObservationStamp::from_parts(base_wall() + mins(2), clock.monotonic + past_ceiling);

    // Act
    let result = store.observe_account(
        &seat("a"),
        &partial(
            ReadingSource::Poll,
            stepped_back,
            vec![unstarted(five, None, &stepped_back)],
        ),
    );

    // Assert
    assert_eq!(result, Ok(()));
    assert_eq!(store.rejection_totals().contradictory, 0);
    let meter = store.account(&seat("a")).expect("an entry");
    assert!(meter.dropped_fast.is_none(), "the kept window clears");
}

/// A kept window is let go once it is no longer in effect, even when no
/// later reading names its key.
#[test]
fn a_kept_window_clears_at_its_reset_without_a_reading_for_its_key() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let (t0, t1) = (clock.at(mins(0)), clock.at(mins(1)));
    seed(&store, &Before::PollRunning, t0);
    let no_windows =
        |at: ObservationStamp| observation(ReadingSource::Poll, at, ObservationReport::NoWindows);
    store
        .observe_account(&seat("a"), &no_windows(t1))
        .expect("the dropping report is accepted");
    let kept_before = store.account(&seat("a")).expect("an entry").dropped_fast;

    // Act
    store
        .observe_account(&seat("a"), &no_windows(clock.at(mins(180))))
        .expect("accepted");

    // Assert
    assert!(kept_before.is_some(), "the running window is kept at first");
    let meter = store.account(&seat("a")).expect("an entry");
    assert!(meter.dropped_fast.is_none(), "the kept window clears");
}
