//! Unit tests for the `routectl usage` read surface.
//!
//! Split out of `usage.rs` to keep the implementation file under the
//! readability ceiling; included from there via `#[path = "usage_tests.rs"]`.

use super::*;
use routectl_router::prices::PriceTable;
use routectl_router::{Config, ModelEntry, PricingConfig, ProviderEntry};
use routectl_usage::{CostStatus, UsageDb, open};
use tempfile::TempDir;

/// No refreshed and no shipped price table, so a figure a test asserts can
/// come only from the `[models.X.pricing]` blocks the config builds.
fn no_prices() -> PriceSnapshot {
    PriceSnapshot::default()
}

/// Every model nickname a fixture row in this suite records.
const FIXTURE_MODELS: &[&str] = &[
    "m", "claude", "claude-x", "a", "b", "ga", "gb", "hp", "nocache", "norep", "nulls", "pk",
    "rep", "zero", "m1", "mix-paid",
];

/// Price `upstream` served by `provider` at `pricing` under each nickname
/// in `nicknames`: a `[models.X.pricing]` block applies only to rows whose
/// recorded model names that entry.
fn price_models(
    config: &mut Config,
    nicknames: &[&str],
    provider: &str,
    upstream: &str,
    pricing: &PricingConfig,
) {
    for nickname in nicknames {
        let mut entry = ModelEntry::new(provider, upstream);
        entry.pricing = Some(pricing.clone());
        config.models.insert((*nickname).to_string(), entry);
    }
}

/// The zone every fixture in this suite is read in: US Eastern rules, so a
/// window edge is an exact instant whatever zone the test host runs in.
#[cfg(unix)]
fn zone() -> LocalZone {
    LocalZone::from_posix_tz(&crate::test_tz::eastern()).unwrap()
}

#[cfg(not(unix))]
fn zone() -> LocalZone {
    LocalZone::utc()
}

/// `y-m-d h:min` local in [`zone`].
fn civil(y: i32, m: u8, d: u8, hour: u8, minute: u8) -> PrimitiveDateTime {
    let month = time::Month::try_from(m).unwrap();
    let date = Date::from_calendar_date(y, month, d).unwrap();
    PrimitiveDateTime::new(date, Time::from_hms(hour, minute, 0).unwrap())
}

fn fixed_now() -> OffsetDateTime {
    // 2026-06-11 (Thursday) 14:30 local.
    zone().resolve(civil(2026, 6, 11, 14, 30)).latest().unwrap()
}

fn day_start_ms(y: i32, m: u8, d: u8) -> i64 {
    unix_ms(zone().resolve(civil(y, m, d, 0, 0)).latest().unwrap())
}

#[test]
fn today_window_is_local_midnight_to_now() {
    let now = fixed_now();
    let b = window_bounds(&zone(), WindowFlag::Today, now);
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 11));
    assert_eq!(b.to_ms, unix_ms(now) + 1);
}

#[test]
fn this_week_starts_monday() {
    // 2026-06-11 is a Thursday; the ISO week's Monday is 2026-06-08.
    let now = fixed_now();
    let b = window_bounds(&zone(), WindowFlag::ThisWeek, now);
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 8));
}

#[test]
fn this_month_starts_on_the_first() {
    let now = fixed_now();
    let b = window_bounds(&zone(), WindowFlag::ThisMonth, now);
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 1));
}

#[test]
fn all_time_starts_at_epoch() {
    let now = fixed_now();
    let b = window_bounds(&zone(), WindowFlag::All, now);
    assert_eq!(b.from_ms, 0);
    assert_eq!(b.to_ms, unix_ms(now) + 1);
}

#[test]
fn since_until_is_midnight_to_end_of_day() {
    let now = fixed_now();
    let b = since_bounds(&zone(), "2026-06-01", Some("2026-06-05"), now).unwrap();
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 1));
    // End of 06-05 is start of 06-06 minus 1ms, made inclusive via +1.
    assert_eq!(b.to_ms, day_start_ms(2026, 6, 6));
}

#[test]
fn since_without_until_runs_to_now() {
    let now = fixed_now();
    let b = since_bounds(&zone(), "2026-06-01", None, now).unwrap();
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 1));
    assert_eq!(b.to_ms, unix_ms(now) + 1);
}

#[test]
fn bad_since_date_is_an_error() {
    let now = fixed_now();
    assert!(matches!(
        since_bounds(&zone(), "not-a-date", None, now),
        Err(UsageError::BadDate(_))
    ));
}

#[test]
fn resolve_local_midnight_normal_day_is_single_instant() {
    // A normal (non-DST-transition) local midnight resolves to exactly the
    // expected day-start instant via the single-instant arm.
    let now = fixed_now();
    let resolved = resolve_local_midnight(&zone(), civil(2026, 6, 11, 0, 0), now);
    assert_eq!(unix_ms(resolved), day_start_ms(2026, 6, 11));
}

/// A zone with no midnight on 2026-11-01 (clocks go 23:59:59 -> 01:00) and two
/// on 2026-02-15 (clocks go 00:59:59 -> 00:00).
#[cfg(unix)]
fn midnight_dst_zone() -> LocalZone {
    LocalZone::from_posix_tz(&crate::test_tz::midnight_dst()).unwrap()
}

/// 2026-11-01 15:00 local (UTC-2) in [`midnight_dst_zone`].
const MIDNIGHT_DST_NOW_MS: i64 = 1_793_552_400_000;

#[cfg(unix)]
#[test]
fn resolve_local_midnight_in_a_gap_probes_forward_to_the_first_valid_instant() {
    // 00:00..01:00 does not exist; the 15-minute probe first lands on a real
    // instant at 01:00 local (UTC-2) = 03:00Z.
    let zone = midnight_dst_zone();
    let now = OffsetDateTime::from_unix_timestamp(MIDNIGHT_DST_NOW_MS / 1000).unwrap();
    let resolved = resolve_local_midnight(&zone, civil(2026, 11, 1, 0, 0), now);
    assert_eq!(unix_ms(resolved), 1_793_502_000_000);
    assert_ne!(resolved, now, "the probe must not collapse to now");
}

#[cfg(unix)]
#[test]
fn midnight_gap_debug_event_logs_the_wall_clock_midnight_to_the_second() {
    let zone = midnight_dst_zone();
    let now = OffsetDateTime::from_unix_timestamp(MIDNIGHT_DST_NOW_MS / 1000).unwrap();

    let events = routectl_testkit::capture_events(|| {
        resolve_local_midnight(&zone, civil(2026, 11, 1, 0, 0), now);
    });

    let gap = events
        .iter()
        .find(|event| {
            event
                .message
                .starts_with("local midnight falls in a DST spring-forward gap")
        })
        .expect("the gap DEBUG event fires");
    assert_eq!(gap.level, tracing::Level::DEBUG);
    assert_eq!(gap.field("naive_midnight"), Some("2026-11-01 00:00:00"));
}

#[cfg(unix)]
#[test]
fn resolve_local_midnight_past_the_probe_cap_falls_back_to_now() {
    // A four-hour forward jump at midnight: nothing exists until 04:00, past
    // the 180-minute probe cap.
    let zone = LocalZone::from_posix_tz(&crate::test_tz::posix_tz(
        "<-03>3<+01>-1",
        "11.1.0/0",
        "2.3.0/1",
    ))
    .unwrap();
    let now = OffsetDateTime::from_unix_timestamp(MIDNIGHT_DST_NOW_MS / 1000).unwrap();
    let resolved = resolve_local_midnight(&zone, civil(2026, 11, 1, 0, 0), now);
    assert_eq!(resolved, now);
}

#[cfg(unix)]
#[test]
fn resolve_local_midnight_ambiguous_picks_the_later_instant() {
    // 00:00 local happens twice: first at UTC-2 (02:00Z), then again at UTC-3
    // (03:00Z) after the clocks go back. The window starts at the second.
    let zone = midnight_dst_zone();
    let now = OffsetDateTime::from_unix_timestamp(1_771_167_600).unwrap();
    let resolved = resolve_local_midnight(&zone, civil(2026, 2, 15, 0, 0), now);
    assert_eq!(unix_ms(resolved), 1_771_124_400_000);
}

// --- bucket resolution (pure: no DB, no ambient clock) ---

/// `now` at the given local wall-clock instant in [`zone`].
fn local_now(y: i32, m: u8, d: u8, hour: u8, minute: u8) -> OffsetDateTime {
    zone()
        .resolve(civil(y, m, d, hour, minute))
        .latest()
        .unwrap()
}

/// Assert the grid contract the leaf re-checks at its trust boundary, plus the
/// coverage property widening must never break.
fn assert_grid_contract(anchor_ms: i64, spec: BucketSpec, to_ms: i64) {
    assert!(spec.width_ms > 0, "width must be strictly positive");
    assert!(
        (1..=MAX_BUCKETS).contains(&spec.count),
        "count {} outside 1..={MAX_BUCKETS}",
        spec.count
    );
    let last_start = i128::from(anchor_ms) + (spec.count as i128 - 1) * i128::from(spec.width_ms);
    assert!(
        last_start <= i128::from(i64::MAX),
        "the last bucket start must fit an i64"
    );
    let covered = i128::from(anchor_ms) + spec.count as i128 * i128::from(spec.width_ms);
    assert!(
        covered >= i128::from(to_ms),
        "the grid must cover the whole window"
    );
    assert!(
        anchor_ms <= to_ms,
        "the anchor must precede the upper bound"
    );
}

#[test]
fn today_at_hour_granularity_stays_within_a_day_of_buckets() {
    let now = local_now(2026, 6, 11, 14, 30);
    let bounds = window_bounds(&zone(), WindowFlag::Today, now);

    let (anchor, spec) = resolve_bucket(
        &zone(),
        BucketUnit::Hour,
        bounds.from_ms,
        bounds.to_ms,
        None,
        now,
    )
    .unwrap();

    assert_eq!(anchor, bounds.from_ms, "a dated window anchors on itself");
    assert_eq!(spec.width_ms, 3_600_000, "no widening is needed today");
    // 15 hours elapsed; a DST transition day can shift that by one either way.
    assert!(
        spec.count <= 25,
        "today+hour never exceeds 25: {}",
        spec.count
    );
    assert_grid_contract(anchor, spec, bounds.to_ms);
}

#[test]
fn a_full_week_at_day_granularity_is_seven_buckets() {
    // Sunday 14:30 -- the ISO week's Monday is six days and change back, so the
    // week is fully spanned.
    let now = local_now(2026, 6, 14, 14, 30);
    let bounds = window_bounds(&zone(), WindowFlag::ThisWeek, now);

    let (anchor, spec) = resolve_bucket(
        &zone(),
        BucketUnit::Day,
        bounds.from_ms,
        bounds.to_ms,
        None,
        now,
    )
    .unwrap();

    assert_eq!(spec.width_ms, 86_400_000);
    assert_eq!(spec.count, 7, "a spanned week is seven day buckets");
    assert_grid_contract(anchor, spec, bounds.to_ms);
}

#[test]
fn a_full_month_at_day_granularity_is_one_bucket_per_calendar_day() {
    // Late on the last day of a 30-day month.
    let now = local_now(2026, 6, 30, 23, 0);
    let bounds = window_bounds(&zone(), WindowFlag::ThisMonth, now);

    let (anchor, spec) = resolve_bucket(
        &zone(),
        BucketUnit::Day,
        bounds.from_ms,
        bounds.to_ms,
        None,
        now,
    )
    .unwrap();

    assert_eq!(spec.width_ms, 86_400_000, "a month never needs widening");
    assert!(
        (28..=31).contains(&spec.count),
        "a spanned month is 28-31 day buckets, got {}",
        spec.count
    );
    assert_grid_contract(anchor, spec, bounds.to_ms);
}

#[test]
fn all_time_anchors_on_the_first_rows_local_midnight() {
    // The all-time lower bound is the 1970 epoch; bucketing from there would
    // spend the grid on empty decades, so the earliest row's local midnight is
    // the real anchor.
    let now = local_now(2026, 6, 11, 14, 30);
    let bounds = window_bounds(&zone(), WindowFlag::All, now);
    let first_row = day_start_ms(2026, 6, 9) + 9 * 3_600_000;

    let (anchor, spec) = resolve_bucket(
        &zone(),
        BucketUnit::Day,
        bounds.from_ms,
        bounds.to_ms,
        Some(first_row),
        now,
    )
    .unwrap();

    assert_eq!(
        anchor,
        day_start_ms(2026, 6, 9),
        "anchored at local midnight"
    );
    assert!(
        anchor < first_row,
        "the anchor precedes the row it resolved"
    );
    assert_eq!(spec.width_ms, 86_400_000);
    assert_eq!(spec.count, 3, "Jun 9, 10, and the partial 11th");
    assert_grid_contract(anchor, spec, bounds.to_ms);
}

#[test]
fn all_time_at_day_granularity_past_the_cap_widens_to_whole_days() {
    let now = local_now(2026, 6, 11, 14, 30);
    let bounds = window_bounds(&zone(), WindowFlag::All, now);
    let first_row = unix_ms(now) - 2_500 * 86_400_000;

    let (anchor, spec) = resolve_bucket(
        &zone(),
        BucketUnit::Day,
        bounds.from_ms,
        bounds.to_ms,
        Some(first_row),
        now,
    )
    .unwrap();

    // 2501 day buckets would exceed the cap, so the width becomes a 3-day
    // multiple and the count falls back under it.
    assert_eq!(spec.width_ms, 3 * 86_400_000, "widened to a whole multiple");
    assert!(spec.count <= MAX_BUCKETS);
    assert_grid_contract(anchor, spec, bounds.to_ms);
}

#[test]
fn all_time_at_hour_granularity_widens_to_whole_hours() {
    let now = local_now(2026, 6, 11, 14, 30);
    let bounds = window_bounds(&zone(), WindowFlag::All, now);
    let first_row = unix_ms(now) - 400 * 86_400_000;

    let (anchor, spec) = resolve_bucket(
        &zone(),
        BucketUnit::Hour,
        bounds.from_ms,
        bounds.to_ms,
        Some(first_row),
        now,
    )
    .unwrap();

    assert!(spec.width_ms > 3_600_000, "the hour grid widened");
    assert_eq!(
        spec.width_ms % 3_600_000,
        0,
        "the widened width stays a whole multiple of the requested unit"
    );
    assert!(spec.count <= MAX_BUCKETS);
    assert_grid_contract(anchor, spec, bounds.to_ms);
}

#[test]
fn an_empty_ledger_under_all_time_has_nothing_to_bucket() {
    // No earliest row means no defensible anchor -- the caller reports an empty
    // series rather than a grid over the whole epoch.
    let now = local_now(2026, 6, 11, 14, 30);
    let bounds = window_bounds(&zone(), WindowFlag::All, now);

    assert!(
        resolve_bucket(
            &zone(),
            BucketUnit::Day,
            bounds.from_ms,
            bounds.to_ms,
            None,
            now
        )
        .is_none()
    );
    assert!(
        resolve_bucket(
            &zone(),
            BucketUnit::Hour,
            bounds.from_ms,
            bounds.to_ms,
            None,
            now
        )
        .is_none()
    );
}

#[test]
fn a_dated_window_ignores_the_earliest_row() {
    // Only all-time re-anchors; a dated window's lower bound IS its anchor, even
    // when an earlier row exists.
    let now = local_now(2026, 6, 11, 14, 30);
    let bounds = window_bounds(&zone(), WindowFlag::ThisMonth, now);
    let older = day_start_ms(2020, 1, 1);

    let (anchor, spec) = resolve_bucket(
        &zone(),
        BucketUnit::Day,
        bounds.from_ms,
        bounds.to_ms,
        Some(older),
        now,
    )
    .unwrap();

    assert_eq!(anchor, bounds.from_ms);
    assert_eq!(spec.width_ms, 86_400_000);
}

#[test]
fn a_window_with_no_span_has_nothing_to_bucket() {
    let now = local_now(2026, 6, 11, 14, 30);
    let point = unix_ms(now);

    assert!(resolve_bucket(&zone(), BucketUnit::Hour, point, point, None, now).is_none());
    assert!(resolve_bucket(&zone(), BucketUnit::Day, point, point - 1, None, now).is_none());
}

#[cfg(unix)]
#[test]
fn a_dst_transition_day_has_its_true_length_and_a_covering_grid() {
    // 2026-03-08 runs 23 hours and 2026-11-01 runs 25 in `zone()`. The today
    // window at the last minute of each day spans exactly that, and every
    // window around the transitions still resolves to a covering grid.
    for (y, m, d, hours) in [(2026, 3, 8, 23), (2026, 11, 1, 25), (2026, 3, 9, 24)] {
        let now = local_now(y, m, d, 23, 59);
        let bounds = window_bounds(&zone(), WindowFlag::Today, now);
        assert_eq!(
            unix_ms(now) - bounds.from_ms,
            hours * 3_600_000 - 60_000,
            "{y}-{m}-{d} is {hours} hours long"
        );
        let next_midnight = day_start_ms(y, m, d + 1);
        assert_eq!(next_midnight - bounds.from_ms, hours * 3_600_000);
    }
    for (y, m, d) in [
        (2026, 3, 7),
        (2026, 3, 8),
        (2026, 3, 9),
        (2026, 10, 31),
        (2026, 11, 1),
        (2026, 11, 2),
    ] {
        let now = local_now(y, m, d, 12, 0);
        for unit in [BucketUnit::Hour, BucketUnit::Day] {
            for flag in [
                WindowFlag::Today,
                WindowFlag::ThisWeek,
                WindowFlag::ThisMonth,
            ] {
                let bounds = window_bounds(&zone(), flag, now);
                let (anchor, spec) =
                    resolve_bucket(&zone(), unit, bounds.from_ms, bounds.to_ms, None, now)
                        .unwrap_or_else(|| panic!("{y}-{m}-{d} {unit:?} {flag:?} has a grid"));
                assert_grid_contract(anchor, spec, bounds.to_ms);
            }
            let bounds = window_bounds(&zone(), WindowFlag::All, now);
            let first_row = unix_ms(now) - 14 * 86_400_000;
            let (anchor, spec) = resolve_bucket(
                &zone(),
                unit,
                bounds.from_ms,
                bounds.to_ms,
                Some(first_row),
                now,
            )
            .unwrap_or_else(|| panic!("{y}-{m}-{d} {unit:?} all-time has a grid"));
            assert_grid_contract(anchor, spec, bounds.to_ms);
        }
    }
}

#[cfg(unix)]
#[test]
fn week_and_month_starts_across_a_transition_are_exact() {
    // Friday 2026-03-13 and Friday 2026-11-06, each in the week after a
    // transition whose Monday and month start are on the other side of it.
    let cases = [
        // (now, week start, month start)
        (
            local_now(2026, 3, 13, 12, 0),
            1_773_028_800_000,
            1_772_341_200_000,
        ),
        (
            local_now(2026, 11, 6, 12, 0),
            1_793_595_600_000,
            1_793_505_600_000,
        ),
    ];
    for (now, week, month) in cases {
        assert_eq!(
            window_bounds(&zone(), WindowFlag::ThisWeek, now).from_ms,
            week
        );
        assert_eq!(
            window_bounds(&zone(), WindowFlag::ThisMonth, now).from_ms,
            month
        );
    }
}

// --- window edges, pinned per zone ---

// `test_tz::eastern()` has a 23-hour day on 2026-03-08 and a 25-hour day on
// 2026-11-01; `test_tz::midnight_dst()` has no midnight on 2026-11-01 and two
// on 2026-02-15.

/// `(name, now ms, today start, week start, month start)`. Rows were recorded
/// from the window functions this module used before, under the zone
/// the table is named for.
type WindowRow = (&'static str, i64, i64, i64, i64);

const EASTERN_WINDOWS: &[WindowRow] = &[
    (
        "23-hour day, noon",
        1_772_985_600_000,
        1_772_946_000_000,
        1_772_427_600_000,
        1_772_341_200_000,
    ),
    (
        "25-hour day, noon",
        1_793_552_400_000,
        1_793_505_600_000,
        1_792_987_200_000,
        1_793_505_600_000,
    ),
    (
        "25-hour day, morning",
        1_793_541_600_000,
        1_793_505_600_000,
        1_792_987_200_000,
        1_793_505_600_000,
    ),
    (
        "week and month before spring-forward",
        1_774_022_400_000,
        1_773_979_200_000,
        1_773_633_600_000,
        1_772_341_200_000,
    ),
    (
        "week and month before fall-back",
        1_795_194_000_000,
        1_795_150_800_000,
        1_794_805_200_000,
        1_793_505_600_000,
    ),
    (
        "winter",
        1_771_167_600_000,
        1_771_131_600_000,
        1_770_613_200_000,
        1_769_922_000_000,
    ),
    (
        "monday after spring-forward",
        1_773_057_600_000,
        1_773_028_800_000,
        1_773_028_800_000,
        1_772_341_200_000,
    ),
    (
        "monday after fall-back",
        1_793_624_400_000,
        1_793_595_600_000,
        1_793_595_600_000,
        1_793_505_600_000,
    ),
];

const MIDNIGHT_DST_WINDOWS: &[WindowRow] = &[
    (
        "no midnight: gap probe finds 01:00",
        1_793_552_400_000,
        1_793_502_000_000,
        1_792_983_600_000,
        1_793_502_000_000,
    ),
    (
        "no midnight, morning",
        1_793_541_600_000,
        1_793_502_000_000,
        1_792_983_600_000,
        1_793_502_000_000,
    ),
    (
        "two midnights: the earliest",
        1_771_167_600_000,
        1_771_124_400_000,
        1_770_602_400_000,
        1_769_911_200_000,
    ),
    (
        "summer",
        1_772_985_600_000,
        1_772_938_800_000,
        1_772_420_400_000,
        1_772_334_000_000,
    ),
    (
        "standard time",
        1_774_022_400_000,
        1_773_975_600_000,
        1_773_630_000_000,
        1_772_334_000_000,
    ),
    (
        "month started in the gap",
        1_795_194_000_000,
        1_795_140_000_000,
        1_794_794_400_000,
        1_793_502_000_000,
    ),
    (
        "day after the gap",
        1_793_624_400_000,
        1_793_584_800_000,
        1_793_584_800_000,
        1_793_502_000_000,
    ),
];

/// `(name, date, local midnight start, last-millisecond end)` for
/// `--since D --until D`; `None` start is the error the previous
/// resolver returned for a midnight that does not exist.
type SinceRow = (&'static str, &'static str, Option<i64>, i64);

const EASTERN_SINCE: &[SinceRow] = &[
    (
        "25-hour day",
        "2026-11-01",
        Some(1_793_505_600_000),
        1_793_595_599_999,
    ),
    (
        "23-hour day",
        "2026-03-08",
        Some(1_772_946_000_000),
        1_773_028_799_999,
    ),
    (
        "winter",
        "2026-02-15",
        Some(1_771_131_600_000),
        1_771_217_999_999,
    ),
];

const MIDNIGHT_DST_SINCE: &[SinceRow] = &[
    ("midnight in the gap", "2026-11-01", None, 1_793_584_799_999),
    (
        "two midnights",
        "2026-02-15",
        Some(1_771_124_400_000),
        1_771_210_799_999,
    ),
    (
        "day before the gap",
        "2026-10-31",
        Some(1_793_415_600_000),
        1_793_501_999_999,
    ),
];

/// Every window edge `window_bounds`, `since_bounds` and the all-time bucket
/// anchor produce for the rows of one zone.
fn assert_window_tables(zone: &LocalZone, windows: &[WindowRow], since: &[SinceRow]) {
    for (name, now_ms, today, week, month) in windows {
        let now = OffsetDateTime::from_unix_timestamp(now_ms / 1000).unwrap();
        let bounds = |flag| window_bounds(zone, flag, now);
        assert_eq!(bounds(WindowFlag::Today).from_ms, *today, "{name}: today");
        assert_eq!(bounds(WindowFlag::ThisWeek).from_ms, *week, "{name}: week");
        assert_eq!(
            bounds(WindowFlag::ThisMonth).from_ms,
            *month,
            "{name}: month"
        );
        for flag in [
            WindowFlag::Today,
            WindowFlag::ThisWeek,
            WindowFlag::ThisMonth,
            WindowFlag::All,
        ] {
            assert_eq!(
                bounds(flag).to_ms,
                now_ms + 1,
                "{name}: {flag:?} upper bound"
            );
        }
        assert_eq!(bounds(WindowFlag::All).from_ms, 0, "{name}: all-time");
        let (anchor, _) =
            resolve_bucket(zone, BucketUnit::Hour, 0, now_ms + 1, Some(*now_ms), now).unwrap();
        assert_eq!(
            anchor, *today,
            "{name}: all-time anchor on the row's local midnight"
        );
    }
    for (name, date, start, end) in since {
        let now = OffsetDateTime::from_unix_timestamp(windows[0].1 / 1000).unwrap();
        match since_bounds(zone, date, Some(date), now) {
            Ok(bounds) => {
                assert_eq!(Some(bounds.from_ms), *start, "{name}: since start");
                assert_eq!(bounds.to_ms, end + 1, "{name}: until end");
            }
            Err(err) => {
                assert!(start.is_none(), "{name}: unexpected {err:?}");
                assert!(
                    matches!(err, UsageError::AmbiguousTime(_)),
                    "{name}: {err:?}"
                );
            }
        }
    }
}

#[test]
fn window_edges_are_pinned_per_zone() {
    let test = "window_edges_are_pinned_per_zone";
    crate::test_tz::in_zone(&crate::test_tz::eastern(), module_path!(), test, || {
        assert_window_tables(&LocalZone::load(), EASTERN_WINDOWS, EASTERN_SINCE);
    });
    crate::test_tz::in_zone(
        &crate::test_tz::midnight_dst(),
        module_path!(),
        test,
        || {
            assert_window_tables(&LocalZone::load(), MIDNIGHT_DST_WINDOWS, MIDNIGHT_DST_SINCE);
        },
    );
}

/// The `--since` / `--until` date grammar. Each accepted spelling must name
/// the same day as the canonical one; rows were recorded from the
/// `%Y-%m-%d` reader this module used before.
#[test]
fn since_date_spellings_are_pinned() {
    let now = fixed_now();
    let canonical = since_bounds(&zone(), "2026-06-01", None, now)
        .unwrap()
        .from_ms;
    let accepted = [
        ("unpadded month and day", "2026-6-1"),
        ("unpadded day", "2026-06-1"),
        ("leading space", " 2026-06-01"),
        ("space before month", "2026- 6-01"),
        ("explicit plus sign", "+2026-06-01"),
    ];
    for (name, raw) in accepted {
        assert_eq!(
            since_bounds(&zone(), raw, None, now).unwrap().from_ms,
            canonical,
            "{name}"
        );
    }
    let two_digit_year = since_bounds(&zone(), "26-6-1", None, now).unwrap().from_ms;
    assert!(two_digit_year < 0, "a short year is year 26, not 2026");
    let rejected = [
        ("trailing space", "2026-06-01 "),
        ("five-digit year", "02026-06-01"),
        ("five-digit unsigned year", "10000-01-01"),
        ("three-digit day", "2026-06-001"),
        ("month 13", "2026-13-01"),
        ("not a calendar day", "2026-02-30"),
        ("slashes", "2026/06/01"),
        ("no day", "2026-06"),
        ("trailing junk", "2026-06-01x"),
        ("signed month", "2026-+6-01"),
        ("empty", ""),
    ];
    for (name, raw) in rejected {
        assert!(
            matches!(
                since_bounds(&zone(), raw, None, now),
                Err(UsageError::BadDate(_))
            ),
            "{name}: {raw:?} must be refused"
        );
    }
}

// --- DB-backed rollup + cost tests ---

#[allow(clippy::too_many_arguments)]
fn insert_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    model: &str,
    provider: &str,
    upstream: &str,
    alias: &str,
    outcome: &str,
    input: Option<i64>,
    output: Option<i64>,
    latency_ms: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, ?4, ?5, ?6, 0, ?7, \
             ?8, 0, 0, 1, 0, ?9, ?10)",
            rusqlite::params![
                ts_start, request_id, alias, model, provider, upstream, outcome, latency_ms, input,
                output,
            ],
        )
        .expect("insert row");
}

/// Insert a streaming, successful row with explicit `ttfb_ms`,
/// `reasoning_tokens`, and cache columns so the detail / ttft / presence
/// paths are testable. `reasoning` and `cache_read` are `Option` to exercise
/// NULL-vs-reported-0.
#[allow(clippy::too_many_arguments)]
fn insert_stream_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    model: &str,
    ttfb_ms: i64,
    latency_ms: i64,
    output: Option<i64>,
    reasoning: Option<i64>,
    cache_read: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, \
             fallback_count, output_tokens, reasoning_tokens, cache_read) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', ?3, 'paid', 'up-paid', \
             1, 'ok', ?4, ?5, 0, 0, 1, 0, ?6, ?7, ?8)",
            rusqlite::params![
                ts_start, request_id, model, latency_ms, ttfb_ms, output, reasoning, cache_read,
            ],
        )
        .expect("insert stream row");
}

fn temp_db() -> (TempDir, PathBuf, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = open(&path).expect("open");
    (dir, path, db)
}

/// Config with one subscription provider (oauth://) and one API-key
/// provider whose `up-paid` models carry a price.
fn cost_config() -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "sub".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    config.providers.insert(
        "paid".to_string(),
        ProviderEntry::anthropic_api("env://PAID_KEY"),
    );
    price_models(
        &mut config,
        FIXTURE_MODELS,
        "paid",
        "up-paid",
        &PricingConfig {
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(15.0),
            ..Default::default()
        },
    );
    config
}

fn find<'a>(report: &'a WindowReport, label: &str) -> &'a DisplayRow {
    report
        .rows()
        .find(|r| r.label == label)
        .expect("display row present")
}

#[test]
fn subscription_group_renders_na_subscription() {
    let (_dir, _path, mut db) = temp_db();
    insert_row(
        &db,
        "s1",
        1000,
        "claude",
        "sub",
        "up-sub",
        "al",
        "ok",
        Some(100),
        Some(50),
        10,
    );
    let config = cost_config();
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());
    let report = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    let row = find(&report, "sub");
    assert!(row.any_subscription);
    assert_eq!(row.metrics.cost_usd, None);
    assert_eq!(cost_cell(row), "n/a (subscription)");
}

#[test]
fn api_key_priced_group_shows_dollars() {
    let (_dir, _path, mut db) = temp_db();
    // 1_000_000 input @ $3/Mtok = $3.00; 1_000_000 output @ $15 = $15.00.
    insert_row(
        &db,
        "p1",
        1000,
        "claude",
        "paid",
        "up-paid",
        "al",
        "ok",
        Some(1_000_000),
        Some(1_000_000),
        10,
    );
    let config = cost_config();
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());
    let report = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    let row = find(&report, "paid");
    assert_eq!(row.metrics.cost_usd, Some(18.0));
    assert!(!row.any_subscription);
    assert_eq!(cost_cell(row), "$18.00");
}

#[test]
fn mixed_priced_and_subscription_group_shows_dollars_plus_sub() {
    // One display group (--by alias) spans BOTH a priced API-key provider and
    // a subscription (oauth://) provider. The cost cell must surface both: the
    // priced dollar total AND a `+sub` marker so the subscription portion is
    // not silently hidden behind the dollar figure.
    let (_dir, _path, mut db) = temp_db();
    // Priced fine-row: 1_000_000 in @ $3 + 1_000_000 out @ $15 = $18.00.
    insert_row(
        &db,
        "mix-paid",
        1000,
        "claude",
        "paid",
        "up-paid",
        "shared",
        "ok",
        Some(1_000_000),
        Some(1_000_000),
        10,
    );
    // Subscription fine-row under the SAME alias group.
    insert_row(
        &db,
        "mix-sub",
        1100,
        "claude",
        "sub",
        "up-sub",
        "shared",
        "ok",
        Some(500),
        Some(500),
        10,
    );
    let config = cost_config();
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());
    let report = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        Some(GroupDim::Alias),
        false,
    )
    .unwrap();
    let row = find(&report, "shared");
    assert_eq!(row.metrics.cost_usd, Some(18.0));
    assert!(row.any_subscription);
    assert_eq!(cost_cell(row), "$18.00+sub");
}

#[test]
fn api_key_without_pricing_shows_na() {
    let (_dir, _path, mut db) = temp_db();
    // Provider "paid" but an upstream no model entry prices.
    insert_row(
        &db,
        "u1",
        1000,
        "claude",
        "paid",
        "up-unpriced",
        "al",
        "ok",
        Some(1_000),
        Some(1_000),
        10,
    );
    let config = cost_config();
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());
    let report = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    let row = find(&report, "paid");
    assert_eq!(row.metrics.cost_usd, None);
    assert!(!row.any_subscription);
    assert_eq!(cost_cell(row), "n/a");
}

#[test]
fn by_provider_rolls_up_and_totals_match() {
    let (_dir, _path, mut db) = temp_db();
    insert_row(
        &db,
        "a",
        1000,
        "m",
        "paid",
        "up-paid",
        "al",
        "ok",
        Some(10),
        Some(20),
        5,
    );
    insert_row(
        &db,
        "b",
        1100,
        "m",
        "paid",
        "up-paid",
        "al",
        "ok",
        Some(5),
        Some(7),
        15,
    );
    insert_row(
        &db,
        "c",
        1200,
        "m",
        "sub",
        "up-sub",
        "al",
        "ok",
        Some(3),
        Some(4),
        25,
    );
    let config = cost_config();
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());

    // By provider: two groups plus the always-appended total row.
    let by_prov = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    assert_eq!(by_prov.rows().count(), 3);
    let paid = find(&by_prov, "paid");
    assert_eq!(paid.metrics.requests, 2);
    assert_eq!(paid.metrics.input_tokens, 15);
    assert_eq!(paid.metrics.output_tokens, 27);

    // Default view (by=None => per-model rows): one model "m" row + total.
    let total = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        None,
        false,
    )
    .unwrap();
    assert_eq!(total.rows().count(), 2);
    let t = find(&total, "total");
    assert_eq!(t.metrics.requests, 3);
    assert_eq!(t.metrics.input_tokens, 18);
    assert_eq!(t.metrics.output_tokens, 31);
}

#[test]
fn footer_cache_hit_rate_and_errors() {
    let (_dir, _path, mut db) = temp_db();
    // input=300, one error row, neither reporting cache_read (NULL). The footer
    // rate is presence-gated: with cache_read_present == 0 on every row, no row
    // qualifies, so the rate is None ("not reported"), NOT 0.0. The error count
    // stays a cross-row sum (1).
    insert_row(
        &db,
        "ok1",
        1000,
        "m",
        "paid",
        "up-paid",
        "al",
        "ok",
        Some(300),
        Some(10),
        5,
    );
    insert_row(
        &db,
        "err1",
        1100,
        "m",
        "paid",
        "up-paid",
        "al",
        "upstream_error",
        None,
        None,
        5,
    );
    let config = cost_config();
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());
    let report = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        None,
        false,
    )
    .unwrap();
    assert_eq!(report.total.metrics.errors, 1);
    assert_eq!(report.total.metrics.cache_hit_pct, None);
}

#[test]
fn footer_cache_hit_rate_none_when_no_tokens() {
    let (_dir, _path, mut db) = temp_db();
    // A row with NULL input and no cache_read => denominator 0 => None.
    insert_row(
        &db, "z", 1000, "m", "paid", "up-paid", "al", "ok", None, None, 5,
    );
    let config = cost_config();
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());
    let report = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        None,
        false,
    )
    .unwrap();
    assert_eq!(report.total.metrics.cache_hit_pct, None);
}

#[test]
fn no_data_path_is_friendly_not_error() {
    // A nonexistent db path: open_readonly returns NoData; the command
    // surface must treat it as a clean exit, not an error.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("absent.db");
    let result = open_readonly(&path);
    assert!(matches!(result, Err(OpenError::NoData { .. })));
    // The `run` contract turns that into println + Ok(()); we assert the
    // classification here since `run` writes to stdout.
}

#[test]
fn multi_window_summary_emits_four_blocks() {
    let (_dir, _path, mut db) = temp_db();
    insert_row(
        &db,
        "m1",
        unix_ms(fixed_now()) - 1000,
        "m",
        "paid",
        "up-paid",
        "al",
        "ok",
        Some(10),
        Some(10),
        5,
    );
    let config = cost_config();
    let args = UsageArgs {
        window: WindowFlag::None,
        since: None,
        until: None,
        by: None,
        detail: false,
        db: None,
        k_calibration: false,
    };
    let blocks = build_blocks(&mut db, &config, &no_prices(), &args, &zone(), fixed_now()).unwrap();
    assert_eq!(blocks.len(), 4);
    assert!(blocks[0].contains("today"));
    assert!(blocks[3].contains("all time"));
    // The legend is appended to the last block, keeping the block count at 4.
    assert!(blocks[3].contains("legend:"));
}

#[test]
fn detail_computes_ttft_percentiles_per_group() {
    let (_dir, _path, mut db) = temp_db();
    // 20 streaming ttfb values 1..=20 in one model group. Nearest-rank p95 is
    // value at ceil(0.95*20)=19th => 19; p50 at ceil(0.50*20)=10th => 10.
    for i in 1..=20 {
        insert_stream_row(
            &db,
            &format!("d{i}"),
            1000 + i,
            "m",
            i,
            i + 100,
            Some(1),
            None,
            None,
        );
    }
    let config = cost_config();
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());
    let report = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        bounds,
        None,
        true,
    )
    .unwrap();
    let m = find(&report, "m");
    assert_eq!(m.ttft_p50_ms, Some(10));
    assert_eq!(m.ttft_p95_ms, Some(19));
    // The total row covers the same samples.
    let t = find(&report, "total");
    assert_eq!(t.ttft_p95_ms, Some(19));
}

// --- humanizing formatters ---

#[test]
fn human_count_renders_compact_suffixes() {
    // Arrange + Act + Assert
    assert_eq!(human_count(9999), "9999");
    assert_eq!(human_count(10_000), "10K");
    assert_eq!(human_count(38_349), "38.3K");
    assert_eq!(human_count(4_637_884), "4.6M");
    assert_eq!(human_count(5_000_000), "5M");
    assert_eq!(human_count(1_500_000_000), "1.5B");
}

#[test]
fn human_ms_renders_scaled_durations() {
    // Arrange + Act + Assert
    assert_eq!(human_ms(999), "999ms");
    assert_eq!(human_ms(1000), "1.0s");
    assert_eq!(human_ms(6512), "6.5s");
    // The seconds path never rounds up to 60.0s: just below the round-up
    // floor stays in seconds, at/above it promotes to the minute path.
    assert_eq!(human_ms(59_949), "59.9s");
    assert_eq!(human_ms(59_950), "1m00s");
    assert_eq!(human_ms(60_000), "1m00s");
    assert_eq!(human_ms(90_701), "1m30s");
    assert_eq!(human_ms(273_034), "4m33s");
    assert_eq!(human_ms(3_661_000), "1h01m");
}

#[test]
fn tok_per_s_cell_rounds_the_query_rate_and_marks_an_absent_one() {
    let rate = |throughput_tok_s| QueryMetrics {
        throughput_tok_s,
        ..QueryMetrics::default()
    };
    assert_eq!(tok_per_s_cell(&rate(Some(99.6))), "100");
    assert_eq!(tok_per_s_cell(&rate(None)), "-");
}

#[test]
fn nearest_rank_matches_known_samples_and_handles_empty() {
    // Arrange: sorted 1..=20.
    let sorted: Vec<i64> = (1..=20).collect();

    // Act + Assert
    assert_eq!(nearest_rank(&sorted, 0.50), 10);
    assert_eq!(nearest_rank(&sorted, 0.95), 19);
    // A single-sample group yields that sample at any quantile.
    assert_eq!(nearest_rank(&[42], 0.95), 42);
}

#[test]
fn ttft_cell_renders_dash_for_empty_group() {
    // Arrange + Act + Assert
    assert_eq!(ttft_cell(None), "-");
    assert_eq!(ttft_cell(Some(6512)), "6.5s");
}

#[test]
fn build_output_k_calibration_returns_calibration_report_not_window_blocks() {
    // Arrange: a plain usage row; no calibrated rows, so the no-data message
    // is the calibration path output. This is distinct from any window-block title.
    let (_dir, _path, mut db) = temp_db();
    // A plain usage row to ensure window blocks would be non-empty if produced.
    insert_row(
        &db,
        "r1",
        1000,
        "m",
        "paid",
        "up-paid",
        "al",
        "ok",
        Some(10),
        Some(10),
        5,
    );
    let config = cost_config();
    let args = UsageArgs {
        window: WindowFlag::None,
        since: None,
        until: None,
        by: None,
        detail: false,
        db: None,
        k_calibration: true,
    };

    // Act
    let output = build_output(&mut db, &config, &no_prices(), &args, &zone(), fixed_now()).unwrap();

    // Assert: calibration-path output present (no-data message), not window blocks.
    assert!(
        output.contains("no calibrated predictions"),
        "k_calibration=true must return the calibration path output: {output}"
    );
    assert!(
        !output.contains("== today =="),
        "k_calibration=true must not contain a window-block title: {output}"
    );
}

#[test]
fn build_output_normal_returns_window_blocks_not_calibration_report() {
    // Arrange: empty DB, no calibrated rows, k_calibration=false.
    let (_dir, _path, mut db) = temp_db();
    insert_row(
        &db,
        "r1",
        1000,
        "m",
        "paid",
        "up-paid",
        "al",
        "ok",
        Some(10),
        Some(10),
        5,
    );
    let config = cost_config();
    let args = UsageArgs {
        window: WindowFlag::None,
        since: None,
        until: None,
        by: None,
        detail: false,
        db: None,
        k_calibration: false,
    };

    // Act
    let output = build_output(&mut db, &config, &no_prices(), &args, &zone(), fixed_now()).unwrap();

    // Assert: multi-window output present, no calibration header.
    assert!(
        output.contains("== today =="),
        "k_calibration=false must return window blocks: {output}"
    );
    assert!(
        !output.contains("k-calibration"),
        "k_calibration=false must not contain the calibration report: {output}"
    );
}

// The render-layer, quota-line, --since-title, and --by-model tests live in
// a sibling file to keep each file under the size ceiling. They compile into
// THIS module via include!, so the helpers above stay in scope.
include!("usage_render_tests.rs");
include!("usage_reduction_render_tests.rs");
include!("usage_suppressed_render_tests.rs");
include!("usage_era_pricing_tests.rs");
include!("usage_price_table_tests.rs");
include!("usage_equivalent_cost_tests.rs");
include!("usage_lifetime_cost_tests.rs");
include!("usage_trailing_window_tests.rs");
include!("usage_query_parity_tests.rs");
