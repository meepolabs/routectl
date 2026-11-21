//! A kept window of any key is the contradiction baseline for its key while
//! it is kept: after one omitting report, a lower or unstarted reading for
//! the key, from a poll or a response header, still needs the agreeing run
//! a stored window needs before it can make the account look roomy.

use super::*;

const WEEK_SECS: u64 = 7 * 86_400;

/// One baseline case: how the capped account is fed and how its weekly is
/// then omitted, the lower reading repeated after the omission (true when
/// accepted), the model asked about, and the used fraction the confirmed
/// reading leaves.
struct BaselineRow {
    name: &'static str,
    feed_capped: fn(&QuotaStore, &SeatKey, &Clock),
    omit: fn(&QuotaStore, &SeatKey, ObservationStamp),
    lower: fn(&QuotaStore, &SeatKey, ObservationStamp) -> bool,
    model: &'static str,
    new_used: &'static str,
}

fn omit_anthropic_weekly(store: &QuotaStore, key: &SeatKey, at: ObservationStamp) {
    poll_anthropic(
        store,
        key,
        at,
        &anthropic_body(Some(&window(10, Some(hours(4.0)))), None, &[]),
    );
}

fn omit_codex_weekly(store: &QuotaStore, key: &SeatKey, at: ObservationStamp) {
    poll_codex(
        store,
        key,
        at,
        &codex_window(10, hours(5.0), hours(4.0)),
        "null",
    );
}

/// An Anthropic usage body with the five-hour unchanged and `weekly`.
fn anthropic_weekly_poll(
    store: &QuotaStore,
    key: &SeatKey,
    at: ObservationStamp,
    weekly: &str,
) -> bool {
    let body = anthropic_body(Some(&window(10, Some(hours(4.0)))), Some(weekly), &[]);
    let observation = reduce_anthropic_body(body.as_bytes(), at).expect("a valid usage body");
    store.observe_account(key, &observation).is_ok()
}

/// Anthropic response headers: the five-hour unchanged, the weekly at 5% on
/// the kept reset.
fn anthropic_low_weekly_header(store: &QuotaStore, key: &SeatKey, at: ObservationStamp) -> bool {
    let reset = epoch_secs(days(3.0));
    let quota = anthropic_headers(
        "0.1",
        hours(4.0),
        &[("7d-utilization", "0.05"), ("7d-reset", &reset)],
        None,
    );
    store.observe(key, reduce_anthropic(&quota, &at).snapshot)
}

/// Codex response headers naming only the weekly secondary, at 5% on the
/// kept reset.
fn codex_low_weekly_header(store: &QuotaStore, key: &SeatKey, at: ObservationStamp) -> bool {
    codex_weekly_header(store, key, at, 5, days(3.0))
}

/// Codex response headers naming only the weekly secondary, at `percent`
/// and resetting `reset` past the base.
fn codex_weekly_header(
    store: &QuotaStore,
    key: &SeatKey,
    at: ObservationStamp,
    percent: u32,
    reset: Duration,
) -> bool {
    let reset_after =
        reset.saturating_sub(at.wall().duration_since(base_wall()).unwrap_or_default());
    let mut quota = CodexQuota::default();
    quota.extras = [
        ("secondary-used-percent", percent.to_string()),
        ("secondary-window-minutes", "10080".to_string()),
        (
            "secondary-reset-after-seconds",
            reset_after.as_secs().to_string(),
        ),
        ("secondary-reset-at", epoch_secs(reset)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let observation = reduce_codex_headers(&quota, at)
        .observation
        .expect("the headers carry the weekly");
    store.observe_account(key, &observation).is_ok()
}

fn baseline_rows() -> [BaselineRow; 4] {
    [
        BaselineRow {
            name: "a 5% weekly body",
            feed_capped: feed_capped_weekly,
            omit: omit_anthropic_weekly,
            lower: |s, k, t| anthropic_weekly_poll(s, k, t, &window(5, Some(days(3.0)))),
            model: OPUS,
            new_used: "0.05",
        },
        BaselineRow {
            name: "a 5% anthropic 7d-utilization header",
            feed_capped: feed_capped_weekly,
            omit: omit_anthropic_weekly,
            lower: anthropic_low_weekly_header,
            model: OPUS,
            new_used: "0.05",
        },
        BaselineRow {
            name: "an unstarted weekly body",
            feed_capped: feed_capped_weekly,
            omit: omit_anthropic_weekly,
            lower: |s, k, t| anthropic_weekly_poll(s, k, t, &window(0, None)),
            model: OPUS,
            new_used: "0.0",
        },
        BaselineRow {
            name: "a codex secondary header for the weekly key",
            feed_capped: codex_capped_weekly,
            omit: omit_codex_weekly,
            lower: codex_low_weekly_header,
            model: CODEX,
            new_used: "0.05",
        },
    ]
}

/// The `quota_correction_confirmed` lines `events` carries, as
/// (`window_secs`, `old_used`, `new_used`).
fn confirmed_lines(events: &[routectl_testkit::CapturedEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter(|e| e.field("event") == Some("quota_correction_confirmed"))
        .map(|e| {
            let field = |name| e.field(name).unwrap_or_default().to_string();
            (field("window_secs"), field("old_used"), field("new_used"))
        })
        .collect()
}

#[test]
fn a_lower_reading_after_an_omission_needs_three_agreeing_readings() {
    for row in baseline_rows() {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let key = seat("a");
        (row.feed_capped)(&store, &key, &clock);
        (row.omit)(&store, &key, clock.at(Duration::from_mins(1)));
        let at = |minute| clock.at(Duration::from_mins(minute));
        let kept_before = store.account(&key).expect("an entry").kept().count();

        // Act
        let mut held = Vec::new();
        for minute in [2, 3] {
            let accepted = (row.lower)(&store, &key, at(minute));
            let tier = standing_of(&store, &key, row.model, &at(minute)).tier;
            held.push((accepted, tier, store.rejection_totals().contradictory));
        }
        let mut confirmed = false;
        let events = routectl_testkit::capture_events(|| {
            confirmed = (row.lower)(&store, &key, at(4));
        });

        // Assert
        let name = row.name;
        assert_eq!(
            kept_before, 1,
            "row: {name}: the control: the weekly is kept"
        );
        assert_eq!(
            held,
            [(false, Tier::LowRoom, 1), (false, Tier::LowRoom, 2)],
            "row: {name}: the first two are refused and the kept weekly binds"
        );
        assert!(
            confirmed,
            "row: {name}: the third agreeing reading is taken"
        );
        assert_eq!(
            confirmed_lines(&events),
            [(
                WEEK_SECS.to_string(),
                "0.95".to_string(),
                row.new_used.to_string()
            )],
            "row: {name}"
        );
        assert_eq!(store.account(&key).expect("an entry").kept().count(), 0);
        assert_eq!(
            standing_of(&store, &key, row.model, &at(4)).tier,
            Tier::Roomy,
            "row: {name}"
        );
    }
}

/// The reset a near-reset reading names: two minutes past the base, so it
/// has passed when the low reading arrives at minute 4.
const NEAR_RESET: Duration = Duration::from_mins(2);

/// An Anthropic usage body with `five` and `seven` as given, true when
/// accepted.
fn anthropic_poll(store: &QuotaStore, key: &SeatKey, at: ObservationStamp, body: &str) -> bool {
    let observation = reduce_anthropic_body(body.as_bytes(), at).expect("a valid usage body");
    store.observe_account(key, &observation).is_ok()
}

/// Anthropic response headers with the five-hour at `utilization`
/// resetting `reset` past the base, plus `extras`, true when accepted.
fn anthropic_header(
    store: &QuotaStore,
    key: &SeatKey,
    at: ObservationStamp,
    utilization: &str,
    reset: Duration,
    extras: &[(&str, &str)],
) -> bool {
    let quota = anthropic_headers(utilization, reset, extras, None);
    store.observe(key, reduce_anthropic(&quota, &at).snapshot)
}

fn omit_anthropic_five_hour(store: &QuotaStore, key: &SeatKey, at: ObservationStamp) {
    poll_anthropic(store, key, at, &anthropic_body(None, None, &[]));
}

/// One early-reset case: how the capped account is fed and its window then
/// omitted, the reading that names a near reset at higher used, the low
/// reading on the real reset that follows once the near reset has passed
/// (each true when accepted), and the model asked about.
struct EarlyResetRow {
    name: &'static str,
    feed_capped: fn(&QuotaStore, &SeatKey, &Clock),
    omit: fn(&QuotaStore, &SeatKey, ObservationStamp),
    near: fn(&QuotaStore, &SeatKey, ObservationStamp) -> bool,
    low: fn(&QuotaStore, &SeatKey, ObservationStamp) -> bool,
    model: &'static str,
}

fn early_reset_rows() -> [EarlyResetRow; 5] {
    [
        EarlyResetRow {
            name: "weekly, anthropic usage bodies",
            feed_capped: feed_capped_weekly,
            omit: omit_anthropic_weekly,
            near: |s, k, t| anthropic_weekly_poll(s, k, t, &window(96, Some(NEAR_RESET))),
            low: |s, k, t| anthropic_weekly_poll(s, k, t, &window(5, Some(days(3.0)))),
            model: OPUS,
        },
        EarlyResetRow {
            name: "weekly, anthropic 7d headers",
            feed_capped: feed_capped_weekly,
            omit: omit_anthropic_weekly,
            near: |s, k, t| {
                let reset = epoch_secs(NEAR_RESET);
                let extras = [("7d-utilization", "0.96"), ("7d-reset", reset.as_str())];
                anthropic_header(s, k, t, "0.1", hours(4.0), &extras)
            },
            low: anthropic_low_weekly_header,
            model: OPUS,
        },
        EarlyResetRow {
            name: "weekly, codex secondary headers",
            feed_capped: codex_capped_weekly,
            omit: omit_codex_weekly,
            near: |s, k, t| codex_weekly_header(s, k, t, 96, NEAR_RESET),
            low: codex_low_weekly_header,
            model: CODEX,
        },
        EarlyResetRow {
            name: "five-hour, anthropic usage bodies",
            feed_capped: feed_capped_five_hour,
            omit: omit_anthropic_five_hour,
            near: |s, k, t| {
                anthropic_poll(
                    s,
                    k,
                    t,
                    &anthropic_body(Some(&window(96, Some(NEAR_RESET))), None, &[]),
                )
            },
            low: |s, k, t| {
                anthropic_poll(
                    s,
                    k,
                    t,
                    &anthropic_body(Some(&window(5, Some(hours(4.0)))), None, &[]),
                )
            },
            model: OPUS,
        },
        EarlyResetRow {
            name: "five-hour, anthropic 5h headers",
            feed_capped: feed_capped_five_hour,
            omit: omit_anthropic_five_hour,
            near: |s, k, t| anthropic_header(s, k, t, "0.96", NEAR_RESET, &[]),
            low: |s, k, t| anthropic_header(s, k, t, "0.05", hours(4.0), &[]),
            model: OPUS,
        },
    ]
}

/// A kept window cannot end early either: a reading naming a near reset is
/// refused even at higher used, so the low reading after that reset passes
/// still meets the kept window and the seat keeps reading low on room.
#[test]
fn a_reading_resetting_earlier_than_a_kept_window_is_lower() {
    for row in early_reset_rows() {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let key = seat("a");
        (row.feed_capped)(&store, &key, &clock);
        (row.omit)(&store, &key, clock.at(Duration::from_mins(1)));
        let at = |minute| clock.at(Duration::from_mins(minute));
        let kept_before = store.account(&key).expect("an entry").kept().count();

        // Act
        let near = (row.near)(&store, &key, clock.at(Duration::from_secs(90)));
        let low = (row.low)(&store, &key, at(4));

        // Assert
        let name = row.name;
        assert_eq!(
            kept_before, 1,
            "row: {name}: the control: the window is kept"
        );
        assert_eq!([near, low], [false, false], "row: {name}");
        assert_eq!(store.rejection_totals().contradictory, 2, "row: {name}");
        assert_eq!(
            store.account(&key).expect("an entry").kept().count(),
            1,
            "row: {name}"
        );
        assert_eq!(
            standing_of(&store, &key, row.model, &at(4)).tier,
            Tier::LowRoom,
            "row: {name}"
        );
    }
}
