//! `routectl usage` and `/status/query` report the same groups and figures for
//! every window and every grouping.
//!
//! One ledger, one zone and one pinned instant feed both: the CLI through its
//! own window flags (parsed by clap) and its report builder, the endpoint
//! through its request parser and panel builder. Both price through the same
//! config and the same price-table snapshot. The ledger holds a dated copy of
//! every row shape at six instants, one inside each window boundary, so every
//! window selects a different row set and a flag mapped to the wrong window
//! changes the figures.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use clap::Parser;
use routectl_router::prices::PriceBook;
use routectl_router::{Auth, Config, ModelEntry, PricingConfig, ProviderEntry, Router};
use routectl_usage::{GroupDim, QueryMetrics, QueryResult, UsageDb, open};
use tempfile::TempDir;
use time::{Date, Month, PrimitiveDateTime, Time};

use super::*;
use crate::commands::pricing::PriceSnapshot;
use crate::commands::usage::{
    UsageArgs, UsageWindowArgs, WindowReport, build_window_reports, render_report,
};
use crate::handlers::status::DaemonMeta;
use crate::local_time::unix_ms;
use crate::server::AppState;

/// Each CLI window flag beside the endpoint token naming the same window.
const WINDOWS: [(&str, &str); 6] = [
    ("--today", "today"),
    ("--this-week", "week"),
    ("--this-month", "month"),
    ("--last-24h", "last24h"),
    ("--last-7d", "last7d"),
    ("--all", "all"),
];

/// The CLI's window flags alone, parsed exactly as `routectl usage` parses
/// them.
#[derive(Parser)]
struct WindowFlags {
    #[command(flatten)]
    window: UsageWindowArgs,
}

/// US Eastern, so a calendar window edge is an exact instant whatever zone
/// the test host runs in.
#[cfg(unix)]
fn zone() -> LocalZone {
    LocalZone::from_posix_tz(&crate::test_tz::eastern()).unwrap()
}

#[cfg(not(unix))]
fn zone() -> LocalZone {
    LocalZone::utc()
}

/// `y-m-d h:00` local in [`zone`], as epoch-ms.
fn local_ms(y: i32, m: u8, d: u8, hour: u8) -> i64 {
    let date = Date::from_calendar_date(y, Month::try_from(m).unwrap(), d).unwrap();
    let civil = PrimitiveDateTime::new(date, Time::from_hms(hour, 0, 0).unwrap());
    unix_ms(zone().resolve(civil).latest().unwrap())
}

/// The pinned instant: Thursday 2026-06-11 14:30 local.
fn pinned_now() -> OffsetDateTime {
    let date = Date::from_calendar_date(2026, Month::June, 11).unwrap();
    let civil = PrimitiveDateTime::new(date, Time::from_hms(14, 30, 0).unwrap());
    zone().resolve(civil).latest().unwrap()
}

/// One instant inside each window boundary, relative to [`pinned_now`]:
///
/// | instant           | today | 24h | week | 7d | month | all |
/// |-------------------|-------|-----|------|----|-------|-----|
/// | Thu 11th 13:00    |   x   |  x  |  x   | x  |   x   |  x  |
/// | Wed 10th 22:00    |       |  x  |  x   | x  |   x   |  x  |
/// | Mon 8th 10:00     |       |     |  x   | x  |   x   |  x  |
/// | Fri 5th 12:00     |       |     |      | x  |   x   |  x  |
/// | Tue 2nd 12:00     |       |     |      |    |   x   |  x  |
/// | May 1st 12:00     |       |     |      |    |       |  x  |
///
/// No two windows hold the same set.
fn instants() -> [i64; 6] {
    [
        local_ms(2026, 6, 11, 13),
        local_ms(2026, 6, 10, 22),
        local_ms(2026, 6, 8, 10),
        local_ms(2026, 6, 5, 12),
        local_ms(2026, 6, 2, 12),
        local_ms(2026, 5, 1, 12),
    ]
}

/// One ledger row shape. `None` columns are NULL.
struct Shape {
    model: &'static str,
    alias: &'static str,
    provider: &'static str,
    upstream: &'static str,
    seat: Option<&'static str>,
    stream: bool,
    outcome: &'static str,
    error_class: Option<&'static str>,
    latency_ms: i64,
    ttfb_ms: Option<i64>,
    input: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    lifetimes: Option<&'static str>,
}

/// Every row shape the ledger repeats at each instant: two models on a priced
/// API-key provider, one on a subscription seat pool, one on a provider no
/// source prices; two named seats and a NULL seat; a NULL-seat row on the
/// priced API-key entry (its own account) and one on a forwarded entry (no
/// account); a classified and an unclassified error; and a 30-minute cache
/// write the priced model has no rate for.
const SHAPES: [Shape; 8] = [
    Shape {
        model: "opus",
        alias: "fast",
        provider: "paid",
        upstream: "up-opus",
        seat: Some("work"),
        stream: true,
        outcome: "ok",
        error_class: None,
        latency_ms: 1_200,
        ttfb_ms: Some(120),
        input: Some(1_000),
        output: Some(400),
        cache_read: Some(20_000),
        cache_write: Some(500),
        lifetimes: Some(r#"{"300":500}"#),
    },
    Shape {
        model: "opus",
        alias: "fast",
        provider: "paid",
        upstream: "up-opus",
        seat: Some("home"),
        stream: false,
        outcome: "upstream_error",
        error_class: Some("http-5xx"),
        latency_ms: 400,
        ttfb_ms: None,
        input: None,
        output: None,
        cache_read: None,
        cache_write: None,
        lifetimes: None,
    },
    Shape {
        model: "opus",
        alias: "slow",
        provider: "paid",
        upstream: "up-opus",
        seat: Some("work"),
        stream: true,
        outcome: "ok",
        error_class: None,
        latency_ms: 2_500,
        ttfb_ms: Some(300),
        input: Some(200),
        output: Some(100),
        cache_read: Some(0),
        cache_write: Some(1_000),
        lifetimes: Some(r#"{"1800":1000}"#),
    },
    Shape {
        model: "sonnet",
        alias: "slow",
        provider: "sub",
        upstream: "up-sonnet",
        seat: None,
        stream: true,
        outcome: "ok",
        error_class: None,
        latency_ms: 3_000,
        ttfb_ms: Some(500),
        input: Some(3_000),
        output: Some(900),
        cache_read: Some(60_000),
        cache_write: None,
        lifetimes: None,
    },
    Shape {
        model: "sonnet",
        alias: "fast",
        provider: "sub",
        upstream: "up-sonnet",
        seat: None,
        stream: false,
        outcome: "timeout",
        error_class: None,
        latency_ms: 30_000,
        ttfb_ms: None,
        input: None,
        output: None,
        cache_read: None,
        cache_write: None,
        lifetimes: None,
    },
    Shape {
        model: "opus",
        alias: "fast",
        provider: "paid",
        upstream: "up-opus",
        seat: None,
        stream: true,
        outcome: "ok",
        error_class: None,
        latency_ms: 1_500,
        ttfb_ms: Some(700),
        input: Some(400),
        output: Some(40),
        cache_read: Some(0),
        cache_write: None,
        lifetimes: None,
    },
    Shape {
        model: "relayed",
        alias: "cheap",
        provider: "relay",
        upstream: "up-relayed",
        seat: None,
        stream: true,
        outcome: "ok",
        error_class: None,
        latency_ms: 6_000,
        ttfb_ms: Some(900),
        input: Some(70),
        output: Some(7),
        cache_read: None,
        cache_write: None,
        lifetimes: None,
    },
    Shape {
        model: "haiku",
        alias: "cheap",
        provider: "other",
        upstream: "up-haiku",
        seat: Some("home"),
        stream: false,
        outcome: "ok",
        error_class: None,
        latency_ms: 800,
        ttfb_ms: None,
        input: Some(50),
        output: Some(10),
        cache_read: None,
        cache_write: None,
        lifetimes: None,
    },
];

fn insert(db: &UsageDb, request_id: &str, ts: i64, shape: &Shape) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, seat, \
             stream, outcome, resolved_class, latency_ms, ttfb_ms, tool_count, msg_count, \
             attempt_count, fallback_count, input_tokens, output_tokens, cache_read, \
             cache_write_tokens, cache_write_lifetimes) \
             VALUES (?1, ?1, ?2, 'anthropic', ?3, ?4, ?3, ?5, ?6, 'anthropic-api', ?7, \
             ?8, ?9, ?10, ?11, ?12, 0, 0, 1, 0, ?13, ?14, ?15, ?16, ?17)",
            rusqlite::params![
                ts,
                request_id,
                shape.model,
                shape.alias,
                shape.provider,
                shape.upstream,
                shape.seat,
                shape.stream,
                shape.outcome,
                shape.error_class,
                shape.latency_ms,
                shape.ttfb_ms,
                shape.input,
                shape.output,
                shape.cache_read,
                shape.cache_write,
                shape.lifetimes,
            ],
        )
        .expect("insert parity row");
}

fn seed(db: &UsageDb) {
    for (i, ts) in instants().into_iter().enumerate() {
        for (j, shape) in SHAPES.iter().enumerate() {
            insert(db, &format!("r{i}-{j}"), ts, shape);
        }
    }
}

/// `nickname` served by `provider` on `upstream`, priced at `pricing` when
/// given.
fn add_model(
    config: &mut Config,
    nickname: &str,
    provider: &str,
    upstream: &str,
    pricing: Option<PricingConfig>,
) {
    let mut entry = ModelEntry::new(provider, upstream);
    entry.pricing = pricing;
    config.models.insert(nickname.to_string(), entry);
}

/// A priced API-key provider, a subscription provider, an unpriced API-key
/// provider and a forwarded provider. Neither priced model has a 30-minute
/// write rate.
fn parity_config() -> Config {
    let mut config = Config::default();
    for (name, credential) in [
        ("paid", "env://PAID_KEY"),
        ("sub", "oauth://anthropic"),
        ("other", "env://OTHER_KEY"),
    ] {
        config
            .providers
            .insert(name.to_string(), ProviderEntry::anthropic_api(credential));
    }
    let rates = PricingConfig {
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(15.0),
        cache_read_per_mtok: Some(0.3),
        cache_write_5m_per_mtok: Some(3.75),
        ..Default::default()
    };
    add_model(&mut config, "opus", "paid", "up-opus", Some(rates.clone()));
    add_model(&mut config, "sonnet", "sub", "up-sonnet", Some(rates));
    add_model(&mut config, "haiku", "other", "up-haiku", None);
    config.providers.insert(
        "relay".to_string(),
        ProviderEntry::anthropic_api("forwarded")
            .with_credential("forwarded")
            .with_auth(Auth::Bearer),
    );
    add_model(&mut config, "relayed", "relay", "up-relayed", None);
    config
}

/// The ledger, the config and the price book both surfaces read, kept alive
/// for the test.
struct Fixture {
    _dir: TempDir,
    ledger: PathBuf,
    config: Config,
    book: Arc<PriceBook>,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let ledger = dir.path().join("usage.db");
    seed(&open(&ledger).expect("open ledger"));
    let book = Arc::new(PriceBook::new(dir.path().join("prices.json")));
    Fixture {
        _dir: dir,
        ledger,
        config: parity_config(),
        book,
    }
}

/// The `/status/query` panel for `window` x `group_by`, built by the
/// endpoint's own parser and panel builder over the fixture.
fn endpoint_result(fx: &Fixture, window: &str, group_by: &str) -> QueryResult {
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(
        fx.config.clone(),
    )))));
    let mut state = StatusState::from_app(&app, None, DaemonMeta::for_test());
    state.prices = Arc::clone(&fx.book);
    let body = format!(r#"{{"window":"{window}","group_by":"{group_by}"}}"#);
    let parsed = parse_body(body.as_bytes()).expect("body in vocabulary");
    let zone = zone();
    let now = pinned_now();
    let (spec, bucket) = parsed.resolve(&zone, now);
    let panel = build_panel(
        &fx.ledger,
        spec,
        bucket,
        &state.pricer().pin(),
        now_utc_rfc3339(),
        &zone,
        now,
        &ReadLimit::from(Instant::now() + Duration::from_secs(30)),
    );
    panel
        .data
        .unwrap_or_else(|| panic!("panel available: {:?}", panel.unavailable))
}

/// The `routectl usage` report for `flag` x `by`, built from the CLI's own
/// flag parse and report builder over the fixture.
fn cli_report(fx: &Fixture, flag: &str, by: Option<GroupDim>) -> WindowReport {
    cli_report_with(fx, flag, by, false)
}

/// [`cli_report`], with `--detail` when `detail` is set.
fn cli_report_with(fx: &Fixture, flag: &str, by: Option<GroupDim>, detail: bool) -> WindowReport {
    let flags = WindowFlags::try_parse_from(["usage", flag]).expect("window flag parses");
    let args = UsageArgs {
        window: flags.window.selected(),
        since: None,
        until: None,
        by,
        detail,
        db: None,
        k_calibration: false,
    };
    let mut db = open(&fx.ledger).expect("open ledger");
    let mut reports = build_window_reports(
        &mut db,
        &fx.config,
        &PriceSnapshot::pin(&fx.book),
        &args,
        &zone(),
        pinned_now(),
    )
    .expect("usage report");
    assert_eq!(reports.len(), 1, "one window flag, one report");
    reports.remove(0)
}

/// The label and `reqs` cell of every rendered table row, in order.
fn rendered_rows(report: &WindowReport) -> Vec<(String, String)> {
    let out = render_report(report, &zone());
    out.lines()
        .skip(2)
        .take(report.groups.len() + 1)
        .map(|line| {
            let mut cells = line.split_whitespace();
            let label = cells.next().expect("label cell").to_string();
            let reqs = cells.next().expect("reqs cell").to_string();
            (label, reqs)
        })
        .collect()
}

/// Every CLI grouping beside the endpoint token it must equal: an absent
/// `--by` groups by model.
fn groupings() -> Vec<(Option<GroupDim>, &'static str)> {
    let mut rows = vec![(None, GroupDim::Model.as_str())];
    rows.extend(GroupDim::ALL.map(|dim| (Some(dim), dim.as_str())));
    rows
}

/// Assert one cell of the window x grouping matrix.
fn assert_cell_agrees(fx: &Fixture, flag: &str, token: &str, by: Option<GroupDim>, dim: &str) {
    let case = format!("{flag} --by {by:?} vs window={token} group_by={dim}");
    let cli = cli_report(fx, flag, by);
    let endpoint = endpoint_result(fx, token, dim);

    let cli_groups: Vec<(&str, &QueryMetrics)> = cli
        .groups
        .iter()
        .map(|row| (row.label.as_str(), &row.metrics))
        .collect();
    let endpoint_groups: Vec<(&str, &QueryMetrics)> = endpoint
        .groups
        .iter()
        .map(|group| (group.label.as_str(), &group.metrics))
        .collect();
    assert_eq!(cli_groups, endpoint_groups, "groups: {case}");
    assert_eq!(cli.total.metrics, endpoint.totals, "totals: {case}");

    let mut expected: Vec<(String, String)> = endpoint
        .groups
        .iter()
        .map(|g| (g.label.clone(), g.metrics.requests.to_string()))
        .collect();
    expected.push(("total".to_string(), endpoint.totals.requests.to_string()));
    assert_eq!(rendered_rows(&cli), expected, "rendered rows: {case}");
}

#[test]
fn the_cli_report_equals_the_query_endpoint_for_every_window_and_grouping() {
    // Arrange
    let fx = fixture();

    // Act + Assert: every window x every grouping.
    for (flag, token) in WINDOWS {
        for (by, dim) in groupings() {
            assert_cell_agrees(&fx, flag, token, by, dim);
        }
    }
}

#[test]
fn the_parity_fixture_gives_every_window_its_own_figures() {
    // The control for the test above: a flag mapped to the wrong window can
    // only be caught if no two windows report the same figures, and the
    // grouped figures must cover the cost, seat and error cases the parity
    // claim is about.
    let fx = fixture();

    let totals: Vec<QueryMetrics> = WINDOWS
        .iter()
        .map(|(_, token)| endpoint_result(&fx, token, "model").totals)
        .collect();
    for (i, a) in totals.iter().enumerate() {
        for (j, b) in totals.iter().enumerate().skip(i + 1) {
            assert_ne!(
                a.requests, b.requests,
                "{} vs {}",
                WINDOWS[i].1, WINDOWS[j].1
            );
        }
    }

    let all = endpoint_result(&fx, "all", "seat");
    let seats: Vec<&str> = all.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(seats, ["#paid", "(unattributed)", "home", "work"]);
    let seat = |label: &str| {
        all.groups
            .iter()
            .find(|g| g.label == label)
            .unwrap_or_else(|| panic!("seat {label}"))
            .metrics
            .requests
    };
    assert_eq!(seat("#paid"), 6, "one unseated API-key row per instant");
    assert_eq!(
        seat("(unattributed)"),
        18,
        "the OAuth entry's two unseated rows and the forwarded row, per instant"
    );
    let by_model = endpoint_result(&fx, "all", "model");
    let status = |label: &str| {
        by_model
            .groups
            .iter()
            .find(|g| g.label == label)
            .unwrap_or_else(|| panic!("model {label}"))
            .metrics
            .cost_status
    };
    assert_eq!(status("opus"), routectl_usage::CostStatus::Partial);
    assert_eq!(status("sonnet"), routectl_usage::CostStatus::Subscription);
    assert_eq!(status("haiku"), routectl_usage::CostStatus::Unpriced);
    assert!(by_model.totals.cost_usd.is_some(), "a priced subtotal");
    assert!(
        by_model.totals.equivalent_cost_usd.is_some(),
        "a subscription equivalent"
    );
    assert_eq!(
        by_model.totals.errors_by_class,
        std::collections::BTreeMap::from([
            ("http-5xx".to_string(), 6),
            ("unclassified".to_string(), 6),
        ])
    );
}

/// The six timing percentiles of a metric set.
fn percentiles(m: &QueryMetrics) -> [Option<i64>; 6] {
    [
        m.ttft_p50_ms,
        m.ttft_p90_ms,
        m.ttft_p99_ms,
        m.latency_p50_ms,
        m.latency_p90_ms,
        m.latency_p99_ms,
    ]
}

/// Assert one cell of the window x grouping matrix on the six percentiles
/// the `--detail` report shows.
fn assert_detail_percentiles_agree(
    fx: &Fixture,
    flag: &str,
    token: &str,
    by: Option<GroupDim>,
    dim: &str,
) {
    let case = format!("{flag} --detail --by {by:?} vs window={token} group_by={dim}");
    let cli = cli_report_with(fx, flag, by, true);
    let endpoint = endpoint_result(fx, token, dim);

    let cli_groups: Vec<(&str, [Option<i64>; 6])> = cli
        .groups
        .iter()
        .map(|row| (row.label.as_str(), percentiles(&row.metrics)))
        .collect();
    let endpoint_groups: Vec<(&str, [Option<i64>; 6])> = endpoint
        .groups
        .iter()
        .map(|group| (group.label.as_str(), percentiles(&group.metrics)))
        .collect();
    assert_eq!(cli_groups, endpoint_groups, "group percentiles: {case}");
    assert_eq!(
        percentiles(&cli.total.metrics),
        percentiles(&endpoint.totals),
        "total percentiles: {case}"
    );
}

#[test]
fn detail_percentiles_equal_the_query_endpoint_for_every_window_and_grouping() {
    // Arrange
    let fx = fixture();

    // Act + Assert: every window x every grouping.
    for (flag, token) in WINDOWS {
        for (by, dim) in groupings() {
            assert_detail_percentiles_agree(&fx, flag, token, by, dim);
        }
    }
}

#[test]
fn the_parity_fixture_yields_every_percentile_in_every_window() {
    // The control for the test above: equal `None`s on both sides would pass
    // it, so every window's total must carry all six figures. Six copies of
    // each shape put rank 44 of 48 (latency p90) in the top block already.
    let fx = fixture();

    for (_, token) in WINDOWS {
        let totals = endpoint_result(&fx, token, "model").totals;
        assert!(
            percentiles(&totals).iter().all(Option::is_some),
            "{token}: {totals:?}"
        );
    }
    let all = endpoint_result(&fx, "all", "model").totals;
    assert_eq!(
        percentiles(&all),
        [
            Some(500),
            Some(900),
            Some(900),
            Some(1_500),
            Some(30_000),
            Some(30_000)
        ]
    );
}

/// Under `--detail` each group's TTFT percentiles come from the rows
/// labelled the way the group is: the API-key entry's unseated stream lands
/// on its account's row, and the forwarded entry's on `(unattributed)`.
#[test]
fn detail_ttft_follows_the_seat_label_of_each_group() {
    // Arrange
    let fx = fixture();

    // Act
    let report = cli_report_with(&fx, "--all", Some(GroupDim::Seat), true);

    // Assert
    let ttft = |label: &str| {
        report
            .groups
            .iter()
            .find(|row| row.label == label)
            .map_or_else(
                || panic!("seat row {label}"),
                |row| {
                    let m = &row.metrics;
                    (m.ttft_p50_ms, m.ttft_p90_ms, m.ttft_p99_ms)
                },
            )
    };
    assert_eq!(ttft("#paid"), (Some(700), Some(700), Some(700)));
    assert_eq!(ttft("(unattributed)"), (Some(500), Some(900), Some(900)));
    assert_eq!(
        ttft("work"),
        (Some(120), Some(300), Some(300)),
        "control: a persisted seat keeps its own samples"
    );
}
