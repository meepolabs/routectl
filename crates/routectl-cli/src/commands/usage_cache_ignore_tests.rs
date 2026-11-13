//! Tests for the `usage --detail` cache-ignore block.

use super::*;

use chrono::{DateTime, NaiveDate};
use routectl_core::cache_decision::CALLER_SUPPLIED;
use routectl_core::provider_kind::config::ANTHROPIC_API;
use routectl_usage::open;

use crate::cache_ignore::{CacheIgnoreWindow, UNAVAILABLE_DB, UNAVAILABLE_QUERY_TIMEOUT};
use crate::commands::pricing::PriceSnapshot;
use crate::commands::usage::{
    UsageArgs, WindowFlag, build_output, build_window_report, render_report, window_bounds,
};
use crate::test_usage_dir::usage_dir;

const BLOCK_HEADER: &str = "cache-ignore:";

fn fixed_now() -> DateTime<Local> {
    Local
        .from_local_datetime(
            &NaiveDate::from_ymd_opt(2026, 6, 11)
                .and_then(|d| d.and_hms_opt(14, 30, 0))
                .expect("valid date"),
        )
        .earliest()
        .expect("representable local time")
}

/// `direct` is observable anthropic-api; `vendor` (custom base, no
/// override) is not, so it is listed and never evaluated.
fn fixture_config() -> Config {
    toml::from_str(
        "[providers.direct]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://USAGE_CACHE_IGNORE_TEST_UNUSED\"\n\
         [providers.vendor]\n\
         kind = \"anthropic-api\"\n\
         base_url = \"https://vendor.example.invalid/anthropic\"\n\
         api_key_ref = \"env://USAGE_CACHE_IGNORE_TEST_UNUSED\"\n",
    )
    .expect("fixture config parses")
}

/// `count` consecutive caller-marked turns on `(direct, model)` an hour
/// before `fixed_now`, in one session; the last reads `last_cache_read`.
/// The first turn has no predecessor, so `count - 1` turns qualify.
fn seed_lane(db: &UsageDb, model: &str, count: i64, last_cache_read: i64) {
    let base = fixed_now().timestamp_millis() - 3_600_000;
    for i in 0..count {
        let cache_read = if i == count - 1 { last_cache_read } else { 0 };
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, provider_kind, seat, session_id, \
                 stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
                 fallback_count, input_tokens, output_tokens, cache_read, cache_write_tokens, \
                 cache_front_decision, prefix_epoch_event, operation, probe) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req', 'a', ?3, 'direct', ?4, NULL, \
                 'sess-1', 0, 'ok', 0, 0, 0, 1, 0, 5000, 10, ?5, 0, ?6, 0, 'inference', 0)",
                rusqlite::params![
                    base + i * 1000,
                    format!("{model}-{i}"),
                    model,
                    ANTHROPIC_API,
                    cache_read,
                    CALLER_SUPPLIED,
                ],
            )
            .expect("insert turn");
    }
}

fn ledger_with_ignoring_lane() -> UsageDb {
    let db = open(usage_dir().join("usage.db")).expect("open ledger");
    seed_lane(&db, "m-ignoring", 4, 0);
    seed_lane(&db, "m-clear", 3, 100);
    db
}

fn args(window: WindowFlag, since: Option<&str>, detail: bool) -> UsageArgs {
    UsageArgs {
        window,
        since: since.map(str::to_owned),
        until: None,
        by: None,
        detail,
        db: None,
        k_calibration: false,
    }
}

fn output(db: &UsageDb, args: &UsageArgs) -> String {
    build_output(
        db,
        &fixture_config(),
        &PriceSnapshot::default(),
        args,
        fixed_now(),
    )
    .expect("usage output builds")
}

/// The block's lines: its header through the line before the legend.
fn block_of(out: &str) -> String {
    let start = out.find(BLOCK_HEADER).expect("cache-ignore block present");
    let rest = &out[start..];
    let end = rest.find("legend:").expect("legend follows the block");
    rest[..end].to_owned()
}

#[test]
fn detail_multi_window_summary_carries_the_block_once_with_the_lane_counts() {
    // Arrange
    let db = ledger_with_ignoring_lane();

    // Act
    let out = output(&db, &args(WindowFlag::None, None, true));

    // Assert
    assert_eq!(out.matches(BLOCK_HEADER).count(), 1, "{out}");
    let block = block_of(&out);
    let lines: Vec<&str> = block.lines().collect();
    assert_eq!(
        lines[1..],
        [
            "  provider=direct seat=- model=m-ignoring kind=anthropic-api verdict=ignoring \
             evidence=3 active=0 sessions=1  (markers sent, no cache activity)",
            "  provider=direct seat=- model=m-clear kind=anthropic-api verdict=clear \
             evidence=1 active=1 sessions=1",
            "  not evaluated (cache hits not observable): vendor",
        ],
        "{block}"
    );
}

#[test]
fn without_detail_the_output_is_the_window_blocks_and_legend_alone() {
    // Arrange: the pre-block composition, rendered from the same parts.
    let db = ledger_with_ignoring_lane();
    let config = fixture_config();
    let windows = [
        (WindowFlag::Today, "== today =="),
        (WindowFlag::ThisWeek, "== this week =="),
        (WindowFlag::ThisMonth, "== this month =="),
        (WindowFlag::All, "== all time =="),
    ];
    let mut expected: Vec<String> = windows
        .iter()
        .map(|(flag, title)| {
            let bounds = window_bounds(*flag, fixed_now());
            let report = build_window_report(
                &db,
                &config,
                &PriceSnapshot::default(),
                (*title).to_owned(),
                bounds,
                None,
                false,
            )
            .expect("window report");
            render_report(&report)
        })
        .collect();

    // Act
    let out = output(&db, &args(WindowFlag::None, None, false));

    // Assert: everything before the legend is exactly the four window blocks.
    let legend_start = out.rfind("legend:").expect("legend present");
    if let Some(last) = expected.last_mut() {
        last.push_str(&out[legend_start..]);
    }
    assert_eq!(out, expected.join("\n"));
    assert!(!out.contains(BLOCK_HEADER), "{out}");
}

#[test]
fn the_block_ignores_the_window_and_since_flags() {
    // Arrange
    let db = ledger_with_ignoring_lane();
    let requests = [
        args(WindowFlag::None, None, true),
        args(WindowFlag::Today, None, true),
        args(WindowFlag::All, None, true),
        args(WindowFlag::None, Some("2026-01-01"), true),
    ];

    // Act
    let blocks: Vec<String> = requests.iter().map(|a| block_of(&output(&db, a))).collect();

    // Assert
    for (i, block) in blocks.iter().enumerate() {
        assert_eq!(block, &blocks[0], "request {i}");
    }
    assert!(blocks[0].contains("verdict=ignoring"), "{}", blocks[0]);
}

#[test]
fn an_unavailable_report_renders_one_class_line() {
    // Arrange
    let db = open(usage_dir().join("usage.db")).expect("open ledger");
    db.conn()
        .execute_batch("DROP TABLE requests")
        .expect("drop the requests table");
    let now_ms = fixed_now().timestamp_millis();
    let far = Instant::now() + Duration::from_mins(10);

    // Act
    let block = cache_ignore_block(&db, &fixture_config(), now_ms, far);

    // Assert
    let unavailable: Vec<&str> = block
        .lines()
        .filter(|l| l.trim_start().starts_with("unavailable:"))
        .collect();
    assert_eq!(
        unavailable,
        [format!("  unavailable: {UNAVAILABLE_DB}")],
        "{block}"
    );
    assert!(!block.contains("provider="), "{block}");
}

#[test]
fn a_truncated_report_says_more_lanes_exist() {
    // Arrange
    let report = CacheIgnoreReport {
        window: CacheIgnoreWindow::ending_at(fixed_now().timestamp_millis()),
        status: CacheIgnoreStatus::Evaluated,
        lanes: vec![CacheIgnoreLaneReport {
            provider: "direct".to_owned(),
            seat: Some("a".to_owned()),
            model: Some("m".to_owned()),
            provider_kind: ANTHROPIC_API.to_owned(),
            verdict: LaneVerdict::Insufficient,
            evidence_turns: 1,
            active_turns: 0,
            sessions: 1,
        }],
        unobservable: Vec::new(),
        truncated: true,
    };

    // Act
    let block = render_cache_ignore(&report);

    // Assert
    assert!(block.contains("seat=a model=m"), "{block}");
    assert!(
        block.contains(&format!(
            "more lanes than the {CACHE_IGNORE_LANE_CAP}-lane cap"
        )),
        "{block}"
    );
}

#[test]
fn a_timed_out_read_renders_query_timeout_not_an_error() {
    // Arrange
    let report = CacheIgnoreReport {
        window: CacheIgnoreWindow::ending_at(0),
        status: CacheIgnoreStatus::Unavailable(UNAVAILABLE_QUERY_TIMEOUT),
        lanes: Vec::new(),
        unobservable: Vec::new(),
        truncated: false,
    };

    // Act
    let block = render_cache_ignore(&report);

    // Assert
    assert_eq!(block.lines().count(), 2, "{block}");
    assert!(block.ends_with("  unavailable: query_timeout\n"), "{block}");
}

#[test]
fn lane_identifiers_render_scrubbed_and_neutralized() {
    // Arrange: a client-sent model carrying a credential-shaped run, and a
    // seat carrying `=`, which would forge a field on the lane line.
    let token = format!("ghp_{}", "a".repeat(36));
    let report = CacheIgnoreReport {
        window: CacheIgnoreWindow::ending_at(0),
        status: CacheIgnoreStatus::Evaluated,
        lanes: vec![CacheIgnoreLaneReport {
            provider: "direct".to_owned(),
            seat: Some("s verdict=clear".to_owned()),
            model: Some(format!("m {token}")),
            provider_kind: ANTHROPIC_API.to_owned(),
            verdict: LaneVerdict::Ignoring,
            evidence_turns: 3,
            active_turns: 0,
            sessions: 1,
        }],
        unobservable: vec!["v=x".to_owned()],
        truncated: false,
    };

    // Act
    let block = render_cache_ignore(&report);

    // Assert
    assert!(!block.contains(&token), "{block}");
    assert!(block.contains("model=m [redacted]"), "{block}");
    assert!(block.contains("seat=s verdict?clear"), "{block}");
    assert_eq!(block.matches("verdict=").count(), 1, "{block}");
    assert!(block.contains("not observable): v?x"), "{block}");
}
