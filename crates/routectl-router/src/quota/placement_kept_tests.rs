//! A running window that a complete usage report omits still binds
//! placement: the meter keeps it aside, the placement-safe view lists it, and
//! the pace rule scores it. The five-hour window is kept until its reset;
//! any other until its reset, a reading for its key, or the third
//! consecutive complete report that omits it.

use super::*;

/// The seat's account-wide five-hour window at 95% with four hours of five
/// left, polled at the base.
fn feed_capped_five_hour(store: &QuotaStore, key: &SeatKey, clock: &Clock) {
    poll_anthropic(
        store,
        key,
        clock.at(Duration::ZERO),
        &anthropic_body(Some(&window(95, Some(hours(4.0)))), None, &[]),
    );
}

/// The pace of a 95% five-hour window resetting four hours past the base,
/// read `offset` past the base.
fn capped_pace(offset: Duration) -> f64 {
    let left = hours(4.0).saturating_sub(offset).as_secs_f64();
    0.05 / (left / hours(5.0).as_secs_f64())
}

/// One omission case: what the complete report that omits the five-hour
/// window carries, at what offset it arrives, and the standing read then.
struct OmitRow {
    name: &'static str,
    feed_capped: fn(&QuotaStore, &SeatKey, &Clock),
    omit: fn(&QuotaStore, &SeatKey, ObservationStamp),
    model: &'static str,
    expected: Standing,
}

fn codex_capped(store: &QuotaStore, key: &SeatKey, clock: &Clock) {
    poll_codex(
        store,
        key,
        clock.at(Duration::ZERO),
        &codex_window(95, hours(5.0), hours(4.0)),
        &codex_window(20, days(7.0), days(3.5)),
    );
}

fn omit_rows() -> Vec<OmitRow> {
    let at = Duration::from_mins(1);
    vec![
        OmitRow {
            name: "anthropic body with both windows null",
            feed_capped: feed_capped_five_hour,
            omit: |s, k, t| poll_anthropic(s, k, t, &anthropic_body(None, None, &[])),
            model: OPUS,
            expected: low(capped_pace(at), Reason::LowRoom),
        },
        OmitRow {
            name: "anthropic body reporting only a low weekly",
            feed_capped: feed_capped_five_hour,
            omit: |s, k, t| {
                poll_anthropic(
                    s,
                    k,
                    t,
                    &anthropic_body(None, Some(&window(20, Some(days(3.5)))), &[]),
                );
            },
            model: OPUS,
            expected: low(capped_pace(at), Reason::LowRoom),
        },
        OmitRow {
            name: "codex body without a primary window",
            feed_capped: codex_capped,
            omit: |s, k, t| poll_codex(s, k, t, "null", &codex_window(20, days(7.0), days(3.5))),
            model: CODEX,
            expected: low(capped_pace(at), Reason::LowRoom),
        },
    ]
}

#[test]
fn a_complete_report_omitting_a_running_five_hour_window_never_creates_room() {
    for row in omit_rows() {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let key = seat("a");
        (row.feed_capped)(&store, &key, &clock);
        let before = standing_of(&store, &key, row.model, &clock.at(Duration::ZERO));
        assert_eq!(before.tier, Tier::LowRoom, "row: {}: {before:?}", row.name);
        let at = clock.at(Duration::from_mins(1));

        // Act
        (row.omit)(&store, &key, at);
        let after = standing_of(&store, &key, row.model, &at);

        // Assert
        assert_standing(after, row.expected, row.name);
    }
}

#[test]
fn the_view_lists_the_kept_window_as_kept_beside_a_no_windows_report() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let key = seat("a");
    feed_capped_five_hour(&store, &key, &clock);
    let at = clock.at(Duration::from_mins(1));
    poll_anthropic(&store, &key, at, &anthropic_body(None, None, &[]));

    // Act
    let reading = reading_of(&store, &key, &at);

    // Assert
    assert_eq!(reading.report, ReportState::NoWindows);
    let [kept] = reading.windows.as_slice() else {
        panic!("one window: {reading:?}");
    };
    assert!(kept.kept, "{kept:?}");
    assert!(!kept.stale, "{kept:?}");
    assert_eq!(kept.window.length_secs, 5 * 3_600);
    assert!((kept.window.used - 0.95).abs() < 1e-9);
}

#[test]
fn a_kept_window_ages_on_its_own_stamp_and_then_reads_unread_not_roomy() {
    // Arrange: the omitting reports stay fresh while the kept window, last
    // observed at the base, ages past the stale bound.
    let clock = Clock::new();
    let store = store();
    let key = seat("a");
    feed_capped_five_hour(&store, &key, &clock);
    poll_anthropic(
        &store,
        &key,
        clock.at(Duration::from_mins(1)),
        &anthropic_body(None, None, &[]),
    );
    let later = clock.at(STALE_AFTER + Duration::from_mins(1));
    poll_anthropic(&store, &key, later, &anthropic_body(None, None, &[]));

    // Act
    let standing = standing_of(&store, &key, OPUS, &later);

    // Assert
    assert_standing(standing, unread(Reason::StaleReading), "aged kept window");
}

#[test]
fn once_the_kept_window_resets_a_no_windows_report_ranks_roomy() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let key = seat("a");
    feed_capped_five_hour(&store, &key, &clock);
    poll_anthropic(
        &store,
        &key,
        clock.at(Duration::from_mins(1)),
        &anthropic_body(None, None, &[]),
    );
    let past_reset = clock.at(hours(4.0) + Duration::from_mins(1));
    poll_anthropic(&store, &key, past_reset, &anthropic_body(None, None, &[]));

    // Act
    let standing = standing_of(&store, &key, OPUS, &past_reset);

    // Assert
    assert_standing(standing, roomy(1.0, Reason::NoCap), "past the kept reset");
    assert!(reading_of(&store, &key, &past_reset).windows.is_empty());
}

/// A 95% weekly window with three of seven days left beside a 10% five-hour
/// window, polled at the base: the weekly decides `low_room`.
fn feed_capped_weekly(store: &QuotaStore, key: &SeatKey, clock: &Clock) {
    poll_anthropic(
        store,
        key,
        clock.at(Duration::ZERO),
        &anthropic_body(
            Some(&window(10, Some(hours(4.0)))),
            Some(&window(95, Some(days(3.0)))),
            &[],
        ),
    );
}

/// As [`feed_capped_weekly`] with the 95% window scoped to Opus and the
/// account-wide weekly at 20%.
fn feed_capped_opus(store: &QuotaStore, key: &SeatKey, clock: &Clock) {
    poll_anthropic(
        store,
        key,
        clock.at(Duration::ZERO),
        &anthropic_body(
            Some(&window(10, Some(hours(4.0)))),
            Some(&window(20, Some(days(3.0)))),
            &[("opus", &window(95, Some(days(3.0))))],
        ),
    );
}

/// A Codex account with a 10% five-hour primary and a 95% weekly secondary
/// with three of seven days left.
fn codex_capped_weekly(store: &QuotaStore, key: &SeatKey, clock: &Clock) {
    poll_codex(
        store,
        key,
        clock.at(Duration::ZERO),
        &codex_window(10, hours(5.0), hours(4.0)),
        &codex_window(95, days(7.0), days(3.0)),
    );
}

/// The Anthropic body that keeps the five-hour at 10% and the weekly at 20%
/// but no longer lists the Opus window.
fn body_without_opus() -> String {
    anthropic_body(
        Some(&window(10, Some(hours(4.0)))),
        Some(&window(20, Some(days(3.0)))),
        &[],
    )
}

/// The pace of a 95% weekly window resetting three days past the base, read
/// `offset` past the base.
fn capped_weekly_pace(offset: Duration) -> f64 {
    let left = days(3.0).saturating_sub(offset).as_secs_f64();
    0.05 / (left / days(7.0).as_secs_f64())
}

fn omit_other_rows() -> Vec<OmitRow> {
    let at = Duration::from_mins(1);
    vec![
        OmitRow {
            name: "anthropic body with the weekly null",
            feed_capped: feed_capped_weekly,
            omit: |s, k, t| {
                poll_anthropic(
                    s,
                    k,
                    t,
                    &anthropic_body(Some(&window(10, Some(hours(4.0)))), None, &[]),
                );
            },
            model: OPUS,
            expected: low(capped_weekly_pace(at), Reason::LowRoom),
        },
        OmitRow {
            name: "anthropic body with both windows null",
            feed_capped: feed_capped_weekly,
            omit: |s, k, t| poll_anthropic(s, k, t, &anthropic_body(None, None, &[])),
            model: OPUS,
            expected: low(capped_weekly_pace(at), Reason::LowRoom),
        },
        OmitRow {
            name: "codex body without a secondary window",
            feed_capped: codex_capped_weekly,
            omit: |s, k, t| poll_codex(s, k, t, &codex_window(10, hours(5.0), hours(4.0)), "null"),
            model: CODEX,
            expected: low(capped_weekly_pace(at), Reason::LowRoom),
        },
        OmitRow {
            name: "anthropic body without the opus window, opus conversation",
            feed_capped: feed_capped_opus,
            omit: |s, k, t| poll_anthropic(s, k, t, &body_without_opus()),
            model: OPUS,
            expected: low(capped_weekly_pace(at), Reason::LowRoom),
        },
    ]
}

#[test]
fn a_complete_report_omitting_any_running_window_never_creates_room() {
    for row in omit_other_rows() {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let key = seat("a");
        (row.feed_capped)(&store, &key, &clock);
        let before = standing_of(&store, &key, row.model, &clock.at(Duration::ZERO));
        assert_eq!(before.tier, Tier::LowRoom, "row: {}: {before:?}", row.name);
        let at = clock.at(Duration::from_mins(1));

        // Act
        (row.omit)(&store, &key, at);
        let after = standing_of(&store, &key, row.model, &at);

        // Assert
        assert_standing(after, row.expected, row.name);
    }
}

#[test]
fn a_kept_opus_window_leaves_a_sonnet_conversation_unaffected() {
    // Arrange: the control store only ever saw the body without the Opus
    // window, so it holds exactly what the vendor reports now.
    let clock = Clock::new();
    let (store, control) = (store(), store());
    let key = seat("a");
    feed_capped_opus(&store, &key, &clock);
    let at = clock.at(Duration::from_mins(1));
    poll_anthropic(&store, &key, at, &body_without_opus());
    poll_anthropic(&control, &key, at, &body_without_opus());

    // Act
    let sonnet = standing_of(&store, &key, SONNET, &at);

    // Assert
    let expected = standing_of(&control, &key, SONNET, &at);
    assert_eq!(expected.tier, Tier::Roomy, "the control: {expected:?}");
    assert_standing(sonnet, expected, "sonnet beside a kept opus window");
    assert!(
        reading_of(&store, &key, &at)
            .windows
            .iter()
            .any(|w| w.kept && w.window.scope.as_deref() == Some("opus")),
        "the opus window is kept"
    );
}

#[test]
fn three_consecutive_complete_reports_omitting_a_kept_weekly_let_it_go() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let key = seat("a");
    feed_capped_weekly(&store, &key, &clock);
    let five_only = anthropic_body(Some(&window(10, Some(hours(4.0)))), None, &[]);
    let (t1, t2, t3) = (
        clock.at(Duration::from_mins(1)),
        clock.at(Duration::from_mins(2)),
        clock.at(Duration::from_mins(3)),
    );
    poll_anthropic(&store, &key, t1, &five_only);
    poll_anthropic(&store, &key, t2, &five_only);
    let after_two = standing_of(&store, &key, OPUS, &t2);

    // Act
    poll_anthropic(&store, &key, t3, &five_only);
    let after_three = standing_of(&store, &key, OPUS, &t3);

    // Assert
    assert_standing(
        after_two,
        low(capped_weekly_pace(Duration::from_mins(2)), Reason::LowRoom),
        "two omissions keep it",
    );
    let left = hours(4.0).saturating_sub(Duration::from_mins(3));
    let five_pace = 0.9 / (left.as_secs_f64() / hours(5.0).as_secs_f64());
    assert_standing(
        after_three,
        roomy(five_pace, Reason::Pace),
        "three let it go",
    );
    let reading = reading_of(&store, &key, &t3);
    assert!(reading.windows.iter().all(|w| !w.kept), "{reading:?}");
}

/// A kept window other than the five-hour is no contradiction baseline: a
/// reading for its key is never refused against it and simply replaces it,
/// even when lower and on a different reset.
#[test]
fn a_weekly_reported_again_replaces_the_kept_one() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let key = seat("a");
    feed_capped_weekly(&store, &key, &clock);
    let t1 = clock.at(Duration::from_mins(1));
    poll_anthropic(
        &store,
        &key,
        t1,
        &anthropic_body(Some(&window(10, Some(hours(4.0)))), None, &[]),
    );
    let t2 = clock.at(Duration::from_mins(2));

    // Act: `poll_anthropic` asserts the poll is accepted.
    poll_anthropic(
        &store,
        &key,
        t2,
        &anthropic_body(
            Some(&window(10, Some(hours(4.0)))),
            Some(&window(30, Some(days(5.0)))),
            &[],
        ),
    );

    // Assert
    assert_eq!(store.rejection_totals().contradictory, 0);
    let reading = reading_of(&store, &key, &t2);
    let weekly: Vec<&AccountWindow> = reading
        .windows
        .iter()
        .filter(|w| w.window.length_secs == 7 * 86_400)
        .collect();
    let [weekly] = weekly.as_slice() else {
        panic!("one weekly window: {reading:?}");
    };
    assert!(!weekly.kept, "{weekly:?}");
    assert!((weekly.window.used - 0.30).abs() < 1e-9, "{weekly:?}");
    assert_eq!(standing_of(&store, &key, OPUS, &t2).tier, Tier::Roomy);
}

#[test]
fn a_kept_weekly_is_let_go_once_its_reset_passes() {
    // Arrange: the weekly resets two hours past the base, the five-hour
    // later, and each poll after the drop omits the weekly, two in all.
    let clock = Clock::new();
    let store = store();
    let key = seat("a");
    let five = window(10, Some(hours(4.0)));
    poll_anthropic(
        &store,
        &key,
        clock.at(Duration::ZERO),
        &anthropic_body(Some(&five), Some(&window(95, Some(hours(2.0)))), &[]),
    );
    let five_only = anthropic_body(Some(&five), None, &[]);
    poll_anthropic(&store, &key, clock.at(Duration::from_mins(1)), &five_only);
    let kept_before = store.account(&key).expect("an entry").kept().count();
    let past_reset = clock.at(hours(2.0) + Duration::from_mins(1));

    // Act
    poll_anthropic(&store, &key, past_reset, &five_only);

    // Assert
    assert_eq!(kept_before, 1, "the control: the weekly is kept");
    assert_eq!(store.account(&key).expect("an entry").kept().count(), 0);
    assert_eq!(
        standing_of(&store, &key, OPUS, &past_reset).tier,
        Tier::Roomy
    );
}

#[test]
fn a_kept_five_hour_window_stands_beside_a_fresh_shorter_codex_window() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let key = seat("a");
    codex_capped(&store, &key, &clock);
    let at = clock.at(Duration::from_mins(1));

    // Act
    poll_codex(
        &store,
        &key,
        at,
        &codex_window(10, hours(4.0), hours(3.0)),
        &codex_window(20, days(7.0), days(3.5)),
    );

    // Assert
    let reading = reading_of(&store, &key, &at);
    let short: Vec<&AccountWindow> = reading
        .windows
        .iter()
        .filter(|w| w.window.length_secs < 86_400)
        .collect();
    let [four, five] = short.as_slice() else {
        panic!("two short windows: {reading:?}");
    };
    assert_eq!((four.window.length_secs, four.kept), (4 * 3_600, false));
    assert_eq!((five.window.length_secs, five.kept), (5 * 3_600, true));
    assert_eq!(reading.windows.iter().filter(|w| w.kept).count(), 1);
    assert_standing(
        standing_of(&store, &key, CODEX, &at),
        low(capped_pace(Duration::from_mins(1)), Reason::LowRoom),
        "the kept five-hour decides",
    );
}
