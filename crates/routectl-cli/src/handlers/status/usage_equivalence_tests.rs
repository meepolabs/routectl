//! Differential guard: `/status/query` and the `/status/usage` panel must
//! report the same truth over their COMMON projection, and must both match an
//! independent hand-computed expectation for the seeded window.
//!
//! Both folds are driven directly, below one `open` and against LITERAL window
//! bounds, so a single connection and a single snapshot feed both and no clock
//! read can confound the comparison. The pricer is `RowCost::Unpriced` for
//! every row: no cost figure is comparable between the two surfaces, so
//! pricing is kept out of the test entirely.
//!
//! Why a hand oracle and not just A-vs-B: `AGG_SQL` and `QUERY_AGG_SQL` are
//! assembled at compile time from the SAME `agg_base_columns!()` /
//! `agg_group_by!()` literals, so SQL-level drift on the shared fields is
//! impossible by construction and a pure A-vs-B comparison is near-tautological
//! on exactly the comparable fields -- both paths can be identically wrong and
//! still agree. The literal expectations below close that common-mode gap for
//! the values the fixture encodes.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use routectl_usage::{GroupDim, QueryResult, QuerySpec, RowCost, open, query};
use tempfile::TempDir;

use super::*;

/// Inclusive lower bound of the window under test, epoch-ms. A literal, never
/// a clock read: the two production handlers each take their own `Local::now()`,
/// and reproducing that skew here would only make the guard flaky.
const FROM_MS: i64 = 1_700_000_000_000;

/// Exclusive upper bound: one calendar day after [`FROM_MS`].
const TO_MS: i64 = FROM_MS + 86_400_000;

// --- the hand-computed oracle -------------------------------------------
//
// Derived BY HAND from `seed` below, not from a program run. `-` is a NULL
// column. Rows A and D sit at `from_ms - 1` and `to_ms`, outside the half-open
// interval, and contribute to nothing; the seven rows below are in-window:
//
//   row  outcome           stream ttfb  in   out  reas cread cw5m cw1h   stu   fb
//   B    ok                  1    100   10    20    5     7    3    2   {2}     0
//   C    ok                  0     -     4     6    -     -    -    -    -      0
//   E    ok                  1     -     -    11    -     -    -    -    -      0
//   F    upstream_error      1     50    2     3    -     -    -    -    -      0
//   G    ok                  0     -     1     1    -     -    -    -    -      3
//   H    client_disconnect   1     -     -     -    -     9    -    -   {1,4}   0
//   I    ok                  1     -   100   200    -    50   10   20    -      0
//
// requests                = COUNT(*)                                      = 7
// ok                      = B + C + E + G + I                             = 5
// errors                  = outcome NOT IN (ok, client_disconnect) = F     = 1
// client_disconnect_total = H                                             = 1
//   (and 7 == 5 + 1 + 1, the panel's own reconciliation identity)
// input_tokens            = 10 + 4 + 0 + 2 + 1 + 0 + 100                  = 117
// output_tokens           = 20 + 6 + 11 + 3 + 1 + 0 + 200                 = 241
// reasoning_tokens        = 5 (only B reports one)                        = 5
// cache_read_billed       = SUM(cache_read) = 7 + 9 + 50                  = 66
// server_tool_calls       = summed json values: 2 + (1 + 4)               = 7
// cache_write_5m          = 3 + 10                                        = 13
// cache_write_1h          = 2 + 20                                        = 22
// stream_count            = SUM(stream) = B + E + F + H + I               = 5

const EXPECTED_REQUESTS: i64 = 7;
const EXPECTED_OK: i64 = 5;
const EXPECTED_ERRORS: i64 = 1;
const EXPECTED_CLIENT_DISCONNECT_TOTAL: i64 = 1;
const EXPECTED_INPUT_TOKENS: i64 = 117;
const EXPECTED_OUTPUT_TOKENS: i64 = 241;
const EXPECTED_REASONING_TOKENS: i64 = 5;
const EXPECTED_CACHE_READ_BILLED: i64 = 66;
const EXPECTED_SERVER_TOOL_CALLS: i64 = 7;
const EXPECTED_CACHE_WRITE_5M: i64 = 13;
const EXPECTED_CACHE_WRITE_1H: i64 = 22;
const EXPECTED_STREAM_COUNT: i64 = 5;

/// The metric vocabulary BOTH surfaces carry -- the projection this guard
/// asserts equal. Every token is a contracts sec-15 wire name and an `i64` on
/// both sides.
const SHARED: [&str; 12] = [
    "requests",
    "ok",
    "errors",
    "input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "cache_read_billed",
    "server_tool_calls",
    "client_disconnect_total",
    "cache_write_5m",
    "cache_write_1h",
    "stream_count",
];

/// `QueryMetrics` tokens with NO panel counterpart. They cannot diverge
/// BETWEEN the paths because only one path computes them; their correctness
/// rests on the query path's own tests.
const QUERY_ONLY: [&str; 12] = [
    "fallback_served",
    "ttft_p50_ms",
    "ttft_p95_ms",
    "latency_p50_ms",
    "latency_p95_ms",
    "throughput_tok_s",
    "ctx_avg",
    "ctx_peak",
    "cache_hit_pct",
    "cost_usd",
    "equivalent_cost_usd",
    "cost_status",
];

/// Panel metric-surface fields with NO `/status/query` counterpart: the group
/// key the panel reports at its own fine grain, plus the panel-only figures.
const PANEL_ONLY: [&str; 7] = [
    "alias",
    "provider",
    "model",
    "upstream",
    "cache_read_peak",
    "cache_read_present",
    "errors_by_class",
];

/// One ledger row's tunable columns. Defaults describe a minimal
/// non-streaming success reporting no token counters, so each seeded row sets
/// only the columns its expectation depends on.
///
/// Every seeded row nonetheless sets `outcome` explicitly -- see [`seed`].
struct Fixture {
    request_id: &'static str,
    ts_start: i64,
    model: Option<&'static str>,
    provider: Option<&'static str>,
    upstream: Option<&'static str>,
    alias: &'static str,
    outcome: &'static str,
    stream: i64,
    latency_ms: i64,
    ttfb_ms: Option<i64>,
    fallback_count: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cache_read: Option<i64>,
    cache_write_5m: Option<i64>,
    cache_write_1h: Option<i64>,
    /// The raw `server_tool_use` JSON object, whose integer values sum into
    /// `server_tool_calls`.
    server_tool_use: Option<&'static str>,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            request_id: "r",
            ts_start: FROM_MS,
            model: Some("m1"),
            provider: Some("pa"),
            upstream: Some("u1"),
            alias: "al",
            outcome: "ok",
            stream: 0,
            latency_ms: 10,
            ttfb_ms: None,
            fallback_count: 0,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            server_tool_use: None,
        }
    }
}

fn insert(db: &UsageDb, f: &Fixture) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens, reasoning_tokens, \
             cache_read, cache_write_5m, cache_write_1h, server_tool_use) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, ?4, ?5, ?6, ?7, ?8, \
             ?9, ?10, 0, 0, 1, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            rusqlite::params![
                f.ts_start,
                f.request_id,
                f.alias,
                f.model,
                f.provider,
                f.upstream,
                f.stream,
                f.outcome,
                f.latency_ms,
                f.ttfb_ms,
                f.fallback_count,
                f.input_tokens,
                f.output_tokens,
                f.reasoning_tokens,
                f.cache_read,
                f.cache_write_5m,
                f.cache_write_1h,
                f.server_tool_use,
            ],
        )
        .expect("insert fixture row");
}

/// Seed the seven divergence-prone shapes the oracle above is computed from.
///
/// Every row sets `outcome` EXPLICITLY. `Outcome`'s Rust `#[default]` is
/// `ClientDisconnect` (it is the abnormal-exit sentinel), which lands OUTSIDE
/// the `errors` predicate and INSIDE `client_disconnect_total` -- a defaulted
/// outcome would silently move two of the twelve expectations.
fn seed(db: &UsageDb) {
    // A/D: the half-open interval's two exclusions. Both carry large counters,
    // so leaking either one would move every token in the oracle.
    let excluded = Fixture {
        stream: 1,
        input_tokens: Some(9_000),
        output_tokens: Some(9_000),
        reasoning_tokens: Some(9_000),
        cache_read: Some(9_000),
        cache_write_5m: Some(9_000),
        cache_write_1h: Some(9_000),
        server_tool_use: Some(r#"{"leak":9000}"#),
        ..Fixture::default()
    };
    insert(
        db,
        &Fixture {
            request_id: "a-before-window",
            ts_start: FROM_MS - 1,
            outcome: "ok",
            ..excluded
        },
    );
    insert(
        db,
        &Fixture {
            request_id: "d-at-upper-bound",
            ts_start: TO_MS,
            outcome: "ok",
            ..excluded
        },
    );

    // B: the inclusive lower bound. The only row reporting reasoning tokens,
    // cache writes, and a server-tool map.
    insert(
        db,
        &Fixture {
            request_id: "b-at-lower-bound",
            ts_start: FROM_MS,
            outcome: "ok",
            stream: 1,
            latency_ms: 500,
            ttfb_ms: Some(100),
            input_tokens: Some(10),
            output_tokens: Some(20),
            reasoning_tokens: Some(5),
            cache_read: Some(7),
            cache_write_5m: Some(3),
            cache_write_1h: Some(2),
            server_tool_use: Some(r#"{"web_search":2}"#),
            ..Fixture::default()
        },
    );

    // C: the inclusive upper edge, AND the second upstream under model `m1`,
    // so the fine grain fans out and both rollup folds do real work.
    insert(
        db,
        &Fixture {
            request_id: "c-at-upper-edge-second-upstream",
            ts_start: TO_MS - 1,
            upstream: Some("u2"),
            outcome: "ok",
            input_tokens: Some(4),
            output_tokens: Some(6),
            ..Fixture::default()
        },
    );

    // E: NULL ttfb_ms, NULL input_tokens, NULL cache_read on a streaming
    // success -- the shapes where a COALESCE or a COUNT-vs-SUM confusion shows.
    insert(
        db,
        &Fixture {
            request_id: "e-null-counters",
            ts_start: FROM_MS + 1_000,
            outcome: "ok",
            stream: 1,
            output_tokens: Some(11),
            ..Fixture::default()
        },
    );

    // F: a mid-stream FAILURE that still stamped a ttfb_ms. The historical
    // defect's shape: it must count as an error and stay out of every
    // streaming-success population.
    insert(
        db,
        &Fixture {
            request_id: "f-mid-stream-failure-with-ttfb",
            ts_start: FROM_MS + 2_000,
            outcome: "upstream_error",
            stream: 1,
            latency_ms: 300,
            ttfb_ms: Some(50),
            input_tokens: Some(2),
            output_tokens: Some(3),
            ..Fixture::default()
        },
    );

    // G: served after a fallback. `fallback_count = 3` is ONE request, never
    // three (contracts sec 15).
    insert(
        db,
        &Fixture {
            request_id: "g-fallback-served",
            ts_start: FROM_MS + 3_000,
            outcome: "ok",
            fallback_count: 3,
            input_tokens: Some(1),
            output_tokens: Some(1),
            ..Fixture::default()
        },
    );

    // H: a client hangup. Outside `errors`, inside `client_disconnect_total`,
    // and still contributing its cache-read snapshot and server-tool calls.
    insert(
        db,
        &Fixture {
            request_id: "h-client-disconnect",
            ts_start: FROM_MS + 4_000,
            outcome: "client_disconnect",
            stream: 1,
            cache_read: Some(9),
            server_tool_use: Some(r#"{"a":1,"b":4}"#),
            ..Fixture::default()
        },
    );

    // I: a second model on its own provider / upstream / alias, with no
    // configured price. Nothing here is priced (the test's pricer answers
    // `Unpriced` for every row), so this row exists to give the coarse rollup
    // a second label to fold.
    insert(
        db,
        &Fixture {
            request_id: "i-unpriced-model",
            ts_start: FROM_MS + 5_000,
            model: Some("m-unpriced"),
            provider: Some("pb"),
            upstream: Some("u3"),
            alias: "al2",
            outcome: "ok",
            stream: 1,
            input_tokens: Some(100),
            output_tokens: Some(200),
            cache_read: Some(50),
            cache_write_5m: Some(10),
            cache_write_1h: Some(20),
            ..Fixture::default()
        },
    );
}

/// Seed one ledger and run BOTH folds against it: the panel's `collect` and
/// the `/status/query` `query`, over the same literal window, on the same
/// connection, in that order.
///
/// The `TempDir` is returned so the caller keeps the ledger alive for the
/// duration of the test.
fn both_folds() -> (TempDir, UsagePanel, QueryResult) {
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open ledger");
    seed(&db);

    let bounds = WindowBounds {
        from_ms: FROM_MS,
        to_ms: TO_MS,
    };
    let panel = collect(
        &db,
        "today",
        bounds,
        Instant::now() + Duration::from_mins(10),
    )
    .expect("panel fold");
    let spec = QuerySpec {
        from_ms: bounds.from_ms,
        to_ms: bounds.to_ms,
        group_by: GroupDim::Model,
        alias_filter: None,
        provider_filter: None,
        bucket: None,
    };
    let result = query(
        &db,
        &spec,
        |_| RowCost::Unpriced,
        Instant::now() + Duration::from_mins(10),
    )
    .expect("query fold");
    (dir, panel, result)
}

/// Sum one per-group panel field across every rollup group. Three of the
/// twelve shared tokens are panel-side per-group only, so their window-wide
/// value is the sum over the groups the panel reports.
fn panel_group_sum(panel: &UsagePanel, field: impl Fn(&UsageGroup) -> i64) -> i64 {
    panel.groups.iter().map(field).sum()
}

/// The serialized key set of one JSON object.
fn keys_of(value: &serde_json::Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("a metric surface serializes as a JSON object")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn shared_projection_agrees_between_query_and_usage_panel() {
    // Arrange
    let (_dir, panel, result) = both_folds();

    // Act
    let p = &panel.totals;
    let q = &result.totals;

    // Assert: the nine tokens both surfaces carry at window totals.
    assert_eq!(p.requests, q.requests, "requests");
    assert_eq!(p.ok, q.ok, "ok");
    assert_eq!(p.errors, q.errors, "errors");
    assert_eq!(p.input_tokens, q.input_tokens, "input_tokens");
    assert_eq!(p.output_tokens, q.output_tokens, "output_tokens");
    assert_eq!(p.reasoning_tokens, q.reasoning_tokens, "reasoning_tokens");
    assert_eq!(
        p.cache_read_billed, q.cache_read_billed,
        "cache_read_billed"
    );
    assert_eq!(
        p.server_tool_calls, q.server_tool_calls,
        "server_tool_calls"
    );
    assert_eq!(
        p.client_disconnect_total, q.client_disconnect_total,
        "client_disconnect_total"
    );

    // And the three the panel reports per group only.
    assert_eq!(
        panel_group_sum(&panel, |g| g.cache_write_5m),
        q.cache_write_5m,
        "cache_write_5m"
    );
    assert_eq!(
        panel_group_sum(&panel, |g| g.cache_write_1h),
        q.cache_write_1h,
        "cache_write_1h"
    );
    assert_eq!(
        panel_group_sum(&panel, |g| g.stream_count),
        q.stream_count,
        "stream_count"
    );
}

#[test]
fn both_paths_match_the_hand_computed_window_oracle() {
    // Both paths assemble their SQL from the same shared column literals, so
    // agreeing with each other does not prove either is right. This asserts
    // both against the independent derivation at the top of this file.
    let (_dir, panel, result) = both_folds();

    for (label, requests, ok, errors, disconnects, input, output, reasoning, cread, stu) in [
        (
            "usage panel",
            panel.totals.requests,
            panel.totals.ok,
            panel.totals.errors,
            panel.totals.client_disconnect_total,
            panel.totals.input_tokens,
            panel.totals.output_tokens,
            panel.totals.reasoning_tokens,
            panel.totals.cache_read_billed,
            panel.totals.server_tool_calls,
        ),
        (
            "status query",
            result.totals.requests,
            result.totals.ok,
            result.totals.errors,
            result.totals.client_disconnect_total,
            result.totals.input_tokens,
            result.totals.output_tokens,
            result.totals.reasoning_tokens,
            result.totals.cache_read_billed,
            result.totals.server_tool_calls,
        ),
    ] {
        assert_eq!(requests, EXPECTED_REQUESTS, "{label}: requests");
        assert_eq!(ok, EXPECTED_OK, "{label}: ok");
        assert_eq!(errors, EXPECTED_ERRORS, "{label}: errors");
        assert_eq!(
            disconnects, EXPECTED_CLIENT_DISCONNECT_TOTAL,
            "{label}: client_disconnect_total"
        );
        assert_eq!(
            requests,
            ok + errors + disconnects,
            "{label}: requests == ok + errors + client_disconnect_total"
        );
        assert_eq!(input, EXPECTED_INPUT_TOKENS, "{label}: input_tokens");
        assert_eq!(output, EXPECTED_OUTPUT_TOKENS, "{label}: output_tokens");
        assert_eq!(
            reasoning, EXPECTED_REASONING_TOKENS,
            "{label}: reasoning_tokens"
        );
        assert_eq!(
            cread, EXPECTED_CACHE_READ_BILLED,
            "{label}: cache_read_billed"
        );
        assert_eq!(
            stu, EXPECTED_SERVER_TOOL_CALLS,
            "{label}: server_tool_calls"
        );
    }

    // The three panel-side per-group tokens against the same oracle.
    assert_eq!(
        panel_group_sum(&panel, |g| g.cache_write_5m),
        EXPECTED_CACHE_WRITE_5M,
        "usage panel: cache_write_5m"
    );
    assert_eq!(
        panel_group_sum(&panel, |g| g.cache_write_1h),
        EXPECTED_CACHE_WRITE_1H,
        "usage panel: cache_write_1h"
    );
    assert_eq!(
        panel_group_sum(&panel, |g| g.stream_count),
        EXPECTED_STREAM_COUNT,
        "usage panel: stream_count"
    );
    assert_eq!(
        result.totals.cache_write_5m, EXPECTED_CACHE_WRITE_5M,
        "status query: cache_write_5m"
    );
    assert_eq!(
        result.totals.cache_write_1h, EXPECTED_CACHE_WRITE_1H,
        "status query: cache_write_1h"
    );
    assert_eq!(
        result.totals.stream_count, EXPECTED_STREAM_COUNT,
        "status query: stream_count"
    );
}

#[test]
fn every_metric_token_is_shared_or_declared_one_sided() {
    // The durable half of the guard: adding a metric to EITHER surface fails
    // here until it is declared shared (and therefore asserted equal above) or
    // explicitly one-sided. The day a derived metric such as `ttft_p50_ms`
    // gains a panel counterpart, this is what forces the population-filter
    // question to be answered -- at the moment it is answerable.
    let (_dir, panel, result) = both_folds();
    let shared: BTreeSet<String> = SHARED.iter().map(|s| (*s).to_string()).collect();
    let query_only: BTreeSet<String> = QUERY_ONLY.iter().map(|s| (*s).to_string()).collect();
    let panel_only: BTreeSet<String> = PANEL_ONLY.iter().map(|s| (*s).to_string()).collect();

    // The three vocabularies must be PAIRWISE disjoint, checked BEFORE the key
    // comparisons so a mis-declared token reports as the vocabulary error it
    // is rather than as a confusing key-set mismatch. The two one-sided lists
    // matter as much as SHARED: a metric named in BOTH of them satisfies each
    // key assertion below while never entering SHARED, so it would never be
    // compared for equivalence -- exactly the silent drift this pin exists to
    // catch.
    assert!(
        shared.is_disjoint(&query_only),
        "SHARED must be disjoint from QUERY_ONLY"
    );
    assert!(
        shared.is_disjoint(&panel_only),
        "SHARED must be disjoint from PANEL_ONLY"
    );
    assert!(
        query_only.is_disjoint(&panel_only),
        "QUERY_ONLY must be disjoint from PANEL_ONLY -- a token in both is \
         asserted by neither key check and never compared for equivalence"
    );

    let query_keys =
        keys_of(&serde_json::to_value(&result.totals).expect("serialize QueryMetrics"));
    assert_eq!(
        query_keys,
        shared.union(&query_only).cloned().collect::<BTreeSet<_>>(),
        "a QueryMetrics token is neither SHARED nor QUERY_ONLY"
    );

    // The panel's metric surface is its totals plus its rollup groups; the
    // envelope fields (window, bounds, quota, would_trim) are not part of the
    // per-request metric vocabulary this guard governs.
    let panel_json = serde_json::to_value(&panel).expect("serialize UsagePanel");
    let mut panel_keys = keys_of(&panel_json["totals"]);
    let groups = panel_json["groups"]
        .as_array()
        .expect("panel groups serialize as an array");
    assert!(
        !groups.is_empty(),
        "the fixture must produce at least one rollup group"
    );
    for group in groups {
        panel_keys.extend(keys_of(group));
    }
    assert_eq!(
        panel_keys,
        shared.union(&panel_only).cloned().collect::<BTreeSet<_>>(),
        "a usage-panel metric field is neither SHARED nor PANEL_ONLY"
    );
}
