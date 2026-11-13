//! Tests for the cache-ignore report assembler.

use super::*;

use std::path::PathBuf;

use routectl_core::cache_decision::CALLER_SUPPLIED;
use routectl_usage::{CACHE_IGNORE_LANE_CAP, open, open_readonly};

use crate::test_usage_dir::usage_dir;

/// Every fixture turn sits an hour before this, well inside the window.
const NOW_MS: i64 = 10_000_000_000;
const TURN_BASE_MS: i64 = NOW_MS - 3_600_000;
const STEP_MS: i64 = 1_000;

/// Providers covering each eligibility path:
/// - `direct`: anthropic-api on the default base (observable).
/// - `aws`: bedrock (observable on both shapes).
/// - `vendor`: anthropic-api on a custom base with no override (not observable).
/// - `resp`: openai-responses, observable by default.
/// - `resp_dark`: openai-responses with an explicit non-observable override.
fn fixture_config() -> Config {
    toml::from_str(
        "[providers.direct]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://CACHE_IGNORE_TEST_UNUSED\"\n\
         [providers.aws]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         creds = { kind = \"default-chain\" }\n\
         [providers.vendor]\n\
         kind = \"anthropic-api\"\n\
         base_url = \"https://vendor.example.invalid/anthropic\"\n\
         api_key_ref = \"env://CACHE_IGNORE_TEST_UNUSED\"\n\
         [providers.resp]\n\
         kind = \"openai-responses\"\n\
         api_key_ref = \"env://CACHE_IGNORE_TEST_UNUSED\"\n\
         [providers.resp_dark]\n\
         kind = \"openai-responses\"\n\
         api_key_ref = \"env://CACHE_IGNORE_TEST_UNUSED\"\n\
         cache_capability = { supports_top_level_cache_control = false, cache_hit_observable = false }\n",
    )
    .expect("fixture config parses")
}

/// One qualifying turn on `(provider, model)` of `kind`, carrying the
/// caller-supplied decision token as core spells it.
struct Turn<'a> {
    provider: &'a str,
    kind: &'a str,
    model: &'a str,
    ts_start: i64,
    input: i64,
    cache_read: i64,
}

fn insert(db: &UsageDb, request_id: &str, t: &Turn<'_>) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, provider_kind, seat, session_id, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, cache_read, cache_write_tokens, \
             cache_front_decision, prefix_epoch_event, operation, probe) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req', 'a', ?3, ?4, ?5, NULL, 'sess-1', 0, \
             'ok', 0, 0, 0, 1, 0, ?6, ?7, 0, ?8, 0, 'inference', 0)",
            rusqlite::params![
                t.ts_start,
                request_id,
                t.model,
                t.provider,
                t.kind,
                t.input,
                t.cache_read,
                CALLER_SUPPLIED,
            ],
        )
        .expect("insert turn");
}

/// `count` consecutive turns on one lane; the first has no previous turn,
/// so a lane of `count` turns carries `count - 1` qualifying turns. The
/// last turn reads `last_cache_read` from the cache.
fn seed_lane(
    db: &UsageDb,
    provider: &str,
    kind: &str,
    model: &str,
    count: i64,
    last_cache_read: i64,
) {
    for i in 0..count {
        let turn = Turn {
            provider,
            kind,
            model,
            ts_start: TURN_BASE_MS + i * STEP_MS,
            input: 5000,
            cache_read: if i == count - 1 { last_cache_read } else { 0 },
        };
        insert(db, &format!("{provider}-{model}-{i}"), &turn);
    }
}

fn fresh_ledger() -> (PathBuf, UsageDb) {
    let path = usage_dir().join("usage.db");
    let db = open(&path).expect("open ledger");
    (path, db)
}

fn far_deadline() -> Instant {
    Instant::now() + Duration::from_mins(10)
}

fn lane<'r>(
    report: &'r CacheIgnoreReport,
    provider: &str,
    model: &str,
) -> Option<&'r CacheIgnoreLaneReport> {
    report
        .lanes
        .iter()
        .find(|l| l.provider == provider && l.model.as_deref() == Some(model))
}

#[test]
fn lane_verdicts_follow_config_eligibility_and_the_evidence_counts() {
    // Arrange
    let (_path, db) = fresh_ledger();
    seed_lane(&db, "direct", ANTHROPIC_API, "m-ignoring", 4, 0);
    seed_lane(&db, "direct", ANTHROPIC_API, "m-clear", 4, 100);
    seed_lane(&db, "aws", BEDROCK, "m-thin", 3, 0);
    seed_lane(&db, "vendor", ANTHROPIC_API, "m-dark", 4, 0);
    seed_lane(&db, "gone", ANTHROPIC_API, "m-gone", 4, 0);
    // Tagged with a marker-carrying kind so the query WOULD count it if the
    // assembler handed it a rule: only the configured kind keeps it out.
    seed_lane(&db, "resp", ANTHROPIC_API, "m-resp", 4, 0);

    // Act
    let report = cache_ignore_report(&db, &fixture_config(), NOW_MS, far_deadline());

    // Assert
    assert_eq!(report.status, CacheIgnoreStatus::Evaluated);
    let rows: [(&str, &str, &str, Option<LaneVerdict>); 6] = [
        (
            "evidence 3, active 0",
            "direct",
            "m-ignoring",
            Some(LaneVerdict::Ignoring),
        ),
        ("active > 0", "direct", "m-clear", Some(LaneVerdict::Clear)),
        (
            "evidence 2",
            "aws",
            "m-thin",
            Some(LaneVerdict::Insufficient),
        ),
        ("hit not observable", "vendor", "m-dark", None),
        ("provider absent from config", "gone", "m-gone", None),
        ("openai-responses provider", "resp", "m-resp", None),
    ];
    for (name, provider, model, expected) in rows {
        let got = lane(&report, provider, model).map(|l| l.verdict);
        assert_eq!(got, expected, "row: {name}");
    }
    assert_eq!(
        report.lanes.len(),
        3,
        "only the three eligible lanes are reported"
    );
    assert_eq!(
        report.unobservable,
        vec!["vendor".to_owned()],
        "only configured anthropic-api / bedrock providers are listed; \
         an absent or openai-responses provider never is"
    );
    assert!(!report.truncated);
    let ignoring = lane(&report, "direct", "m-ignoring").expect("ignoring lane");
    assert_eq!(
        (
            ignoring.evidence_turns,
            ignoring.active_turns,
            ignoring.sessions
        ),
        (3, 0, 1)
    );
}

#[test]
fn rows_spelled_with_the_core_kind_and_decision_tokens_are_counted() {
    // Arrange: `insert` writes the core `CALLER_SUPPLIED` token, and each lane
    // carries a core kind constant, so a mirror typo in the query drops them.
    let (_path, db) = fresh_ledger();
    seed_lane(&db, "direct", ANTHROPIC_API, "m-a", 4, 0);
    seed_lane(&db, "aws", BEDROCK, "m-b", 4, 0);

    // Act
    let report = cache_ignore_report(&db, &fixture_config(), NOW_MS, far_deadline());

    // Assert
    for (provider, model) in [("direct", "m-a"), ("aws", "m-b")] {
        let got = lane(&report, provider, model).map(|l| l.evidence_turns);
        assert_eq!(got, Some(3), "lane {provider}/{model}");
    }
}

#[test]
fn an_expired_deadline_reports_query_timeout_and_leaves_the_connection_usable() {
    // Arrange: enough rows that the query runs past one progress callback.
    let (_path, db) = fresh_ledger();
    db.conn().execute_batch("BEGIN").expect("begin");
    for lane_no in 0..500 {
        seed_lane(&db, "direct", ANTHROPIC_API, &format!("m-{lane_no}"), 8, 0);
    }
    db.conn().execute_batch("COMMIT").expect("commit");
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("instant predates the process start");
    let config = fixture_config();

    let window = CacheIgnoreWindow::ending_at(NOW_MS);
    let rule = CacheIgnoreRule {
        provider: "direct".to_owned(),
        min_prompt_tokens: 0,
        lifetime_ms: 300_000,
    };

    // Act
    let shed = cache_ignore_report(&db, &config, NOW_MS, expired);
    // No guard installed here: a deadline left behind by the timed-out call
    // would interrupt this read.
    let unguarded = cache_ignore_lanes(&db, window.from_ms, window.to_ms, &[rule]);
    let after = cache_ignore_report(&db, &config, NOW_MS, far_deadline());

    // Assert
    assert_eq!(
        shed.status,
        CacheIgnoreStatus::Unavailable(UNAVAILABLE_QUERY_TIMEOUT)
    );
    assert!(shed.lanes.is_empty());
    assert_eq!(shed.unobservable, vec!["vendor".to_owned()]);
    let unguarded = unguarded.expect("an unguarded read after the timeout succeeds");
    assert_eq!(unguarded.lanes.len(), CACHE_IGNORE_LANE_CAP);
    assert_eq!(after.status, CacheIgnoreStatus::Evaluated);
    assert_eq!(after.lanes.len(), CACHE_IGNORE_LANE_CAP);
}

/// Evidence on the `direct` lane for two turns `gap_ms` apart, the second
/// of `input` prompt tokens.
fn direct_evidence(gap_ms: i64, input: i64) -> Option<i64> {
    let (_path, db) = fresh_ledger();
    for (i, (ts_start, input)) in [(TURN_BASE_MS, 5000), (TURN_BASE_MS + gap_ms, input)]
        .into_iter()
        .enumerate()
    {
        let turn = Turn {
            provider: "direct",
            kind: ANTHROPIC_API,
            model: "m",
            ts_start,
            input,
            cache_read: 0,
        };
        insert(&db, &format!("direct-{i}"), &turn);
    }
    let report = cache_ignore_report(&db, &fixture_config(), NOW_MS, far_deadline());
    lane(&report, "direct", "m").map(|l| l.evidence_turns)
}

/// The anthropic-api rule is a 300 s lifetime and a 4096-token minimum; the
/// assembler must hand the query both in the query's units.
#[test]
fn the_rule_built_from_config_bounds_the_lifetime_and_the_minimum() {
    let rows: [(&str, i64, i64, Option<i64>); 4] = [
        ("gap at the lifetime", 300_000, 5000, Some(1)),
        ("gap one ms past the lifetime", 300_001, 5000, None),
        ("prompt at the minimum", STEP_MS, 4096, Some(1)),
        ("prompt one under the minimum", STEP_MS, 4095, None),
    ];

    let red: Vec<_> = rows
        .into_iter()
        .map(|(name, gap_ms, input, expected)| (name, direct_evidence(gap_ms, input), expected))
        .filter(|(_, got, expected)| got != expected)
        .collect();

    assert!(red.is_empty(), "(row, got, expected): {red:#?}");
}

#[test]
fn truncation_from_the_query_reaches_the_report() {
    // Arrange: one lane more than the query returns.
    let (_path, db) = fresh_ledger();
    let lanes = i64::try_from(CACHE_IGNORE_LANE_CAP).expect("cap fits") + 1;
    db.conn().execute_batch("BEGIN").expect("begin");
    for lane_no in 0..lanes {
        seed_lane(&db, "direct", ANTHROPIC_API, &format!("m-{lane_no}"), 2, 0);
    }
    db.conn().execute_batch("COMMIT").expect("commit");

    // Act
    let report = cache_ignore_report(&db, &fixture_config(), NOW_MS, far_deadline());

    // Assert
    assert_eq!(report.status, CacheIgnoreStatus::Evaluated);
    assert!(report.truncated);
    assert_eq!(report.lanes.len(), CACHE_IGNORE_LANE_CAP);
}

#[test]
fn a_failing_query_reports_db_unavailable() {
    // Arrange
    let (_path, db) = fresh_ledger();
    db.conn()
        .execute_batch("DROP TABLE requests")
        .expect("drop the requests table");

    // Act
    let report = cache_ignore_report(&db, &fixture_config(), NOW_MS, far_deadline());

    // Assert
    assert_eq!(
        report.status,
        CacheIgnoreStatus::Unavailable(UNAVAILABLE_DB)
    );
    assert_eq!(report.status.as_str(), "unavailable");
}

#[test]
fn a_missing_ledger_reports_no_data_and_still_lists_unobservable_providers() {
    // Arrange
    let missing = usage_dir().join("usage.db");
    let Err(err) = open_readonly(&missing) else {
        panic!("a missing ledger file must not open");
    };

    // Act
    let report = CacheIgnoreReport::open_failed(&fixture_config(), NOW_MS, &err);

    // Assert
    assert_eq!(
        report.status,
        CacheIgnoreStatus::Unavailable(UNAVAILABLE_NO_DATA)
    );
    assert_eq!(report.unobservable, vec!["vendor".to_owned()]);
    assert_eq!(report.window, CacheIgnoreWindow::ending_at(NOW_MS));
}

#[test]
fn the_window_is_the_six_hours_ending_at_now() {
    let window = CacheIgnoreWindow::ending_at(NOW_MS);

    assert_eq!(window.to_ms, NOW_MS);
    assert_eq!(window.to_ms - window.from_ms, 6 * 3_600_000);
}
