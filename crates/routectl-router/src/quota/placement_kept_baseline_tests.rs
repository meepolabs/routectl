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
    let mut quota = CodexQuota::default();
    quota.extras = [
        ("secondary-used-percent", "5".to_string()),
        ("secondary-window-minutes", "10080".to_string()),
        (
            "secondary-reset-after-seconds",
            days(3.0).as_secs().to_string(),
        ),
        ("secondary-reset-at", epoch_secs(days(3.0))),
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
