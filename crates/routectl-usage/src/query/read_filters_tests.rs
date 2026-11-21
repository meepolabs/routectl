//! Every statement of the grouped read takes its window, filters and
//! watermark from the one shared predicate, and the side reads it feeds agree
//! with the aggregate under each filter.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::Transaction;
use tempfile::TempDir;

use super::aggregate::{
    ERRORS_BY_CLASS_BUCKETED_SQL, ERRORS_BY_CLASS_SQL, QUERY_AGG_SQL, ReadWindow, SERIES_AGG_SQL,
    read_error_classes, read_fine_rows,
};
use super::cache_lifetimes::{CACHE_LIFETIMES_BUCKETED_SQL, CACHE_LIFETIMES_SQL, read_lifetimes};
use super::deadline::ReadLimit;
use super::grouped::GroupDim;
use super::timings::{read_timings, statements};
use super::{GroupKey, UsageDb};

const READ_FILTERS: &str = read_filters!();

/// The optional-filter shape every bind-param filter of the predicate takes.
const OPTIONAL_FILTER: &str = "IS NULL OR";

/// Every request filter: its bind param and the column it compares.
const FILTERS: [(&str, u8, &str); 3] = [
    ("alias", 3, "alias"),
    ("provider", 4, "provider"),
    ("model", 6, "COALESCE(model, requested_model)"),
];

/// Every request filter and the clause that applies it, one per bind param.
fn filter_clauses() -> Vec<(&'static str, String)> {
    FILTERS
        .iter()
        .map(|&(filter, param, column)| {
            (
                filter,
                format!("(?{param} {OPTIONAL_FILTER} {column} = ?{param})"),
            )
        })
        .collect()
}

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
    // eight timing statements, and the predicate carries every filter and the
    // inference-row predicate (the source scan of every requests reader
    // relies on that).
    assert_eq!(statements.len(), 14, "statement census");
    assert_eq!(
        filters_in_predicate,
        FILTERS.len(),
        "one optional clause per request filter"
    );
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
        for (filter, clause) in filter_clauses() {
            assert_eq!(
                sql.matches(clause.as_str()).count(),
                1,
                "{name} must apply the {filter} filter exactly once: {sql}"
            );
        }
    }
}

fn open_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = crate::db::open(dir.path().join("usage.db")).expect("open");
    (dir, db)
}

/// Rows over two aliases, two providers and two models, with error rows of
/// several classes (one unclassified), a success, outcomes a breakdown must
/// not count, and an error never dispatched (no served model or provider,
/// requested as `m2`), spread over two buckets of 1000 ms.
fn seed(db: &UsageDb) {
    let rows = [
        (
            "e1",
            100,
            "al",
            Some("pa"),
            Some("m1"),
            "upstream_error",
            Some("http-5xx"),
        ),
        (
            "e2",
            1500,
            "al",
            Some("pa"),
            Some("m2"),
            "upstream_error",
            None,
        ),
        ("ok", 1600, "al", Some("pa"), Some("m1"), "ok", None),
        (
            "e3",
            200,
            "bl",
            Some("pb"),
            Some("m1"),
            "upstream_error",
            Some("timeout"),
        ),
        (
            "e4",
            1200,
            "bl",
            Some("pa"),
            Some("m1"),
            "gate_blocked",
            Some("rate-limited"),
        ),
        (
            "e5",
            1700,
            "al",
            Some("pb"),
            Some("m2"),
            "upstream_error",
            Some("timeout"),
        ),
        (
            "e6",
            1800,
            "bl",
            None,
            None,
            "gate_blocked",
            Some("rate-limited"),
        ),
        (
            "cd",
            300,
            "al",
            Some("pa"),
            Some("m1"),
            "client_disconnect",
            Some("timeout"),
        ),
        (
            "cx",
            400,
            "bl",
            Some("pb"),
            Some("m2"),
            "cancelled",
            Some("timeout"),
        ),
    ];
    for (request_id, ts_start, alias, provider, model, outcome, class) in rows {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 resolved_class) \
                 VALUES (?1, ?1, ?2, 'openai', COALESCE(?5, 'm2'), ?3, ?5, ?4, 'u1', 0, ?6, \
                 10, 0, 0, 1, 0, ?7)",
                rusqlite::params![ts_start, request_id, alias, provider, model, outcome, class],
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
    let window = |alias_filter, provider_filter, model_filter| ReadWindow {
        from_ms: 0,
        to_ms: 2000,
        alias_filter,
        provider_filter,
        model_filter,
        watermark: i64::MAX,
        anchor_ms: 0,
    };
    // (case, alias filter, provider filter, model filter, total errors the
    // filter keeps)
    let cases = [
        ("unfiltered", None, None, None, 6),
        ("alias filter", Some("al"), None, None, 3),
        ("provider filter", None, Some("pa"), None, 3),
        ("model filter", None, None, Some("m2"), 3),
    ];

    for (case, alias_filter, provider_filter, model_filter, expected_errors) in cases {
        for bucket_width_ms in [None, Some(1000)] {
            // Act
            let (errors, classes) = read_errors_and_classes(
                &db,
                &window(alias_filter, provider_filter, model_filter),
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

/// One ledger row of the model-filter fixture.
struct ModelRow {
    request_id: &'static str,
    ts_start: i64,
    requested_model: &'static str,
    /// `None` is a request never dispatched: no served model or provider.
    model: Option<&'static str>,
    provider: Option<&'static str>,
    outcome: &'static str,
    class: Option<&'static str>,
    latency_ms: i64,
    cache_write_tokens: Option<i64>,
    cache_write_lifetimes: Option<&'static str>,
}

/// Served `m1` rows (a success with a split write, an error with an unsplit
/// write), a request for `m1` never dispatched, and `m2` rows, one of them
/// requested as `m1` but served by `m2`, over two buckets of 1000 ms.
const MODEL_ROWS: [ModelRow; 5] = [
    ModelRow {
        request_id: "m1-ok",
        ts_start: 100,
        requested_model: "asked",
        model: Some("m1"),
        provider: Some("pa"),
        outcome: "ok",
        class: None,
        latency_ms: 100,
        cache_write_tokens: Some(10),
        cache_write_lifetimes: Some(r#"{"300":10}"#),
    },
    ModelRow {
        request_id: "m1-undispatched",
        ts_start: 1100,
        requested_model: "m1",
        model: None,
        provider: None,
        outcome: "gate_blocked",
        class: Some("rate-limited"),
        latency_ms: 1,
        cache_write_tokens: None,
        cache_write_lifetimes: None,
    },
    ModelRow {
        request_id: "m1-error",
        ts_start: 1200,
        requested_model: "asked",
        model: Some("m1"),
        provider: Some("pb"),
        outcome: "upstream_error",
        class: Some("timeout"),
        latency_ms: 200,
        cache_write_tokens: Some(20),
        cache_write_lifetimes: None,
    },
    ModelRow {
        request_id: "m2-asked-as-m1",
        ts_start: 300,
        requested_model: "m1",
        model: Some("m2"),
        provider: Some("pa"),
        outcome: "upstream_error",
        class: Some("http-5xx"),
        latency_ms: 900,
        cache_write_tokens: Some(30),
        cache_write_lifetimes: Some(r#"{"3600":30}"#),
    },
    ModelRow {
        request_id: "m2-ok",
        ts_start: 1300,
        requested_model: "asked",
        model: Some("m2"),
        provider: Some("pa"),
        outcome: "ok",
        class: None,
        latency_ms: 800,
        cache_write_tokens: None,
        cache_write_lifetimes: None,
    },
];

fn seed_models(db: &UsageDb) {
    for row in &MODEL_ROWS {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 resolved_class, cache_write_tokens, cache_write_lifetimes) \
                 VALUES (?1, ?1, ?2, 'anthropic', ?3, 'al', ?4, ?5, ?5, 0, ?6, \
                 ?7, 0, 0, 1, 0, ?8, ?9, ?10)",
                rusqlite::params![
                    row.ts_start,
                    row.request_id,
                    row.requested_model,
                    row.model,
                    row.provider,
                    row.outcome,
                    row.latency_ms,
                    row.class,
                    row.cache_write_tokens,
                    row.cache_write_lifetimes,
                ],
            )
            .expect("insert row");
    }
}

/// What one read of the grouped read saw: the fine model labels of its rows,
/// and one figure that grows with every row it counts.
type Seen = (BTreeSet<String>, i64);

fn model_label(key: &GroupKey) -> String {
    key.model.clone().unwrap_or_default()
}

/// Requests per fine row.
fn aggregate_seen(tx: &Transaction<'_>, window: &ReadWindow<'_>, width: Option<i64>) -> Seen {
    let rows = read_fine_rows(tx, window, width, &ReadLimit::unbounded()).expect("fine rows");
    let labels = rows
        .iter()
        .map(|(row, _)| model_label(&row.agg.key))
        .collect();
    (labels, rows.iter().map(|(row, _)| row.agg.requests).sum())
}

/// Error rows counted by class.
fn error_classes_seen(tx: &Transaction<'_>, window: &ReadWindow<'_>, width: Option<i64>) -> Seen {
    let classes =
        read_error_classes(tx, window, width, &ReadLimit::unbounded()).expect("error classes");
    let labels = classes.keys().map(|(key, _)| model_label(key)).collect();
    let count = classes.values().flat_map(BTreeMap::values).sum();
    (labels, count)
}

/// Cache-write tokens by lifetime, unknown lifetime included.
fn cache_lifetimes_seen(tx: &Transaction<'_>, window: &ReadWindow<'_>, width: Option<i64>) -> Seen {
    let lifetimes = read_lifetimes(tx, window, width, &ReadLimit::unbounded()).expect("lifetimes");
    let labels = lifetimes.keys().map(|(key, _)| model_label(key)).collect();
    let tokens = lifetimes
        .values()
        .map(|l| l.tokens_by_lifetime.values().sum::<i64>() + l.unknown_tokens)
        .sum();
    (labels, tokens)
}

/// The window's latency p99 under the model grouping.
fn timings_seen(tx: &Transaction<'_>, window: &ReadWindow<'_>, width: Option<i64>) -> Seen {
    let limit = ReadLimit::unbounded();
    let samples = read_timings(tx, window, GroupDim::Model, &BTreeMap::new(), width, &limit)
        .expect("timings");
    let labels = samples.by_label.keys().cloned().collect();
    let ranks = samples
        .finish(width.map(|_| 2), &limit)
        .expect("percentiles");
    (labels, ranks.totals.latency_p99_ms.unwrap_or_default())
}

type ReadSeen = fn(&Transaction<'_>, &ReadWindow<'_>, Option<i64>) -> Seen;

fn labels(models: &[&str]) -> BTreeSet<String> {
    models.iter().map(|m| (*m).to_string()).collect()
}

#[test]
fn the_model_filter_keeps_only_that_models_rows_in_every_read() {
    // Arrange
    let (_dir, db) = open_db();
    seed_models(&db);
    let window = |model_filter| ReadWindow {
        from_ms: 0,
        to_ms: 2000,
        alias_filter: None,
        provider_filter: None,
        model_filter,
        watermark: i64::MAX,
        anchor_ms: 0,
    };
    // (read, its figure for m1 alone, its figure unfiltered). m1 holds the
    // served m1 rows and the request for m1 never dispatched; the row asked
    // as m1 but served by m2 is m2's.
    let cases: [(&str, ReadSeen, i64, i64); 4] = [
        ("aggregate requests", aggregate_seen, 3, 5),
        ("error classes", error_classes_seen, 2, 3),
        ("cache lifetimes", cache_lifetimes_seen, 30, 60),
        ("timing latency p99", timings_seen, 200, 900),
    ];

    for (read, seen, m1_figure, all_figure) in cases {
        for width in [None, Some(1000)] {
            let at = format!("{read}, buckets {width:?}");
            let tx = db.conn().unchecked_transaction().expect("begin read");

            // Act
            let filtered = seen(&tx, &window(Some("m1")), width);
            let unfiltered = seen(&tx, &window(None), width);

            // Assert: the unfiltered read sees both models, so the fixture
            // can show a row of the wrong model.
            assert_eq!(filtered, (labels(&["m1"]), m1_figure), "{at}: m1");
            assert_eq!(
                unfiltered,
                (labels(&["m1", "m2"]), all_figure),
                "{at}: unfiltered"
            );
        }
    }
}
