//! Welds the v22 read indexes to the statements they serve: each index holds
//! exactly the `requests` columns its statements read, and the planner answers
//! those statements from it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::db::{UsageDb, open};
use crate::query::{
    AGG_SQL, ERROR_ROWS_PREDICATE, ERRORS_BY_CLASS_BUCKETED_SQL, ERRORS_BY_CLASS_SQL,
    LATEST_QUOTA_BY_SEAT_SQL, QUERY_AGG_SQL, QUOTA_ROWS_PREDICATE, SERIES_AGG_SQL,
};

const AGG_INDEX: &str = "idx_requests_agg";
const ERROR_INDEX: &str = "idx_requests_errors";
const QUOTA_INDEX: &str = "idx_requests_quota_seat";

fn fresh_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path: PathBuf = dir.path().join("usage.db");
    let db = open(&path).expect("open fresh");
    seed_rows(db.conn());
    (dir, db)
}

/// A few ok and error rows, so the statements run against real data.
fn seed_rows(conn: &Connection) {
    for (i, outcome) in ["ok", "upstream_error", "timeout", "ok"].iter().enumerate() {
        conn.execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, seat, stream, tool_count, msg_count, \
             outcome, attempt_count, fallback_count, latency_ms, ttfb_ms, input_tokens, \
             output_tokens, resolved_class, quota_utilization) \
             VALUES (?1, ?1 + 10, ?2, 'anthropic', 'm', 'a', 'm', 'p', 's', 1, 0, 1, ?3, 1, 0, \
             50, 5, 10, 20, NULL, 0.5)",
            rusqlite::params![
                100 + i64::try_from(i).expect("small"),
                format!("r{i}"),
                outcome
            ],
        )
        .expect("seed row");
    }
}

/// The `requests` column names, read from the live table.
fn requests_columns(conn: &Connection) -> BTreeSet<String> {
    conn.prepare("SELECT name FROM pragma_table_info('requests')")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

/// The key columns of `index`, in order.
fn index_columns(conn: &Connection, index: &str) -> Vec<String> {
    conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
        .expect("prepare")
        .query_map([index], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

/// The `requests` columns `sql` names: every identifier token in the statement
/// text that is a column of the table. An alias that shares a column's name is
/// only ever the alias of an expression over that same column, so the set
/// never names a column the statement does not read.
fn columns_read(sql: &str, columns: &BTreeSet<String>) -> BTreeSet<String> {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| columns.contains(*token))
        .map(str::to_owned)
        .collect()
}

/// The `EXPLAIN QUERY PLAN` detail lines of `sql`, every parameter bound to a
/// small integer (the two filters to NULL, which is how an unfiltered read
/// binds them).
fn plan(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare explain");
    let binds: Vec<Option<i64>> = (1..=stmt.parameter_count())
        .map(|i| match i {
            1 => Some(0),
            2 => Some(10_000),
            3 | 4 => None,
            _ => Some(1_000),
        })
        .collect();
    stmt.query_map(rusqlite::params_from_iter(binds.iter()), |r| {
        r.get::<_, String>(3)
    })
    .expect("query explain")
    .collect::<Result<_, _>>()
    .expect("collect explain")
}

const AGG_STATEMENTS: [(&str, &str); 3] = [
    ("AGG_SQL", AGG_SQL),
    ("QUERY_AGG_SQL", QUERY_AGG_SQL),
    ("SERIES_AGG_SQL", SERIES_AGG_SQL),
];

const ERROR_STATEMENTS: [(&str, &str); 2] = [
    ("unbucketed", ERRORS_BY_CLASS_SQL),
    ("bucketed", ERRORS_BY_CLASS_BUCKETED_SQL),
];

#[test]
fn each_aggregate_statement_is_answered_from_the_covering_index() {
    // Arrange
    let (_dir, db) = fresh_db();
    let expected = format!("USING COVERING INDEX {AGG_INDEX} ");

    for (name, sql) in AGG_STATEMENTS {
        // Act
        let plan = plan(db.conn(), sql);

        // Assert
        assert!(
            plan.iter()
                .any(|d| d.starts_with("SEARCH r ") && d.contains(&expected)),
            "{name} must search the window through {AGG_INDEX} alone; plan was {plan:?}"
        );
    }
}

#[test]
fn the_covering_index_holds_exactly_the_columns_the_aggregate_statements_read() {
    // Arrange
    let (_dir, db) = fresh_db();
    let columns = requests_columns(db.conn());
    let indexed: BTreeSet<String> = index_columns(db.conn(), AGG_INDEX).into_iter().collect();

    // Act
    let mut read_by_any = BTreeSet::new();
    for (name, sql) in AGG_STATEMENTS {
        let read = columns_read(sql, &columns);
        let missing: Vec<&String> = read.difference(&indexed).collect();
        assert!(
            missing.is_empty(),
            "{name} reads columns {AGG_INDEX} lacks: {missing:?}"
        );
        read_by_any.extend(read);
    }

    // Assert: no indexed column is dead weight, and the window column leads.
    assert_eq!(indexed, read_by_any);
    assert_eq!(
        index_columns(db.conn(), AGG_INDEX)
            .first()
            .map(String::as_str),
        Some("ts_start")
    );
}

#[test]
fn the_error_class_statement_bucketed_and_not_is_answered_from_the_error_index() {
    // Arrange
    let (_dir, db) = fresh_db();
    let expected = format!("USING COVERING INDEX {ERROR_INDEX} ");

    for (name, sql) in ERROR_STATEMENTS {
        // Act
        let plan = plan(db.conn(), sql);

        // Assert
        assert!(
            plan.iter()
                .any(|d| d.starts_with("SEARCH requests ") && d.contains(&expected)),
            "{name} error-class read must search through {ERROR_INDEX} alone; plan was {plan:?}"
        );
    }
}

#[test]
fn the_error_index_holds_exactly_the_columns_the_class_statements_read() {
    // Arrange
    let (_dir, db) = fresh_db();
    let columns = requests_columns(db.conn());
    let indexed: BTreeSet<String> = index_columns(db.conn(), ERROR_INDEX).into_iter().collect();

    // Act
    let read: BTreeSet<String> = ERROR_STATEMENTS
        .iter()
        .flat_map(|(_, sql)| columns_read(sql, &columns))
        .collect();

    // Assert
    assert_eq!(indexed, read);
}

/// The partial-index predicate of a v22 index DDL: the text after `WHERE`.
fn partial_predicate(ddl: &str) -> &str {
    ddl.split_once(" WHERE ")
        .map(|(_, predicate)| predicate.trim())
        .expect("a partial index carries a WHERE")
}

#[test]
fn the_error_index_is_over_exactly_the_error_class_statements_predicate() {
    // Arrange
    let guarded = format!("AND {ERROR_ROWS_PREDICATE}\n");

    // Act
    let predicate = partial_predicate(CREATE_ERROR_ROWS_INDEX);

    // Assert: the index holds exactly the rows the statements read, and each
    // statement filters on that same predicate.
    assert_eq!(predicate, ERROR_ROWS_PREDICATE);
    for (name, sql) in ERROR_STATEMENTS {
        assert_eq!(sql.matches(&guarded).count(), 1, "{name}");
    }
}

#[test]
fn the_quota_index_is_seat_then_time_over_exactly_the_latest_quota_predicate() {
    // Arrange
    let (_dir, db) = fresh_db();

    // Act
    let columns = index_columns(db.conn(), QUOTA_INDEX);
    let predicate = partial_predicate(CREATE_QUOTA_SEAT_INDEX);

    // Assert
    assert_eq!(columns, ["seat", "ts_start"]);
    assert_eq!(predicate, QUOTA_ROWS_PREDICATE);
}

#[test]
fn every_requests_reference_in_the_latest_quota_read_carries_the_quota_predicate() {
    // Arrange
    let guarded = format!("WHERE ({QUOTA_ROWS_PREDICATE}) AND ");

    // Act
    let references = LATEST_QUOTA_BY_SEAT_SQL
        .matches("FROM requests AS ")
        .count();
    let filtered = LATEST_QUOTA_BY_SEAT_SQL.matches(&guarded).count();

    // Assert: the one `JOIN requests` is the rowid lookup of the chosen row.
    assert!(references > 0, "the read must name requests");
    assert_eq!(filtered, references);
}

#[test]
fn the_latest_quota_read_seeks_the_quota_index_and_never_scans_requests() {
    // Arrange
    let (_dir, db) = fresh_db();
    let aliases = ["lowest", "higher", "unseated", "newest"];

    // Act
    let plan = plan(db.conn(), LATEST_QUOTA_BY_SEAT_SQL);

    // Assert
    for alias in aliases {
        let prefix = format!("SEARCH {alias} ");
        assert!(
            plan.iter()
                .any(|d| d.starts_with(&prefix) && d.contains(&format!("INDEX {QUOTA_INDEX} "))),
            "{alias} must seek {QUOTA_INDEX}; plan was {plan:?}"
        );
    }
    assert!(
        plan.iter()
            .any(|d| d == "SEARCH latest USING INTEGER PRIMARY KEY (rowid=?)"),
        "the chosen row must be read by rowid; plan was {plan:?}"
    );
    let scans: Vec<&String> = plan
        .iter()
        .filter(|d| {
            d.starts_with("SCAN ")
                && !matches!(
                    d.as_str(),
                    "SCAN seats" | "SCAN buckets" | "SCAN CONSTANT ROW"
                )
        })
        .collect();
    assert!(
        scans.is_empty(),
        "no table scan expected; plan was {plan:?}"
    );
}
