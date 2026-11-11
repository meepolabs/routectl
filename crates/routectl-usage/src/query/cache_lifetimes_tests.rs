//! Tests for the both-era cache-write total and the per-lifetime write sums.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::*;
use crate::db::open;
use crate::query::{BucketSpec, GroupDim, QuerySpec, RowCost, aggregate, query};

/// One ledger row's cache-write columns plus the few keys a test varies.
#[derive(Clone, Copy)]
struct Writes {
    ts_start: i64,
    provider: &'static str,
    legacy_5m: Option<i64>,
    legacy_1h: Option<i64>,
    total: Option<i64>,
    split: Option<&'static str>,
    input: Option<i64>,
    cache_read: Option<i64>,
}

impl Default for Writes {
    fn default() -> Self {
        Self {
            ts_start: 100,
            provider: "pa",
            legacy_5m: None,
            legacy_1h: None,
            total: None,
            split: None,
            input: None,
            cache_read: None,
        }
    }
}

fn open_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open");
    (dir, db)
}

fn seed(db: &UsageDb, rows: &[Writes]) {
    for (index, w) in rows.iter().enumerate() {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, cache_read, cache_write_5m, cache_write_1h, \
                 cache_write_tokens, cache_write_lifetimes) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', 'm1', ?3, 'u1', 0, \
                 'ok', 10, 0, 0, 1, 0, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    w.ts_start,
                    format!("r{index}"),
                    w.provider,
                    w.input,
                    w.cache_read,
                    w.legacy_5m,
                    w.legacy_1h,
                    w.total,
                    w.split,
                ],
            )
            .expect("insert row");
    }
}

fn only_row(rows: &[AggRow]) -> &AggRow {
    assert_eq!(rows.len(), 1, "the fixture seeds one fine group");
    &rows[0]
}

fn lifetimes_of(row: &AggRow) -> &CacheWriteLifetimes {
    row.cache_write_lifetimes
        .as_ref()
        .expect("the priced read attaches lifetimes")
}

fn no_deadline() -> Instant {
    Instant::now() + Duration::from_mins(10)
}

fn spec(bucket: Option<BucketSpec>) -> QuerySpec {
    QuerySpec {
        from_ms: 0,
        to_ms: 10_000,
        group_by: GroupDim::Provider,
        alias_filter: None,
        provider_filter: None,
        bucket,
    }
}

fn legacy(legacy_5m: Option<i64>, legacy_1h: Option<i64>) -> Writes {
    Writes {
        legacy_5m,
        legacy_1h,
        ..Writes::default()
    }
}

fn new_era(total: i64, split: Option<&'static str>) -> Writes {
    Writes {
        total: Some(total),
        split,
        ..Writes::default()
    }
}

#[test]
fn mixed_era_total_sums_both_eras_and_counts_zero_but_not_absent() {
    // Arrange: every write shape the ledger holds, in one fine group.
    let (_dir, db) = open_db();
    seed(
        &db,
        &[
            legacy(Some(10), None),
            legacy(None, Some(20)),
            legacy(Some(3), Some(4)),
            legacy(Some(0), None),
            new_era(100, Some(r#"{"300":60,"3600":40}"#)),
            new_era(50, None),
            new_era(0, None),
            Writes::default(),
            Writes::default(),
        ],
    );

    // Act
    let row = only_row(&aggregate(&db, 0, 10_000).expect("aggregate")).clone();

    // Assert: legacy sums 10 + 20 + 7 + 0 plus new totals 100 + 50 + 0.
    assert_eq!(row.requests, 9);
    assert_eq!(row.cache_write_tokens, 187);
    // Seven reporting rows, two of them observed zeros; the two absent rows
    // are not counted.
    assert_eq!(row.cache_write_present, 7);
    // The legacy columns still describe the pre-total rows alone.
    assert_eq!(row.cache_write_5m, 13);
    assert_eq!(row.cache_write_1h, 24);
}

#[test]
fn total_expression_single_source_rows() {
    // (name, row, expected total, expected present)
    let cases = [
        ("new total", new_era(7, None), 7, 1),
        ("both legacy null and no total", Writes::default(), 0, 0),
        ("legacy 5m only", legacy(Some(3), None), 3, 1),
        ("legacy 1h only", legacy(None, Some(5)), 5, 1),
    ];
    for (name, writes, total, present) in cases {
        // Arrange
        let (_dir, db) = open_db();
        seed(&db, &[writes]);

        // Act
        let rows = aggregate(&db, 0, 10_000).expect("aggregate");

        // Assert
        let row = only_row(&rows);
        assert_eq!(row.cache_write_tokens, total, "{name}: total");
        assert_eq!(row.cache_write_present, present, "{name}: present");
    }
}

#[test]
fn hit_fraction_denominator_includes_new_era_write_total() {
    // Arrange: no uncached input, 1000 read from cache and 1000 written.
    let (_dir, db) = open_db();
    seed(
        &db,
        &[Writes {
            input: Some(0),
            cache_read: Some(1000),
            total: Some(1000),
            ..Writes::default()
        }],
    );

    // Act
    let result = query(&db, &spec(None), |_| RowCost::Unpriced, no_deadline()).expect("query");

    // Assert
    let pct = result.totals.cache_hit_pct.expect("hit fraction present");
    assert!((pct - 50.0).abs() < 1e-9, "expected 50%, got {pct}");
}

/// One per-lifetime row: its name, the seeded rows, and the expected
/// lifetime sums, unknown remainder and invalid flag.
type LifetimeCase = (&'static str, Vec<Writes>, Vec<(u32, i64)>, i64, bool);

#[test]
fn per_lifetime_sums_single_source_rows() {
    // (name, rows, expected lifetimes, expected unknown, expected invalid)
    let cases: [LifetimeCase; 7] = [
        (
            "legacy columns merge as 300 and 3600",
            vec![legacy(Some(10), Some(20)), legacy(Some(5), None)],
            vec![(300, 15), (3600, 20)],
            0,
            false,
        ),
        (
            "json split summed per lifetime",
            vec![
                new_era(100, Some(r#"{"300":60,"1800":40}"#)),
                new_era(50, Some(r#"{"1800":50}"#)),
            ],
            vec![(300, 60), (1800, 90)],
            0,
            false,
        ),
        (
            "unknown remainder is total minus split sum over new rows",
            vec![
                new_era(100, Some(r#"{"300":70}"#)),
                new_era(40, None),
                new_era(0, None),
            ],
            vec![(300, 70)],
            70,
            false,
        ),
        (
            "legacy and new eras merge into one lifetime",
            vec![legacy(Some(10), None), new_era(30, Some(r#"{"300":25}"#))],
            vec![(300, 35)],
            5,
            false,
        ),
        (
            "a reported zero lifetime is kept",
            vec![new_era(9, Some(r#"{"300":9,"3600":0}"#))],
            vec![(300, 9), (3600, 0)],
            0,
            false,
        ),
        (
            "unusable split entries fall into the unknown remainder",
            vec![new_era(
                100,
                Some(r#"{"300":10,"abc":20,"0":5,"0300":5,"60":"7","120":-3}"#),
            )],
            vec![(300, 10)],
            90,
            false,
        ),
        (
            "a split above its total flags the group invalid",
            vec![
                new_era(10, Some(r#"{"300":20}"#)),
                new_era(50, Some(r#"{"300":20}"#)),
            ],
            vec![(300, 40)],
            30,
            true,
        ),
    ];
    for (name, rows, expected, unknown, invalid) in cases {
        // Arrange
        let (_dir, db) = open_db();
        seed(&db, &rows);

        // Act
        let read = aggregate_with_cache_lifetimes(&db, 0, 10_000).expect("aggregate");

        // Assert
        let got = lifetimes_of(only_row(&read));
        let expected: BTreeMap<u32, i64> = expected.into_iter().collect();
        assert_eq!(got.tokens_by_lifetime, expected, "{name}: lifetimes");
        assert_eq!(got.unknown_tokens, unknown, "{name}: unknown");
        assert_eq!(got.invalid_split, invalid, "{name}: invalid");
    }
}

#[test]
fn invalid_split_flags_only_its_own_group() {
    // Arrange: one over-full split under provider pa, a valid one under pb.
    let (_dir, db) = open_db();
    seed(
        &db,
        &[
            Writes {
                provider: "pa",
                ..new_era(10, Some(r#"{"300":20}"#))
            },
            Writes {
                provider: "pb",
                ..new_era(20, Some(r#"{"300":20}"#))
            },
        ],
    );

    // Act
    let rows = aggregate_with_cache_lifetimes(&db, 0, 10_000).expect("aggregate");

    // Assert
    let by_provider = |p: &str| {
        lifetimes_of(
            rows.iter()
                .find(|r| r.key.provider.as_deref() == Some(p))
                .expect("group present"),
        )
        .clone()
    };
    assert!(by_provider("pa").invalid_split);
    assert!(!by_provider("pb").invalid_split);
}

#[test]
fn group_without_writes_gets_an_empty_lifetime_set() {
    // Arrange
    let (_dir, db) = open_db();
    seed(&db, &[Writes::default()]);

    // Act
    let rows = aggregate_with_cache_lifetimes(&db, 0, 10_000).expect("aggregate");

    // Assert
    assert_eq!(
        lifetimes_of(only_row(&rows)),
        &CacheWriteLifetimes::default()
    );
}

/// Every fine row the pricing closure saw, with its lifetimes.
fn priced_rows(db: &UsageDb, bucket: Option<BucketSpec>) -> Vec<AggRow> {
    let seen = RefCell::new(Vec::new());
    query(
        db,
        &spec(bucket),
        |row| {
            seen.borrow_mut().push(row.clone());
            RowCost::Unpriced
        },
        no_deadline(),
    )
    .expect("query");
    seen.into_inner()
}

/// Sum lifetimes across `rows` per provider.
fn per_provider(rows: &[AggRow]) -> BTreeMap<String, CacheWriteLifetimes> {
    let mut out: BTreeMap<String, CacheWriteLifetimes> = BTreeMap::new();
    for row in rows {
        let acc = out
            .entry(row.key.provider.clone().unwrap_or_default())
            .or_default();
        let lifetimes = lifetimes_of(row);
        for (seconds, tokens) in &lifetimes.tokens_by_lifetime {
            acc.add_lifetime(*seconds, *tokens);
        }
        acc.unknown_tokens += lifetimes.unknown_tokens;
        acc.invalid_split |= lifetimes.invalid_split;
    }
    out
}

fn spread_fixture() -> Vec<Writes> {
    vec![
        Writes {
            ts_start: 100,
            ..legacy(Some(10), Some(1))
        },
        Writes {
            ts_start: 1_500,
            ..new_era(100, Some(r#"{"300":60,"1800":30}"#))
        },
        Writes {
            ts_start: 2_500,
            ..new_era(40, Some(r#"{"1800":40}"#))
        },
        Writes {
            ts_start: 2_600,
            ..new_era(25, None)
        },
        Writes {
            ts_start: 3_100,
            provider: "pb",
            ..new_era(8, Some(r#"{"3600":8}"#))
        },
        Writes {
            ts_start: 3_900,
            provider: "pb",
            ..legacy(Some(2), None)
        },
    ]
}

#[test]
fn query_attaches_lifetimes_to_every_priced_row() {
    // Arrange
    let (_dir, db) = open_db();
    seed(&db, &spread_fixture());

    // Act
    let rows = priced_rows(&db, None);

    // Assert: the hand-computed per-provider sums.
    let sums = per_provider(&rows);
    assert_eq!(
        sums["pa"].tokens_by_lifetime,
        BTreeMap::from([(300, 70), (1800, 70), (3600, 1)])
    );
    assert_eq!(sums["pa"].unknown_tokens, 35);
    assert_eq!(
        sums["pb"].tokens_by_lifetime,
        BTreeMap::from([(300, 2), (3600, 8)])
    );
    assert_eq!(sums["pb"].unknown_tokens, 0);
}

#[test]
fn series_bucket_lifetime_sums_reconcile_to_the_group_sums() {
    // Arrange: rows spread across four 1000 ms buckets and two groups.
    let (_dir, db) = open_db();
    seed(&db, &spread_fixture());

    // Act
    let unbucketed = priced_rows(&db, None);
    let bucketed = priced_rows(
        &db,
        Some(BucketSpec {
            width_ms: 1_000,
            count: 10,
        }),
    );

    // Assert: the bucketed path prices each row per bucket, and those
    // per-bucket lifetimes sum to the unbucketed group sums.
    assert!(
        bucketed.len() > unbucketed.len(),
        "the fixture must split a group across buckets"
    );
    assert_eq!(per_provider(&bucketed), per_provider(&unbucketed));
}

#[test]
fn aggregate_reads_no_lifetime_split() {
    // Arrange: a row whose split the priced read would report.
    let (_dir, db) = open_db();
    seed(&db, &[new_era(10, Some(r#"{"300":10}"#))]);

    // Act
    let rows = aggregate(&db, 0, 10_000).expect("aggregate");

    // Assert: the unpriced read neither attaches a split nor names the split
    // column in its statement.
    assert_eq!(only_row(&rows).cache_write_lifetimes, None);
    assert!(!super::super::aggregate::AGG_SQL.contains("cache_write_lifetimes"));
}
