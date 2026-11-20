//! A running five-hour window that a complete usage report omits still binds
//! placement: the meter keeps it aside, the placement-safe view lists it, and
//! the pace rule scores it, until its reset passes.

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
