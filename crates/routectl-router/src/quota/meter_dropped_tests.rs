//! A complete report that drops a running window still in effect: later
//! readings for the account-wide five-hour key reconcile against the dropped
//! window until one is accepted or the window stops being in effect; any
//! other dropped window is kept for the view only, until a reading for its
//! key, its reset, or enough consecutive omissions.

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
    /// Accepted, holding the five-hour window at this used fraction.
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
        let fast = used_of(&store, &key(FIVE_HOURS, None));
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
            assert_eq!(fast, None, "row: {name}: never room");
            assert_eq!(store.rejection_totals().contradictory, 1, "row: {name}");
            continue;
        };
        assert_eq!(result, Ok(()), "row: {name}");
        assert!(
            meter.dropped_fast.is_none(),
            "row: {name}: the kept window clears"
        );
        assert_eq!(store.rejection_totals().contradictory, 0, "row: {name}");
        assert_eq!(
            fast,
            Some(expected),
            "row: {name}: the taken reading is held"
        );
    }
}

/// Only the five-hour window is kept as a baseline: a dropped running weekly
/// window is kept for the view alone, so a later unstarted weekly reading
/// replaces it and does not hold up the account's five-hour updates.
#[test]
fn a_dropped_weekly_window_is_no_contradiction_baseline() {
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
    let kept_before: Vec<WindowKey> = store
        .account(&seat("a"))
        .expect("an entry")
        .kept()
        .map(|kept| kept.reading().key().clone())
        .collect();

    // Act
    let result = store.observe_account(
        &seat("a"),
        &complete(
            t2,
            vec![
                unstarted(seven.clone(), None, &t2),
                unstarted(five, None, &t2),
            ],
        ),
    );

    // Assert
    assert_eq!(kept_before, vec![seven], "the control: the weekly is kept");
    assert_eq!(result, Ok(()));
    assert_eq!(store.rejection_totals().contradictory, 0);
    let meter = store.account(&seat("a")).expect("an entry");
    assert_eq!(meter.kept().count(), 0, "the reading replaces the kept one");
    assert_eq!(
        used_of(&store, &key(FIVE_HOURS, None)),
        Some(0.0),
        "the idle five-hour window is held"
    );
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
    assert_eq!(
        used_of(&store, &key(FIVE_HOURS, None)),
        Some(0.0),
        "the idle five-hour window is held"
    );
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

/// A complete report that lists a kept weekly window restarts its omission
/// count even when the listed reading itself is set aside (here it is older
/// than a header that named the weekly unknown), so two later omissions
/// still leave the window kept.
#[test]
fn a_complete_report_listing_a_kept_window_restarts_its_omission_count() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let five = key(FIVE_HOURS, None);
    let seven = key(SEVEN_DAYS, None);
    let t = |minute| clock.at(mins(minute));
    let complete = |at: ObservationStamp, weekly: Option<f64>| {
        let mut windows = vec![running(five.clone(), 0.10, mins(240), &at)];
        windows.extend(weekly.map(|used| running(seven.clone(), used, mins(4_320), &at)));
        observation(
            ReadingSource::Poll,
            at,
            ObservationReport::Complete(WindowSet::new(windows).expect("a set")),
        )
    };
    let observe = |observation: AccountObservation| {
        store
            .observe_account(&seat("a"), &observation)
            .expect("accepted");
    };
    observe(complete(t(0), Some(0.95)));
    observe(complete(t(1), None));
    assert!(store.observe(&seat("a"), header(t(3), 0.10, mins(240))));
    observe(complete(t(2), Some(0.30)));

    // Act
    observe(complete(t(4), None));
    observe(complete(t(5), None));

    // Assert
    let meter = store.account(&seat("a")).expect("an entry");
    let kept: Vec<f64> = meter
        .kept()
        .map(|kept| kept.reading().used().fraction())
        .collect();
    assert_eq!(kept, vec![0.95], "the weekly is still kept");
    observe(complete(t(6), None));
    let meter = store.account(&seat("a")).expect("an entry");
    assert_eq!(meter.kept().count(), 0, "the third omission lets it go");
}
