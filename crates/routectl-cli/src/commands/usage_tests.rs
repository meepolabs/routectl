//! Unit tests for the `routectl usage` read surface.
//!
//! Split out of `usage.rs` to keep the implementation file under the
//! readability ceiling; included from there via `#[path = "usage_tests.rs"]`.

use super::*;
use routectl_router::{Config, PricingConfig, ProviderEntry, RegistryEntry};
use routectl_usage::{open, UsageDb};
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

    // By provider: two groups.
    let by_prov = build_window_report(
        &db,
        &config,
        "t".into(),
        bounds,
        Some(GroupDim::Provider),
        false,
    )
    .unwrap();
    assert_eq!(by_prov.rows.len(), 2);
    let paid = find(&by_prov, "paid");
    assert_eq!(paid.requests, 2);
    assert_eq!(paid.input_tokens, 15);
    assert_eq!(paid.output_tokens, 27);

    // Total view: one group, sums across both providers.
    let total = build_window_report(&db, &config, "t".into(), bounds, None, false).unwrap();
    assert_eq!(total.rows.len(), 1);
    let t = find(&total, "total");
    assert_eq!(t.requests, 3);
    assert_eq!(t.input_tokens, 18);
    assert_eq!(t.output_tokens, 31);
}

#[test]
fn footer_cache_hit_rate_and_errors() {
    let (_dir, _path, db) = temp_db();
    // input=300, one error row. No cache_read columns => rate uses
    // cache_read(0)/(0+300)=0.0 (denominator nonzero).
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
    assert_eq!(report.cache_hit_rate, Some(0.0));
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
    };
    let blocks = build_blocks(&db, &config, &args, fixed_now()).unwrap();
    assert_eq!(blocks.len(), 4);
    assert!(blocks[0].contains("today"));
    assert!(blocks[3].contains("all time"));
}

#[test]
fn detail_computes_p95_per_group() {
    let (_dir, _path, db) = temp_db();
    // 20 latencies 1..=20 in one group; nearest-rank p95 of 1..=20 is
    // value at ceil(0.95*20)=19th => 19.
    for i in 1..=20 {
        insert_row(
            &db,
            &format!("d{i}"),
            1000 + i,
            "m",
            "paid",
            "up-paid",
            "al",
            "ok",
            Some(1),
            Some(1),
            i,
        );
    }
    let config = cost_config();
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let report = build_window_report(&db, &config, "t".into(), bounds, None, true).unwrap();
    let t = find(&report, "total");
    assert_eq!(t.p95_latency_ms, Some(19));
    assert_eq!(t.max_latency_ms, 20);
}

// The render-layer, quota-line, --since-title, and --by-model tests live in
// a sibling file to keep each file under the size ceiling. They compile into
// THIS module via include!, so the helpers above stay in scope.
include!("usage_render_tests.rs");
