//! Tests for the pace rule.
//!
//! Every standing here is read from a real reading: a usage body or response
//! headers reduced by the vendor's own reducer, merged by the meter, and read
//! back through the placement-safe view. A hand-built `AccountReading` would
//! assert the rule against a shape the meter may never produce.

use super::*;

use std::time::{Duration, Instant, SystemTime};

use routectl_core::upstream_meta::{AnthropicUnifiedQuota, CodexQuota, OVERAGE_CLAIM};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::quota::codex_headers::reduce_codex_headers;
use crate::quota::key::{SeatKey, seat_key_for_secret_ref};
use crate::quota::reduce::reduce_anthropic;
use crate::quota::store::QuotaStore;
use crate::quota::usage_body::{reduce_anthropic_body, reduce_codex_body};
use crate::quota::view::STALE_AFTER;

/// 2026-11-17T12:00:00Z, a whole second so header resets are exact.
const BASE_EPOCH_SECS: u64 = 1_794_916_800;
const OPUS: &str = "claude-opus-4-7";
const SONNET: &str = "claude-sonnet-4-5";
const CODEX: &str = "gpt-5-codex";
const ONE_MS: Duration = Duration::from_millis(1);

fn base_wall() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(BASE_EPOCH_SECS)
}

fn hours(h: f64) -> Duration {
    Duration::from_secs_f64(h * 3_600.0)
}

fn days(d: f64) -> Duration {
    hours(d * 24.0)
}

/// Stamps an offset past one base on both clocks.
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

fn seat(label: &str) -> SeatKey {
    let secret_ref = routectl_auth::SecretRef::OAuth {
        provider: "anthropic".to_string(),
        label: Some(label.to_string()),
    };
    seat_key_for_secret_ref(Some(&secret_ref)).expect("an oauth ref yields a key")
}

fn store() -> QuotaStore {
    let store = QuotaStore::default();
    store.admit_seats([seat("a"), seat("b")]);
    store
}

fn rfc3339(offset: Duration) -> String {
    let secs = (base_wall() + offset)
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("a post-epoch instant")
        .as_secs();
    OffsetDateTime::from_unix_timestamp(i64::try_from(secs).expect("a representable instant"))
        .expect("a valid timestamp")
        .format(&Rfc3339)
        .expect("an RFC 3339 rendering")
}

fn epoch_secs(offset: Duration) -> String {
    (BASE_EPOCH_SECS + offset.as_secs()).to_string()
}

/// One Anthropic usage-body window: `percent` used, resetting `reset` past
/// the base, or unstarted (`null` reset) when `reset` is `None`.
fn window(percent: u32, reset: Option<Duration>) -> String {
    let reset = reset.map_or_else(|| "null".to_string(), |at| format!("\"{}\"", rfc3339(at)));
    format!(r#"{{"utilization": {percent}, "resets_at": {reset}}}"#)
}

/// An Anthropic usage body; `None` reports the member as `null`.
fn anthropic_body(five: Option<&str>, seven: Option<&str>, scoped: &[(&str, &str)]) -> String {
    let mut members = vec![
        format!(r#""five_hour": {}"#, five.unwrap_or("null")),
        format!(r#""seven_day": {}"#, seven.unwrap_or("null")),
    ];
    members.extend(
        scoped
            .iter()
            .map(|(name, window)| format!(r#""seven_day_{name}": {window}"#)),
    );
    format!("{{{}}}", members.join(", "))
}

fn poll_anthropic(store: &QuotaStore, key: &SeatKey, at: ObservationStamp, body: &str) {
    let observation = reduce_anthropic_body(body.as_bytes(), at).expect("a valid usage body");
    store
        .observe_account(key, &observation)
        .expect("the poll is accepted");
}

/// Anthropic response headers with a five-hour window at `utilization`
/// resetting `reset` past the base, plus `extras`.
fn anthropic_headers(
    utilization: &str,
    reset: Duration,
    extras: &[(&str, &str)],
    claim: Option<&str>,
) -> AnthropicUnifiedQuota {
    let mut quota = AnthropicUnifiedQuota::default();
    quota.utilization = Some(utilization.into());
    quota.representative_claim = claim.map(str::to_string);
    quota.extras = std::iter::once(("5h-reset".to_string(), epoch_secs(reset)))
        .chain(
            extras
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        )
        .collect();
    quota
}

fn header_anthropic(
    store: &QuotaStore,
    key: &SeatKey,
    at: ObservationStamp,
    quota: &AnthropicUnifiedQuota,
) {
    assert!(
        store.observe(key, reduce_anthropic(quota, &at).snapshot),
        "the header snapshot is accepted"
    );
}

fn codex_window(percent: u32, length: Duration, reset_after: Duration) -> String {
    format!(
        r#"{{"used_percent": {percent}, "limit_window_seconds": {}, "reset_after_seconds": {}, "reset_at": {}}}"#,
        length.as_secs(),
        reset_after.as_secs(),
        epoch_secs(reset_after),
    )
}

fn poll_codex(
    store: &QuotaStore,
    key: &SeatKey,
    at: ObservationStamp,
    primary: &str,
    secondary: &str,
) {
    let body = format!(
        r#"{{"plan_type": "plus", "rate_limit": {{"allowed": true, "limit_reached": false, "primary_window": {primary}, "secondary_window": {secondary}}}}}"#
    );
    let observation = reduce_codex_body(body.as_bytes(), at).expect("a valid usage body");
    store
        .observe_account(key, &observation)
        .expect("the poll is accepted");
}

/// Codex response headers: a five-hour primary at 25% with two hours left and
/// a weekly secondary at 40% with three days left.
fn header_codex(store: &QuotaStore, key: &SeatKey, at: ObservationStamp) {
    let mut quota = CodexQuota::default();
    quota.primary_used_percent = Some("25".into());
    quota.primary_reset_at = Some(epoch_secs(hours(2.0)));
    quota.extras = [
        ("primary-window-minutes", "300".to_string()),
        ("primary-reset-after-seconds", "7200".to_string()),
        ("secondary-used-percent", "40".to_string()),
        ("secondary-window-minutes", "10080".to_string()),
        ("secondary-reset-after-seconds", "259200".to_string()),
        ("secondary-reset-at", epoch_secs(days(3.0))),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let observation = reduce_codex_headers(&quota, at)
        .observation
        .expect("the headers carry windows");
    store
        .observe_account(key, &observation)
        .expect("the headers are accepted");
}

/// `key`'s reading from the placement-safe view at `now`.
fn reading_of(store: &QuotaStore, key: &SeatKey, now: &ObservationStamp) -> AccountReading {
    store
        .readings(now)
        .accounts
        .into_iter()
        .find(|reading| reading.account == key.as_str())
        .expect("the seat is configured")
}

fn standing_of(store: &QuotaStore, key: &SeatKey, model: &str, now: &ObservationStamp) -> Standing {
    standing(Some(&reading_of(store, key, now)), model, now)
}

fn roomy(score: f64, reason: Reason) -> Standing {
    Standing {
        tier: Tier::Roomy,
        score: Some(score),
        reason,
    }
}

fn low(score: f64, reason: Reason) -> Standing {
    Standing {
        tier: Tier::LowRoom,
        score: Some(score),
        reason,
    }
}

fn unread(reason: Reason) -> Standing {
    Standing {
        tier: Tier::Unread,
        score: None,
        reason,
    }
}

fn assert_standing(actual: Standing, expected: Standing, name: &str) {
    assert_eq!(
        (actual.tier, actual.reason),
        (expected.tier, expected.reason),
        "row: {name}: {actual:?}"
    );
    match (actual.score, expected.score) {
        (Some(a), Some(e)) => assert!((a - e).abs() < 1e-9, "row: {name}: score {a} != {e}"),
        (a, e) => assert_eq!(a, e, "row: {name}"),
    }
}

/// One scoring row: what the account reported, read at what offset, for which
/// model, and the standing that must result.
struct Row {
    name: &'static str,
    feed: fn(&QuotaStore, &SeatKey, &Clock),
    read_at: Duration,
    model: &'static str,
    expected: Standing,
}

fn scoring_rows() -> Vec<Row> {
    vec![
        Row {
            name: "unstarted five-hour window",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(Some(&window(0, None)), None, &[]),
                )
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(1.0, Reason::Unstarted),
        },
        Row {
            name: "unstarted weekly window",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(None, Some(&window(0, None)), &[]),
                )
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(1.0, Reason::Unstarted),
        },
        Row {
            name: "running 0.4 with 2.5 h of 5 h left",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(Some(&window(40, Some(hours(2.5)))), None, &[]),
                )
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(1.2, Reason::Pace),
        },
        Row {
            name: "a weekly worse than the five-hour decides",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(
                        Some(&window(20, Some(hours(2.5)))),
                        Some(&window(50, Some(days(3.5)))),
                        &[],
                    ),
                );
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(1.0, Reason::Pace),
        },
        Row {
            name: "a five-hour worse than the weekly decides",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(
                        Some(&window(80, Some(hours(4.0)))),
                        Some(&window(10, Some(days(3.5)))),
                        &[],
                    ),
                );
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(0.25, Reason::Pace),
        },
        Row {
            name: "fresh no-windows report",
            feed: |s, k, c| {
                poll_anthropic(s, k, c.at(Duration::ZERO), &anthropic_body(None, None, &[]))
            },
            read_at: STALE_AFTER,
            model: OPUS,
            expected: roomy(1.0, Reason::NoCap),
        },
        Row {
            name: "no-windows report aged past the stale bound",
            feed: |s, k, c| {
                poll_anthropic(s, k, c.at(Duration::ZERO), &anthropic_body(None, None, &[]))
            },
            read_at: STALE_AFTER + ONE_MS,
            model: OPUS,
            expected: unread(Reason::NoReading),
        },
        Row {
            name: "unstarted five-hour with a weekly at 0.95",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(
                        Some(&window(0, None)),
                        Some(&window(95, Some(days(3.0)))),
                        &[],
                    ),
                );
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: low(0.05 / (3.0 / 7.0), Reason::LowRoom),
        },
        Row {
            name: "room exactly 0.10",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(Some(&window(90, Some(hours(2.5)))), None, &[]),
                )
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(0.2, Reason::Pace),
        },
        Row {
            name: "room 0.09",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(Some(&window(91, Some(hours(2.5)))), None, &[]),
                )
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: low(0.18, Reason::LowRoom),
        },
        Row {
            name: "fresh overage billing",
            feed: |s, k, c| {
                header_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_headers("0.1", hours(2.5), &[], Some(OVERAGE_CLAIM)),
                )
            },
            read_at: Duration::from_mins(1),
            model: OPUS,
            expected: low(0.0, Reason::Overage),
        },
        Row {
            name: "header without a fallback percentage",
            feed: |s, k, c| {
                header_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_headers("0.4", hours(2.5), &[], Some("five_hour")),
                )
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(1.2, Reason::Pace),
        },
        Row {
            name: "header with a fallback percentage",
            feed: |s, k, c| {
                header_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_headers(
                        "0.4",
                        hours(2.5),
                        &[("fallback-percentage", "0.5")],
                        Some("five_hour"),
                    ),
                );
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(1.2, Reason::Pace),
        },
        Row {
            name: "an opus weekly at 0.95 binds an opus model",
            feed: feed_with_opus_weekly,
            read_at: Duration::ZERO,
            model: OPUS,
            expected: low(0.05 / (3.0 / 7.0), Reason::LowRoom),
        },
        Row {
            name: "an opus weekly at 0.95 does not bind a sonnet model",
            feed: feed_with_opus_weekly,
            read_at: Duration::ZERO,
            model: SONNET,
            expected: roomy(1.6, Reason::Pace),
        },
        Row {
            name: "an unknown scope at 0.99 is ignored",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(
                        Some(&window(10, Some(hours(2.5)))),
                        Some(&window(20, Some(days(3.5)))),
                        &[("oauth_apps", &window(99, Some(days(3.0))))],
                    ),
                );
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(1.6, Reason::Pace),
        },
        Row {
            name: "one stale relevant window beside a fresh one",
            feed: |s, k, c| {
                feed_five_hour_and_weekly(s, k, c);
                header_anthropic(
                    s,
                    k,
                    c.at(Duration::from_mins(6)),
                    &anthropic_headers("0.1", hours(2.5), &[], None),
                );
            },
            read_at: Duration::from_mins(8),
            model: OPUS,
            expected: unread(Reason::StaleReading),
        },
        Row {
            name: "both windows refreshed (control for the stale row)",
            feed: |s, k, c| {
                feed_five_hour_and_weekly(s, k, c);
                header_anthropic(
                    s,
                    k,
                    c.at(Duration::from_mins(6)),
                    &anthropic_headers(
                        "0.1",
                        hours(2.5),
                        &[
                            ("7d-utilization", "0.2"),
                            ("7d-reset", &epoch_secs(days(3.5))),
                        ],
                        None,
                    ),
                );
            },
            read_at: Duration::from_mins(8),
            model: OPUS,
            expected: roomy(0.8 / (5_032.0 / 10_080.0), Reason::Pace),
        },
        Row {
            name: "a codex poll",
            feed: |s, k, c| {
                poll_codex(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &codex_window(30, hours(5.0), hours(1.0)),
                    &codex_window(64, days(7.0), days(2.0)),
                );
            },
            read_at: Duration::ZERO,
            model: CODEX,
            expected: roomy(0.36 / (2.0 / 7.0), Reason::Pace),
        },
        Row {
            name: "codex headers",
            feed: |s, k, c| header_codex(s, k, c.at(Duration::ZERO)),
            read_at: Duration::ZERO,
            model: CODEX,
            expected: roomy(0.6 / (3.0 / 7.0), Reason::Pace),
        },
        Row {
            name: "a codex poll with an unstarted five-hour window",
            feed: |s, k, c| {
                poll_codex(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &codex_window(0, hours(5.0), hours(5.0)),
                    "null",
                )
            },
            read_at: Duration::ZERO,
            model: CODEX,
            expected: roomy(1.0, Reason::Unstarted),
        },
        Row {
            name: "a codex unstarted window read later still scores 1.0",
            feed: |s, k, c| {
                poll_codex(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &codex_window(0, hours(5.0), hours(5.0)),
                    "null",
                )
            },
            read_at: Duration::from_mins(5),
            model: CODEX,
            expected: roomy(1.0, Reason::Unstarted),
        },
        Row {
            name: "an unstarted five-hour beside a running weekly paces",
            feed: |s, k, c| {
                poll_anthropic(
                    s,
                    k,
                    c.at(Duration::ZERO),
                    &anthropic_body(
                        Some(&window(0, None)),
                        Some(&window(20, Some(days(3.5)))),
                        &[],
                    ),
                );
            },
            read_at: Duration::ZERO,
            model: OPUS,
            expected: roomy(1.0, Reason::Pace),
        },
        Row {
            name: "nothing reported",
            feed: |_, _, _| {},
            read_at: Duration::ZERO,
            model: OPUS,
            expected: unread(Reason::NoReading),
        },
    ]
}

/// A five-hour at 10% (2.5 h left) and a weekly at 20% (3.5 d left), polled
/// at the base.
fn feed_five_hour_and_weekly(store: &QuotaStore, key: &SeatKey, clock: &Clock) {
    poll_anthropic(
        store,
        key,
        clock.at(Duration::ZERO),
        &anthropic_body(
            Some(&window(10, Some(hours(2.5)))),
            Some(&window(20, Some(days(3.5)))),
            &[],
        ),
    );
}

/// The windows of [`feed_five_hour_and_weekly`] plus an opus weekly at 95%
/// with three days left.
fn feed_with_opus_weekly(store: &QuotaStore, key: &SeatKey, clock: &Clock) {
    poll_anthropic(
        store,
        key,
        clock.at(Duration::ZERO),
        &anthropic_body(
            Some(&window(10, Some(hours(2.5)))),
            Some(&window(20, Some(days(3.5)))),
            &[("opus", &window(95, Some(days(3.0))))],
        ),
    );
}

#[test]
fn each_reading_scores_by_its_slowest_relevant_window() {
    for row in scoring_rows() {
        // Arrange
        let clock = Clock::new();
        let store = store();
        (row.feed)(&store, &seat("a"), &clock);

        // Act
        let standing = standing_of(&store, &seat("a"), row.model, &clock.at(row.read_at));

        // Assert
        assert_standing(standing, row.expected, row.name);
    }
}

#[test]
fn a_seat_with_no_meter_entry_or_no_account_is_unread() {
    let clock = Clock::new();
    let now = clock.at(Duration::ZERO);

    assert_standing(
        standing(None, OPUS, &now),
        unread(Reason::NoReading),
        "no account",
    );
}

#[test]
fn an_unknown_scope_is_logged_once_however_often_it_is_read() {
    // Arrange: a tag no other test in this binary uses, so no sibling claims
    // the process-wide line first.
    let clock = Clock::new();
    let store = store();
    poll_anthropic(
        &store,
        &seat("a"),
        clock.at(Duration::ZERO),
        &anthropic_body(
            Some(&window(10, Some(hours(2.5)))),
            None,
            &[("placement_scope_probe", &window(50, Some(days(3.0))))],
        ),
    );
    let now = clock.at(Duration::ZERO);

    // Act
    let events = routectl_testkit::capture_events(|| {
        for _ in 0..3 {
            standing_of(&store, &seat("a"), OPUS, &now);
        }
    });

    // Assert
    let lines = events
        .iter()
        .filter(|event| event.field("event") == Some("placement_scope_ignored"))
        .filter(|event| event.field("scope") == Some("placement_scope_probe"))
        .count();
    assert_eq!(lines, 1);
}

#[test]
fn an_exhausted_weekly_loses_to_any_roomy_seat() {
    // Arrange: seat a has an unstarted five-hour window but a weekly at 95%;
    // seat b is roomy with a poor pace (85% used, 4.9 h of 5 h left).
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    poll_anthropic(
        &store,
        &seat("a"),
        t0,
        &anthropic_body(
            Some(&window(0, None)),
            Some(&window(95, Some(days(3.0)))),
            &[],
        ),
    );
    poll_anthropic(
        &store,
        &seat("b"),
        t0,
        &anthropic_body(Some(&window(85, Some(hours(4.9)))), None, &[]),
    );
    let standings = [
        standing_of(&store, &seat("a"), OPUS, &t0),
        standing_of(&store, &seat("b"), OPUS, &t0),
    ];

    // Act
    let bucket = best_bucket(&standings, &[0, 1]);

    // Assert
    assert_eq!(bucket, Some((Tier::Roomy, vec![1])), "{standings:?}");
}

// ---- best_bucket ----

/// The best tier and the candidates tied in its bucket.
type Bucket = Option<(Tier, Vec<usize>)>;

fn scored(tier: Tier, score: f64) -> Standing {
    Standing {
        tier,
        score: Some(score),
        reason: Reason::Pace,
    }
}

#[test]
fn the_best_tier_wins_whatever_the_scores() {
    let rows: [(&str, Vec<Standing>, Bucket); 4] = [
        (
            "roomy beats a higher-scoring low_room and an unread",
            vec![
                scored(Tier::LowRoom, 9.0),
                unread(Reason::NoReading),
                scored(Tier::Roomy, 0.2),
            ],
            Some((Tier::Roomy, vec![2])),
        ),
        (
            "low_room beats unread",
            vec![unread(Reason::StaleReading), scored(Tier::LowRoom, 0.0)],
            Some((Tier::LowRoom, vec![1])),
        ),
        (
            "every unread seat ties",
            vec![unread(Reason::NoReading), unread(Reason::StaleReading)],
            Some((Tier::Unread, vec![0, 1])),
        ),
        ("no candidate", vec![], None),
    ];
    for (name, standings, expected) in rows {
        let candidates: Vec<usize> = (0..standings.len()).collect();
        assert_eq!(
            best_bucket(&standings, &candidates),
            expected,
            "row: {name}"
        );
    }
}

#[test]
fn only_the_candidates_compete_and_a_missing_standing_reads_unread() {
    let standings = [scored(Tier::Roomy, 5.0), scored(Tier::LowRoom, 0.1)];

    assert_eq!(
        best_bucket(&standings, &[1]),
        Some((Tier::LowRoom, vec![1]))
    );
    assert_eq!(
        best_bucket(&standings, &[1, 2]),
        Some((Tier::LowRoom, vec![1]))
    );
    assert_eq!(best_bucket(&standings, &[2]), Some((Tier::Unread, vec![2])));
}

#[test]
fn seats_in_one_rounded_bucket_tie_and_a_lower_bucket_never_joins() {
    let rows: [(&str, [f64; 2], Vec<usize>); 6] = [
        (
            "0.125 rounds down to 0.12 (exact tie, even)",
            [0.125, 0.12],
            vec![0, 1],
        ),
        (
            "0.375 rounds up to 0.38 (exact tie, even)",
            [0.375, 0.38],
            vec![0, 1],
        ),
        (
            "2.675 is below the tie in binary: 2.67",
            [2.675, 2.67],
            vec![0, 1],
        ),
        ("2.675 stays out of the 2.68 bucket", [2.675, 2.68], vec![1]),
        ("1.234 and 1.2349 share 1.23", [1.234, 1.2349], vec![0, 1]),
        (
            "1.236 rounds into 1.24, above 1.234",
            [1.234, 1.236],
            vec![1],
        ),
    ];
    for (name, scores, expected) in rows {
        let standings = scores.map(|score| scored(Tier::Roomy, score));
        assert_eq!(
            best_bucket(&standings, &[0, 1]),
            Some((Tier::Roomy, expected)),
            "row: {name}"
        );
    }
}

/// Expected values are Python's `round(x, 2)` on the same literals.
#[test]
fn scores_round_half_to_even_as_python_rounds_them() {
    let rows = [
        (0.125, 0.12),
        (0.375, 0.38),
        (0.625, 0.62),
        (0.875, 0.88),
        (2.675, 2.67),
        (1.005, 1.0),
        (1.015, 1.01),
        (1.025, 1.02),
        (0.005, 0.01),
        (0.015, 0.01),
        (0.045, 0.04),
        (1.125, 1.12),
        (0.135, 0.14),
        (12.345, 12.35),
        (0.285, 0.28),
        (1_000_000.0, 1_000_000.0),
        (0.300_000_000_000_000_04, 0.3),
    ];
    for (score, expected) in rows {
        assert_eq!(rounded(score), expected, "round({score}, 2)");
    }
}
