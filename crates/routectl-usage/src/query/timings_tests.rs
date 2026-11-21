//! Exact timing percentiles: the nearest-rank rule, the populations, the
//! display labels, the buckets, the empty sets, the order property, and
//! independence from how the window is cut.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::*;
use crate::db::{UsageDb, open};
use crate::query::grouped::{BucketSpec, QueryMetrics, QueryResult, QuerySpec, query_with};
use crate::query::slices::ReadPlan;
use crate::query::{AggRow, RowCost};

fn no_deadline() -> ReadLimit {
    ReadLimit::from(Instant::now() + Duration::from_mins(10))
}

fn sorted_rank(values: &[i64], percent: usize) -> Option<i64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    (n > 0).then(|| sorted[((percent * n).div_ceil(100)).max(1) - 1])
}

/// A deterministic heavy-tailed draw with many ties.
fn spread(seed: u64, n: usize) -> Vec<i64> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let draw = (state % 1_000) as i64;
            if draw < 950 { draw } else { draw * draw * 10 }
        })
        .collect()
}

#[test]
fn nearest_ranks_match_a_full_sort_at_every_size() {
    for n in (0..=210).chain([999, 1_000, 1_001, 4_321]) {
        // Arrange
        let values = spread(n as u64 + 7, n);
        let expected = [50, 90, 99].map(|p| sorted_rank(&values, p));

        // Act
        let got = nearest_ranks(&mut values.clone());

        // Assert
        assert_eq!(got, expected, "n = {n}");
    }
}

#[test]
fn nearest_ranks_on_a_known_set() {
    // Arrange: 1..=10, so rank ceil(q * 10).
    let mut values: Vec<i64> = (1..=10).rev().collect();

    // Act + Assert
    assert_eq!(nearest_ranks(&mut values), [Some(5), Some(9), Some(10)]);
}

fn open_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open");
    (dir, db)
}

/// One row of the timing fixture.
struct Row {
    ts: i64,
    provider: Option<&'static str>,
    seat: Option<&'static str>,
    model: Option<&'static str>,
    stream: i64,
    outcome: &'static str,
    ttfb: Option<i64>,
    latency: i64,
    operation: Option<&'static str>,
}

const BASE: Row = Row {
    ts: 0,
    provider: Some("pa"),
    seat: None,
    model: Some("m1"),
    stream: 1,
    outcome: "ok",
    ttfb: Some(100),
    latency: 1_000,
    operation: None,
};

fn insert(db: &UsageDb, id: usize, row: &Row) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, fallback_count, \
             operation, seat) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', ?3, ?4, 'u', ?5, ?6, ?7, ?8, \
             0, 0, 1, 0, ?9, ?10)",
            rusqlite::params![
                row.ts,
                format!("r{id}"),
                row.model,
                row.provider,
                row.stream,
                row.outcome,
                row.latency,
                row.ttfb,
                row.operation,
                row.seat,
            ],
        )
        .expect("insert");
}

fn spec(group_by: GroupDim, bucket: Option<BucketSpec>) -> QuerySpec {
    QuerySpec {
        from_ms: 0,
        to_ms: 40_000,
        group_by,
        alias_filter: None,
        provider_filter: None,
        model_filter: None,
        bucket,
        entry_accounts: Default::default(),
    }
}

fn unpriced(_: &AggRow) -> RowCost {
    RowCost::Unpriced(Vec::new())
}

fn timed(db: &mut UsageDb, spec: &QuerySpec, slices: usize) -> QueryResult {
    let plan = ReadPlan {
        slices: Some(slices),
        watermark: None,
        open_slice: None,
    };
    query_with(db, spec, unpriced, no_deadline(), &plan).expect("query")
}

/// The six timing percentiles a metric set carries.
fn ranks(m: &QueryMetrics) -> Percentiles {
    Percentiles {
        ttft_p50_ms: m.ttft_p50_ms,
        ttft_p90_ms: m.ttft_p90_ms,
        ttft_p99_ms: m.ttft_p99_ms,
        latency_p50_ms: m.latency_p50_ms,
        latency_p90_ms: m.latency_p90_ms,
        latency_p99_ms: m.latency_p99_ms,
    }
}

/// Every metric set of `result`, named: each group, the totals, each bucket.
fn metric_sets(result: &QueryResult) -> Vec<(String, &QueryMetrics)> {
    let mut sets: Vec<(String, &QueryMetrics)> = result
        .groups
        .iter()
        .map(|g| (format!("group {}", g.label), &g.metrics))
        .collect();
    sets.push(("totals".to_string(), &result.totals));
    if let Some(series) = &result.series {
        for (i, bucket) in series.buckets.iter().enumerate() {
            sets.push((format!("bucket {i}"), &bucket.metrics));
        }
    }
    sets
}

#[test]
fn ttft_counts_only_streaming_successes_and_latency_every_inference_row() {
    // Arrange: one eligible row, then rows each failing one TTFT condition
    // with a huge TTFT, and a token-count row with a huge latency.
    let (_dir, mut db) = open_db();
    insert(&db, 0, &BASE);
    let excluded = [
        Row {
            stream: 0,
            ttfb: Some(9_000),
            latency: 2_000,
            ..BASE
        },
        Row {
            outcome: "upstream_error",
            ttfb: Some(9_000),
            latency: 3_000,
            ..BASE
        },
        Row {
            ttfb: None,
            latency: 4_000,
            ..BASE
        },
    ];
    for (i, row) in excluded.iter().enumerate() {
        insert(&db, i + 1, row);
    }
    insert(
        &db,
        9,
        &Row {
            operation: Some("count_tokens"),
            latency: 99_000,
            ttfb: Some(99_000),
            ..BASE
        },
    );

    // Act
    let result = timed(&mut db, &spec(GroupDim::Provider, None), 1);

    // Assert
    let t = &result.totals;
    assert_eq!((t.ttft_p50_ms, t.ttft_p99_ms), (Some(100), Some(100)));
    assert_eq!(
        t.latency_p50_ms,
        Some(2_000),
        "four inference latencies 1k..4k"
    );
    assert_eq!(t.latency_p99_ms, Some(4_000), "the token-count row is out");
    assert_eq!(t.requests, 4, "latency counts the latency_avg_ms divisor");
    assert_eq!(ranks(&result.groups[0].metrics), ranks(t));
}

/// Entry -> account for the own-credential entry `pa`, as the caller derives
/// it from config.
fn entry_accounts() -> BTreeMap<String, String> {
    BTreeMap::from([("pa".to_string(), "#pa-key".to_string())])
}

#[test]
fn groups_are_labelled_as_the_aggregate_labels_them() {
    // Arrange: a NULL-seat row on the mapped entry `pa`, a NULL provider and
    // served model, and a persisted seat on `pa`; every grouping reads with
    // the entry map set, which only the seat grouping may consult.
    let (_dir, mut db) = open_db();
    insert(&db, 0, &BASE);
    insert(
        &db,
        1,
        &Row {
            provider: None,
            model: None,
            latency: 7_000,
            ..BASE
        },
    );
    insert(
        &db,
        2,
        &Row {
            seat: Some("persisted"),
            latency: 3_000,
            ..BASE
        },
    );

    for (group_by, labels) in [
        (
            GroupDim::Provider,
            vec![(UNATTRIBUTED, 7_000), ("pa", 3_000)],
        ),
        (GroupDim::Model, vec![("m1", 3_000), ("req-model", 7_000)]),
        (GroupDim::Alias, vec![("al", 7_000)]),
        (
            GroupDim::Seat,
            vec![
                ("#pa-key", 1_000),
                (UNATTRIBUTED, 7_000),
                ("persisted", 3_000),
            ],
        ),
    ] {
        // Act
        let spec = QuerySpec {
            entry_accounts: entry_accounts(),
            ..spec(group_by, None)
        };
        let result = timed(&mut db, &spec, 1);
        let mut read_labels = statement_labels(&db, group_by, &spec.entry_accounts);
        read_labels.sort();

        // Assert: the timing statement reads the aggregate's labels, and each
        // group carries the figures of its own rows.
        let got: Vec<(&str, Option<i64>)> = result
            .groups
            .iter()
            .map(|g| (g.label.as_str(), g.metrics.latency_p99_ms))
            .collect();
        let want: Vec<(&str, Option<i64>)> = labels
            .iter()
            .map(|(label, latency)| (*label, Some(*latency)))
            .collect();
        assert_eq!(got, want, "{group_by:?}");
        let aggregate_labels: Vec<&str> = result.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(read_labels, aggregate_labels, "{group_by:?}");
    }
}

/// The labels the timing statement of `group_by` reads, deduplicated.
fn statement_labels(
    db: &UsageDb,
    group_by: GroupDim,
    entry_accounts: &BTreeMap<String, String>,
) -> Vec<String> {
    let tx = db.conn().unchecked_transaction().expect("begin read");
    let window = ReadWindow {
        from_ms: 0,
        to_ms: 40_000,
        alias_filter: None,
        provider_filter: None,
        model_filter: None,
        watermark: i64::MAX,
        anchor_ms: 0,
    };
    let samples =
        read_timings(&tx, &window, group_by, entry_accounts, None, &no_deadline()).expect("read");
    samples.by_label.into_keys().collect()
}

/// Sixty rows over four providers and the whole window, with spread values.
fn seed_dense(db: &UsageDb) {
    let values = spread(3, 120);
    let providers = [Some("pa"), Some("pb"), Some("pc"), None];
    for i in 0..60 {
        let row = Row {
            ts: (i as i64) * 40_000 / 60,
            provider: providers[i % 4],
            stream: i64::from(i % 7 != 0),
            ttfb: (i % 11 != 0).then_some(values[i]),
            latency: values[60 + i] + 1,
            ..BASE
        };
        insert(db, i, &row);
    }
}

#[test]
fn the_figures_do_not_depend_on_how_the_window_is_sliced() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_dense(&db);
    let bucketed = spec(
        GroupDim::Provider,
        Some(BucketSpec {
            width_ms: 10_000,
            count: 4,
        }),
    );

    for spec in [spec(GroupDim::Provider, None), bucketed] {
        // Act
        let one = timed(&mut db, &spec, 1);
        let four = timed(&mut db, &spec, 4);

        // Assert
        let one_ranks: Vec<_> = metric_sets(&one)
            .into_iter()
            .map(|(at, m)| (at, ranks(m)))
            .collect();
        let four_ranks: Vec<_> = metric_sets(&four)
            .into_iter()
            .map(|(at, m)| (at, ranks(m)))
            .collect();
        assert_eq!(one_ranks, four_ranks);
        assert_eq!(one.groups.len(), 4);
        assert!(
            one_ranks
                .iter()
                .all(|(_, r)| r.latency_p50_ms.is_some() || r == &Percentiles::default()),
            "positive control: every populated set carries figures"
        );
    }
}

/// `(ts_start, provider label, eligible TTFT, latency)` read straight back.
type LedgerRow = (i64, String, Option<i64>, i64);

#[test]
fn each_bucket_and_group_is_its_own_rows_percentiles() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_dense(&db);
    let spec = spec(
        GroupDim::Provider,
        Some(BucketSpec {
            width_ms: 10_000,
            count: 5,
        }),
    );
    let rows: Vec<LedgerRow> = db
        .conn()
        .prepare(
            "SELECT ts_start, COALESCE(provider, '(unattributed)'), \
             CASE WHEN stream = 1 AND outcome = 'ok' THEN ttfb_ms END, latency_ms FROM requests",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let expect = |keep: &dyn Fn(&LedgerRow) -> bool| {
        let ttft: Vec<i64> = rows
            .iter()
            .filter(|r| keep(r))
            .filter_map(|r| r.2)
            .collect();
        let latency: Vec<i64> = rows.iter().filter(|r| keep(r)).map(|r| r.3).collect();
        Percentiles {
            ttft_p50_ms: sorted_rank(&ttft, 50),
            ttft_p90_ms: sorted_rank(&ttft, 90),
            ttft_p99_ms: sorted_rank(&ttft, 99),
            latency_p50_ms: sorted_rank(&latency, 50),
            latency_p90_ms: sorted_rank(&latency, 90),
            latency_p99_ms: sorted_rank(&latency, 99),
        }
    };

    // Act
    let result = timed(&mut db, &spec, 4);

    // Assert
    let buckets = &result.series.as_ref().expect("series").buckets;
    assert_eq!(buckets.len(), 5);
    for (index, bucket) in buckets.iter().enumerate() {
        let index = index as i64;
        assert_eq!(
            ranks(&bucket.metrics),
            expect(&|r| r.0 / 10_000 == index),
            "bucket {index}"
        );
    }
    assert_eq!(
        ranks(&buckets[4].metrics),
        Percentiles::default(),
        "empty bucket"
    );
    for group in &result.groups {
        let label = &group.label;
        assert_eq!(
            ranks(&group.metrics),
            expect(&|r| &r.1 == label),
            "group {label}"
        );
    }
    assert_eq!(ranks(&result.totals), expect(&|_| true));
}

#[test]
fn every_timing_statement_is_answered_from_the_covering_index() {
    // Arrange
    let (_dir, db) = open_db();

    for group_by in GroupDim::ALL {
        let (unbucketed, bucketed) = statements(group_by);
        for sql in <[&str; 2]>::from((unbucketed, bucketed)) {
            // Act
            let mut stmt = db
                .conn()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("explain");
            let count = stmt.parameter_count();
            let params: Vec<i64> = vec![1; count];
            let plan: Vec<String> = stmt
                .query_map(rusqlite::params_from_iter(params), |r| r.get(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();

            // Assert
            assert!(
                plan.iter()
                    .any(|d| d.contains("USING COVERING INDEX idx_requests_agg ")),
                "{group_by:?}: {plan:?}"
            );
        }
    }
}

#[test]
fn an_empty_window_reports_every_percentile_absent() {
    // Arrange: one row outside the window, so the ledger is not empty but
    // the window is; and a window whose only row is non-streaming, so TTFT
    // alone is empty.
    let (_dir, mut db) = open_db();
    insert(&db, 0, &Row { ts: 50_000, ..BASE });
    insert(
        &db,
        1,
        &Row {
            ts: 100,
            stream: 0,
            ..BASE
        },
    );
    let outside = QuerySpec {
        from_ms: 60_000,
        to_ms: 70_000,
        ..spec(GroupDim::Model, None)
    };

    // Act
    let empty = timed(&mut db, &outside, 1);
    let no_ttft = timed(&mut db, &spec(GroupDim::Model, None), 1);

    // Assert
    assert!(empty.groups.is_empty());
    assert_eq!(ranks(&empty.totals), Percentiles::default());
    let t = &no_ttft.totals;
    assert_eq!(
        (t.ttft_p50_ms, t.ttft_p90_ms, t.ttft_p99_ms),
        (None, None, None)
    );
    assert_eq!(t.latency_p50_ms, Some(1_000), "positive control");
}

/// Assert `p50 <= p90 <= p99 <= max` for one population of one set, and
/// that the figures are present exactly when the max is.
fn assert_ordered(at: &str, ranks: [Option<i64>; 3], max: Option<i64>) {
    assert_eq!(ranks[0].is_some(), max.is_some(), "{at}: presence");
    if let ([Some(p50), Some(p90), Some(p99)], Some(max)) = (ranks, max) {
        assert!(
            p50 <= p90 && p90 <= p99 && p99 <= max,
            "{at}: {p50} <= {p90} <= {p99} <= {max}"
        );
    }
}

#[test]
fn every_metric_set_orders_its_percentiles_below_its_max() {
    for seed in 0..40_u64 {
        // Arrange: a seeded random ledger of 1 to 80 rows over the window,
        // with heavy-tailed values, mixed eligibility and four providers.
        let (_dir, mut db) = open_db();
        let n = (seed as usize * 7) % 80 + 1;
        let values = spread(seed + 11, 2 * n);
        let providers = [Some("pa"), Some("pb"), Some("pc"), None];
        for i in 0..n {
            let ttfb = values[i];
            let row = Row {
                ts: (values[n + i] * 37 + i as i64) % 40_000,
                provider: providers[(values[i] as usize + i) % 4],
                stream: i64::from(values[n + i] % 5 != 0),
                outcome: if values[i] % 9 == 0 {
                    "upstream_error"
                } else {
                    "ok"
                },
                ttfb: (values[n + i] % 13 != 0).then_some(ttfb),
                latency: ttfb + values[n + i],
                ..BASE
            };
            insert(&db, i, &row);
        }
        let bucketed = spec(
            GroupDim::Provider,
            Some(BucketSpec {
                width_ms: 4_000,
                count: 10,
            }),
        );

        // Act
        let result = timed(&mut db, &bucketed, 4);

        // Assert
        assert_eq!(result.totals.requests, n as i64, "seed {seed}: every row");
        for (at, m) in metric_sets(&result) {
            let at = format!("seed {seed} {at}");
            assert_ordered(
                &format!("{at} ttft"),
                [m.ttft_p50_ms, m.ttft_p90_ms, m.ttft_p99_ms],
                m.ttft_max_ms,
            );
            assert_ordered(
                &format!("{at} latency"),
                [m.latency_p50_ms, m.latency_p90_ms, m.latency_p99_ms],
                m.latency_max_ms,
            );
        }
    }
}
