//! Unit tests for the `routectl usage` read surface.
//!
//! Split out of `usage.rs` to keep the implementation file under the
//! readability ceiling; included from there via `#[path = "usage_tests.rs"]`.

use super::*;
use routectl_router::{Config, PricingConfig, ProviderEntry, RegistryEntry};
use routectl_usage::{UsageDb, open};
use tempfile::TempDir;

fn fixed_now() -> DateTime<Local> {
    // 2026-06-11 (Thursday) 14:30 local.
    Local
        .from_local_datetime(
            &NaiveDate::from_ymd_opt(2026, 6, 11)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
        )
        .earliest()
        .unwrap()
}

fn day_start_ms(y: i32, m: u32, d: u32) -> i64 {
    Local
        .from_local_datetime(
            &NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .earliest()
        .unwrap()
        .timestamp_millis()
}

#[test]
fn today_window_is_local_midnight_to_now() {
    let now = fixed_now();
    let b = window_bounds(WindowFlag::Today, now);
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 11));
    assert_eq!(b.to_ms, now.timestamp_millis() + 1);
}

#[test]
fn this_week_starts_monday() {
    // 2026-06-11 is a Thursday; the ISO week's Monday is 2026-06-08.
    let now = fixed_now();
    let b = window_bounds(WindowFlag::ThisWeek, now);
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 8));
}

#[test]
fn this_month_starts_on_the_first() {
    let now = fixed_now();
    let b = window_bounds(WindowFlag::ThisMonth, now);
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 1));
}

#[test]
fn all_time_starts_at_epoch() {
    let now = fixed_now();
    let b = window_bounds(WindowFlag::All, now);
    assert_eq!(b.from_ms, 0);
    assert_eq!(b.to_ms, now.timestamp_millis() + 1);
}

#[test]
fn since_until_is_midnight_to_end_of_day() {
    let now = fixed_now();
    let b = since_bounds("2026-06-01", Some("2026-06-05"), now).unwrap();
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 1));
    // End of 06-05 is start of 06-06 minus 1ms, made inclusive via +1.
    assert_eq!(b.to_ms, day_start_ms(2026, 6, 6));
}

#[test]
fn since_without_until_runs_to_now() {
    let now = fixed_now();
    let b = since_bounds("2026-06-01", None, now).unwrap();
    assert_eq!(b.from_ms, day_start_ms(2026, 6, 1));
    assert_eq!(b.to_ms, now.timestamp_millis() + 1);
}

#[test]
fn bad_since_date_is_an_error() {
    let now = fixed_now();
    assert!(matches!(
        since_bounds("not-a-date", None, now),
        Err(UsageError::BadDate(_))
    ));
}

#[test]
fn resolve_local_midnight_normal_day_is_single_instant() {
    // A normal (non-DST-transition) local midnight resolves to exactly the
    // expected day-start instant via the LocalResult::Single arm.
    let now = fixed_now();
    let naive = NaiveDate::from_ymd_opt(2026, 6, 11)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let resolved = resolve_local_midnight(naive, now);
    assert_eq!(resolved.timestamp_millis(), day_start_ms(2026, 6, 11));
}

#[test]
fn resolve_local_midnight_gap_probes_forward_not_now() {
    // The spring-forward gap branch cannot be triggered host-TZ-independently
    // here (the host TZ is whatever the test machine uses), so the gap path is
    // covered by inspection: on LocalResult::None the helper warns and probes
    // forward in 15-min steps up to the cap, returning the next VALID instant
    // -- never silently collapsing to `now` unless the probe exhausts. This
    // test pins the contract that a resolvable instant is never equal to a
    // far-future `now`, guarding against a regression to the old
    // `.unwrap_or(now)` collapse for the common (resolvable) case.
    let now = fixed_now();
    let naive = NaiveDate::from_ymd_opt(2026, 3, 8)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let resolved = resolve_local_midnight(naive, now);
    // Whatever the host TZ, a March 8 midnight is resolvable to an instant on
    // or near that calendar day, strictly before `now` (2026-06-11) -- it must
    // not have collapsed to `now`.
    assert!(resolved < now);
}

// --- bucket resolution (pure: no DB, no ambient clock) ---

/// `now` at the given local wall-clock instant.
fn local_now(y: i32, m: u32, d: u32, hour: u32, minute: u32) -> DateTime<Local> {
    Local
        .from_local_datetime(
            &NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(hour, minute, 0)
                .unwrap(),
        )
        .earliest()
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
    let bounds = window_bounds(WindowFlag::Today, now);

    let (anchor, spec) =
        resolve_bucket(BucketUnit::Hour, bounds.from_ms, bounds.to_ms, None, now).unwrap();

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
    let bounds = window_bounds(WindowFlag::ThisWeek, now);

    let (anchor, spec) =
        resolve_bucket(BucketUnit::Day, bounds.from_ms, bounds.to_ms, None, now).unwrap();

    assert_eq!(spec.width_ms, 86_400_000);
    assert_eq!(spec.count, 7, "a spanned week is seven day buckets");
    assert_grid_contract(anchor, spec, bounds.to_ms);
}

#[test]
fn a_full_month_at_day_granularity_is_one_bucket_per_calendar_day() {
    // Late on the last day of a 30-day month.
    let now = local_now(2026, 6, 30, 23, 0);
    let bounds = window_bounds(WindowFlag::ThisMonth, now);

    let (anchor, spec) =
        resolve_bucket(BucketUnit::Day, bounds.from_ms, bounds.to_ms, None, now).unwrap();

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
    let bounds = window_bounds(WindowFlag::All, now);
    let first_row = day_start_ms(2026, 6, 9) + 9 * 3_600_000;

    let (anchor, spec) = resolve_bucket(
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
    let bounds = window_bounds(WindowFlag::All, now);
    let first_row = now.timestamp_millis() - 2_500 * 86_400_000;

    let (anchor, spec) = resolve_bucket(
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
    let bounds = window_bounds(WindowFlag::All, now);
    let first_row = now.timestamp_millis() - 400 * 86_400_000;

    let (anchor, spec) = resolve_bucket(
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
    let bounds = window_bounds(WindowFlag::All, now);

    assert!(resolve_bucket(BucketUnit::Day, bounds.from_ms, bounds.to_ms, None, now).is_none());
    assert!(resolve_bucket(BucketUnit::Hour, bounds.from_ms, bounds.to_ms, None, now).is_none());
}

#[test]
fn a_dated_window_ignores_the_earliest_row() {
    // Only all-time re-anchors; a dated window's lower bound IS its anchor, even
    // when an earlier row exists.
    let now = local_now(2026, 6, 11, 14, 30);
    let bounds = window_bounds(WindowFlag::ThisMonth, now);
    let older = day_start_ms(2020, 1, 1);

    let (anchor, spec) = resolve_bucket(
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
    let point = now.timestamp_millis();

    assert!(resolve_bucket(BucketUnit::Hour, point, point, None, now).is_none());
    assert!(resolve_bucket(BucketUnit::Day, point, point - 1, None, now).is_none());
}

#[test]
fn a_dst_transition_day_still_yields_a_covering_grid() {
    // The host TZ is whatever the test machine uses, so the transition itself is
    // not assertable here; what IS assertable host-independently is that a `now`
    // on either US transition day -- spring-forward, fall-back, and the days
    // around them -- still resolves to a grid the leaf accepts and that covers
    // the whole window at both granularities.
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
                let bounds = window_bounds(flag, now);
                let (anchor, spec) = resolve_bucket(unit, bounds.from_ms, bounds.to_ms, None, now)
                    .unwrap_or_else(|| panic!("{y}-{m}-{d} {unit:?} {flag:?} has a grid"));
                assert_grid_contract(anchor, spec, bounds.to_ms);
            }
            // All-time across the transition, anchored a fortnight before it.
            let bounds = window_bounds(WindowFlag::All, now);
            let first_row = now.timestamp_millis() - 14 * 86_400_000;
            let (anchor, spec) =
                resolve_bucket(unit, bounds.from_ms, bounds.to_ms, Some(first_row), now)
                    .unwrap_or_else(|| panic!("{y}-{m}-{d} {unit:?} all-time has a grid"));
            assert_grid_contract(anchor, spec, bounds.to_ms);
        }
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
/// provider that has a registry price.
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
    config.registry.insert(
        "up-paid".to_string(),
        RegistryEntry {
            pricing: Some(PricingConfig {
                input_per_mtok: Some(3.0),
                output_per_mtok: Some(15.0),
                ..Default::default()
            }),
            provider: None,
        },
    );
    config
}

fn find<'a>(report: &'a WindowReport, label: &str) -> &'a DisplayRow {
    report
        .rows
        .iter()
        .find(|r| r.label == label)
        .expect("display row present")
}

#[test]
fn subscription_group_renders_na_subscription() {
    let (_dir, _path, db) = temp_db();
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
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let report = build_window_report(
        &db,
        &config,
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    let row = find(&report, "sub");
    assert!(row.any_subscription);
    assert_eq!(row.priced_total_usd, None);
    assert_eq!(cost_cell(row), "n/a (subscription)");
}

#[test]
fn api_key_priced_group_shows_dollars() {
    let (_dir, _path, db) = temp_db();
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
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let report = build_window_report(
        &db,
        &config,
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    let row = find(&report, "paid");
    assert_eq!(row.priced_total_usd, Some(18.0));
    assert!(!row.any_subscription);
    assert_eq!(cost_cell(row), "$18.00");
}

#[test]
fn mixed_priced_and_subscription_group_shows_dollars_plus_sub() {
    // One display group (--by alias) spans BOTH a priced API-key provider and
    // a subscription (oauth://) provider. The cost cell must surface both: the
    // priced dollar total AND a `+sub` marker so the subscription portion is
    // not silently hidden behind the dollar figure.
    let (_dir, _path, db) = temp_db();
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
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let report = build_window_report(
        &db,
        &config,
        "t".into(),
        bounds,
        Some(GroupDim::Alias),
        false,
    )
    .unwrap();
    let row = find(&report, "shared");
    assert_eq!(row.priced_total_usd, Some(18.0));
    assert!(row.any_subscription);
    assert_eq!(cost_cell(row), "$18.00+sub");
}

#[test]
fn api_key_without_pricing_shows_na() {
    let (_dir, _path, db) = temp_db();
    // Provider "paid" but an upstream with NO registry entry.
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
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let report = build_window_report(
        &db,
        &config,
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    let row = find(&report, "paid");
    assert_eq!(row.priced_total_usd, None);
    assert!(!row.any_subscription);
    assert_eq!(cost_cell(row), "n/a");
}

#[test]
fn by_provider_rolls_up_and_totals_match() {
    let (_dir, _path, db) = temp_db();
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
    let bounds = window_bounds(WindowFlag::All, fixed_now());

    // By provider: two groups plus the always-appended total row.
    let by_prov = build_window_report(
        &db,
        &config,
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    assert_eq!(by_prov.rows.len(), 3);
    let paid = find(&by_prov, "paid");
    assert_eq!(paid.requests, 2);
    assert_eq!(paid.input_tokens, 15);
    assert_eq!(paid.output_tokens, 27);

    // Default view (by=None => per-model rows): one model "m" row + total.
    let total = build_window_report(&db, &config, "t".into(), bounds, None, false).unwrap();
    assert_eq!(total.rows.len(), 2);
    let t = find(&total, "total");
    assert_eq!(t.requests, 3);
    assert_eq!(t.input_tokens, 18);
    assert_eq!(t.output_tokens, 31);
}

#[test]
fn footer_cache_hit_rate_and_errors() {
    let (_dir, _path, db) = temp_db();
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
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let report = build_window_report(&db, &config, "t".into(), bounds, None, false).unwrap();
    assert_eq!(report.total_errors, 1);
    assert_eq!(report.cache_hit_rate, None);
}

#[test]
fn footer_cache_hit_rate_none_when_no_tokens() {
    let (_dir, _path, db) = temp_db();
    // A row with NULL input and no cache_read => denominator 0 => None.
    insert_row(
        &db, "z", 1000, "m", "paid", "up-paid", "al", "ok", None, None, 5,
    );
    let config = cost_config();
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let report = build_window_report(&db, &config, "t".into(), bounds, None, false).unwrap();
    assert_eq!(report.cache_hit_rate, None);
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
    let (_dir, _path, db) = temp_db();
    insert_row(
        &db,
        "m1",
        fixed_now().timestamp_millis() - 1000,
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
    let blocks = build_blocks(&db, &config, &args, fixed_now()).unwrap();
    assert_eq!(blocks.len(), 4);
    assert!(blocks[0].contains("today"));
    assert!(blocks[3].contains("all time"));
    // The legend is appended to the last block, keeping the block count at 4.
    assert!(blocks[3].contains("legend:"));
}

#[test]
fn detail_computes_ttft_percentiles_per_group() {
    let (_dir, _path, db) = temp_db();
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
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let report = build_window_report(&db, &config, "t".into(), bounds, None, true).unwrap();
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
fn tok_per_s_aggregates_and_handles_zero_window() {
    // Arrange + Act + Assert: 40 tokens over 400ms => 100 tok/s.
    assert_eq!(tok_per_s(40, 400), Some(100));
    // Zero generation window => no rate.
    assert_eq!(tok_per_s(10, 0), None);
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
    let (_dir, _path, db) = temp_db();
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
    let output = build_output(&db, &config, &args, fixed_now()).unwrap();

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
    let (_dir, _path, db) = temp_db();
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
    let output = build_output(&db, &config, &args, fixed_now()).unwrap();

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
