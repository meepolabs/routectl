//! Every statement of the grouped read takes its window, filters and
//! watermark from the one shared predicate, and the side reads it feeds agree
//! with the aggregate under each filter.

use std::collections::{BTreeMap, HashMap};

use tempfile::TempDir;

use super::aggregate::{
    ERRORS_BY_CLASS_BUCKETED_SQL, ERRORS_BY_CLASS_SQL, QUERY_AGG_SQL, ReadWindow, SERIES_AGG_SQL,
    read_error_classes, read_fine_rows,
};
use super::cache_lifetimes::{CACHE_LIFETIMES_BUCKETED_SQL, CACHE_LIFETIMES_SQL};
use super::deadline::ReadLimit;
use super::grouped::GroupDim;
use super::timings::statements;
use super::{GroupKey, UsageDb};

const READ_FILTERS: &str = read_filters!();

/// The optional-filter shape every bind-param filter of the predicate takes.
const OPTIONAL_FILTER: &str = "IS NULL OR";

/// Every statement the grouped read prepares: the aggregate and the error
/// classes and cache-write lifetimes beside it, each unbucketed and bucketed,
/// and the timing read's pair per grouping.
fn grouped_read_statements() -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = [
        ("aggregate", QUERY_AGG_SQL),
        ("aggregate bucketed", SERIES_AGG_SQL),
        ("error classes", ERRORS_BY_CLASS_SQL),
        ("error classes bucketed", ERRORS_BY_CLASS_BUCKETED_SQL),
        ("cache lifetimes", CACHE_LIFETIMES_SQL),
        ("cache lifetimes bucketed", CACHE_LIFETIMES_BUCKETED_SQL),
    ]
    .into_iter()
    .map(|(name, sql)| (name.to_string(), sql))
    .collect();
    for group_by in GroupDim::ALL {
        let (unbucketed, bucketed) = statements(group_by);
        out.push((format!("timings by {group_by:?}"), unbucketed));
        out.push((format!("timings by {group_by:?} bucketed"), bucketed));
    }
    out
}

#[test]
fn every_grouped_read_statement_takes_the_shared_predicate_and_no_other_filter() {
    // Arrange
    let statements = grouped_read_statements();
    let filters_in_predicate = READ_FILTERS.matches(OPTIONAL_FILTER).count();

    // Assert: the census holds the six aggregate-side statements and the
    // eight timing statements, and the predicate carries the filters and the
    // inference-row predicate (the source scan of every requests reader
    // relies on that).
    assert_eq!(statements.len(), 14, "statement census");
    assert_eq!(filters_in_predicate, 2, "alias and provider filters");
    assert!(
        READ_FILTERS.contains(inference_rows!()),
        "the shared predicate carries the inference-row predicate"
    );
    for (name, sql) in statements {
        assert_eq!(
            sql.matches(READ_FILTERS).count(),
            1,
            "{name} must take the shared predicate exactly once: {sql}"
        );
        assert_eq!(
            sql.matches(OPTIONAL_FILTER).count(),
            filters_in_predicate,
            "{name} filters outside the shared predicate: {sql}"
        );
    }
}

fn open_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = crate::db::open(dir.path().join("usage.db")).expect("open");
    (dir, db)
}

/// Rows over two aliases and two providers, with error rows of several
/// classes (one unclassified), a success, and outcomes a breakdown must not
/// count, spread over two buckets of 1000 ms.
fn seed(db: &UsageDb) {
    let rows = [
        ("e1", 100, "al", "pa", "upstream_error", Some("http-5xx")),
        ("e2", 1500, "al", "pa", "upstream_error", None),
        ("ok", 1600, "al", "pa", "ok", None),
        ("e3", 200, "bl", "pb", "upstream_error", Some("timeout")),
        ("e4", 1200, "bl", "pa", "gate_blocked", Some("rate-limited")),
        ("e5", 1700, "al", "pb", "upstream_error", Some("timeout")),
        ("cd", 300, "al", "pa", "client_disconnect", Some("timeout")),
        ("cx", 400, "bl", "pb", "cancelled", Some("timeout")),
    ];
    for (request_id, ts_start, alias, provider, outcome, class) in rows {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 resolved_class) \
                 VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, 'm1', ?4, 'u1', 0, ?5, \
                 10, 0, 0, 1, 0, ?6)",
                rusqlite::params![ts_start, request_id, alias, provider, outcome, class],
            )
            .expect("insert row");
    }
}

/// Errors per fine row, and the classes read beside them, of one read.
type ErrorsAndClasses = (
    HashMap<(GroupKey, i64), i64>,
    HashMap<(GroupKey, i64), BTreeMap<String, i64>>,
);

fn read_errors_and_classes(
    db: &UsageDb,
    window: &ReadWindow<'_>,
    bucket_width_ms: Option<i64>,
) -> ErrorsAndClasses {
    let tx = db.conn().unchecked_transaction().expect("begin read");
    let limit = ReadLimit::unbounded();
    let errors = read_fine_rows(&tx, window, bucket_width_ms, &limit)
        .expect("fine rows")
        .into_iter()
        .map(|(row, bucket)| ((row.agg.key, bucket), row.agg.errors))
        .collect();
    let classes = read_error_classes(&tx, window, bucket_width_ms, &limit).expect("classes");
    (errors, classes)
}

#[test]
fn error_classes_sum_to_errors_on_every_fine_row_under_each_filter() {
    // Arrange
    let (_dir, db) = open_db();
    seed(&db);
    let window = |alias_filter, provider_filter| ReadWindow {
        from_ms: 0,
        to_ms: 2000,
        alias_filter,
        provider_filter,
        watermark: i64::MAX,
        anchor_ms: 0,
    };
    // (case, alias filter, provider filter, total errors the filter keeps)
    let cases = [
        ("unfiltered", None, None, 5),
        ("alias filter", Some("al"), None, 3),
        ("provider filter", None, Some("pa"), 3),
    ];

    for (case, alias_filter, provider_filter, expected_errors) in cases {
        for bucket_width_ms in [None, Some(1000)] {
            // Act
            let (errors, classes) = read_errors_and_classes(
                &db,
                &window(alias_filter, provider_filter),
                bucket_width_ms,
            );

            // Assert: the filter kept exactly its error rows, every class row
            // belongs to a fine row, and each fine row's classes sum to its
            // errors.
            let at = format!("{case}, buckets {bucket_width_ms:?}");
            assert_eq!(
                errors.values().sum::<i64>(),
                expected_errors,
                "{at}: errors"
            );
            for key in classes.keys() {
                assert!(
                    errors.contains_key(key),
                    "{at}: classes of no fine row {key:?}"
                );
            }
            for (key, &row_errors) in &errors {
                let class_sum: i64 = classes.get(key).map_or(0, |c| c.values().sum());
                assert_eq!(class_sum, row_errors, "{at}: {key:?}");
            }
        }
    }
}
