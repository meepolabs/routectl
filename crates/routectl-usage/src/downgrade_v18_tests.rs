//! Tests for the 18 -> 17 step and for chaining it into the 17 -> 16 step.

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::Value;

use super::tests::{meta_version, rewrite_once, temp_db_path, user_version};
use super::*;
use crate::db::open;
use crate::migrate::migrate_to_current;
use crate::schema::{
    CREATE_CAPABILITY_EVENTS_TABLE, CREATE_LEDGER_GAPS_TABLE, CREATE_META_TABLE,
    CREATE_REQUESTS_TABLE, SCHEMA_VERSION,
};

/// The `requests` DDL as the first schema version created it, frozen as
/// text: the starting point of a ledger that climbed every forward step.
const V1_REQUESTS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS requests (
    ts_start        INTEGER NOT NULL,
    ts_end          INTEGER NOT NULL,
    request_id      TEXT    NOT NULL UNIQUE,
    ingress_dialect TEXT    NOT NULL,
    requested_model TEXT    NOT NULL,
    alias           TEXT    NOT NULL,
    model           TEXT,
    upstream        TEXT,
    provider        TEXT,
    provider_kind   TEXT,
    seat            TEXT,
    session_id      TEXT,
    stream          INTEGER NOT NULL,
    max_tokens_req  INTEGER,
    tool_count      INTEGER NOT NULL,
    thinking_req    INTEGER,
    thinking_req_kind TEXT,
    msg_count       INTEGER NOT NULL,
    service_tier    TEXT,
    outcome         TEXT    NOT NULL CHECK (outcome IN (
                        'ok',
                        'upstream_error',
                        'client_disconnect',
                        'timeout',
                        'cancelled',
                        'gate_blocked'
                    )),
    http_status     INTEGER,
    error_class     TEXT,
    finish_reason   TEXT,
    attempt_count   INTEGER NOT NULL,
    fallback_count  INTEGER NOT NULL,
    latency_ms      INTEGER NOT NULL,
    ttfb_ms         INTEGER,
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    reasoning_tokens INTEGER,
    cache_read      INTEGER,
    cache_write_5m  INTEGER,
    cache_write_1h  INTEGER,
    server_tool_use TEXT,
    quota_claim     TEXT,
    quota_status    TEXT,
    quota_overage_status TEXT,
    quota_utilization    REAL,
    quota_overage_utilization REAL,
    quota_reset     INTEGER,
    quota_extras    TEXT,
    extra           TEXT
)";

/// The column list a v17 writer's `INSERT` names, frozen as text. A v17
/// binary handed a downgraded file writes through exactly this list.
const V17_INSERT_COLUMNS: &str = "\
    ts_start, ts_end, request_id, ingress_dialect, requested_model, alias,
    model, upstream, provider, provider_kind, seat, session_id,
    stream, max_tokens_req, tool_count, thinking_req, thinking_req_kind,
    msg_count, service_tier,
    outcome, http_status, error_class, finish_reason, attempt_count, fallback_count,
    latency_ms, ttfb_ms,
    input_tokens, output_tokens, reasoning_tokens, cache_read, cache_write_5m,
    cache_write_1h, server_tool_use,
    quota_claim, quota_status, quota_overage_status, quota_utilization,
    quota_overage_utilization, quota_reset, quota_extras,
    extra,
    would_trim_tokens,
    would_trim_break_even_k,
    would_trim_k_floor,
    would_trim_shadow_misfire,
    would_trim_dedup_tokens,
    would_trim_supersession_tokens,
    would_trim_path_units,
    would_trim_path_extractable,
    would_trim_recorder_version,
    would_trim_raw_marks,
    would_trim_context_fraction,
    resolved_class,
    calib_estimated_tokens,
    calib_prompt_tokens,
    reduction_decision,
    reduction_strings_compressed,
    reduction_strings_skipped,
    reduction_strings_rejected,
    reduction_bytes_saved,
    cache_front_decision,
    cache_terminal_decision,
    prefix_epoch_event";

/// Everything a refused downgrade must leave as it was: both version stamps,
/// the schema text of every object, and the row counts of the tables a step
/// covers.
#[derive(Debug, PartialEq, Eq)]
struct FileState {
    user_version: i64,
    meta_version: String,
    schema: Vec<(String, String, Option<String>)>,
    requests: i64,
    ledger_gaps: Option<i64>,
}

fn file_state(path: &std::path::Path) -> FileState {
    let conn = Connection::open(path).expect("reopen");
    let schema = conn
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
        .expect("prepare")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("schema")
        .collect::<Result<_, _>>()
        .expect("rows");
    FileState {
        user_version: user_version(&conn),
        meta_version: meta_version(&conn),
        schema,
        requests: count(&conn, "requests").expect("requests"),
        ledger_gaps: count(&conn, "ledger_gaps"),
    }
}

/// Row count of `table`, or `None` when it cannot be read.
fn count(conn: &Connection, table: &str) -> Option<i64> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .ok()
}

fn requests_column_names(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM pragma_table_info('requests')")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("columns")
        .collect::<Result<_, _>>()
        .expect("names")
}

/// Insert one request row through the frozen v17 column list, binding a
/// value to every `NOT NULL` column and NULL to the rest.
fn insert_as_v17_writer(conn: &Connection, request_id: &str) {
    let columns: Vec<&str> = V17_INSERT_COLUMNS
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    let values: Vec<Value> = columns
        .iter()
        .map(|&column| match column {
            "request_id" => Value::Text(request_id.to_string()),
            "ingress_dialect" | "requested_model" | "alias" => Value::Text("v".to_string()),
            "outcome" => Value::Text("ok".to_string()),
            "ts_start" | "ts_end" | "stream" | "tool_count" | "msg_count" | "attempt_count"
            | "fallback_count" | "latency_ms" => Value::Integer(1),
            _ => Value::Null,
        })
        .collect();
    let placeholders = (1..=columns.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT OR IGNORE INTO requests ({}) VALUES ({placeholders})",
        columns.join(", ")
    );
    let inserted = conn
        .execute(&sql, rusqlite::params_from_iter(values))
        .expect("v17 insert");
    assert_eq!(inserted, 1);
}

/// A fresh current file holding one request row and one gap row.
fn fresh_v18_with_rows(path: &std::path::Path) {
    let db = open(path).expect("open fresh");
    insert_as_v17_writer(db.conn(), "fresh-row");
    db.conn()
        .execute_batch(
            "INSERT INTO ledger_gaps (ts_ms, cause, dropped) VALUES (1, 'queue_full', 3)",
        )
        .expect("gap row");
}

/// A WAL file stamped v18 whose `requests` is created by `requests_ddl` then
/// extended by `added_columns`, and whose `ledger_gaps` is created by
/// `gaps_ddl` (absent when `None`).
fn v18_file_from(
    path: &std::path::Path,
    requests_ddl: &str,
    added_columns: &[&str],
    gaps_ddl: Option<&str>,
) {
    let conn = Connection::open(path).expect("raw open");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    conn.execute_batch(requests_ddl).expect("requests");
    for column in added_columns {
        conn.execute_batch(&format!("ALTER TABLE requests ADD COLUMN {column}"))
            .expect("add column");
    }
    if let Some(ddl) = gaps_ddl {
        conn.execute_batch(ddl).expect("ledger_gaps");
    }
    conn.execute_batch(CREATE_META_TABLE).expect("meta");
    conn.execute_batch(CREATE_CAPABILITY_EVENTS_TABLE)
        .expect("capability_events");
    conn.execute_batch(
        "INSERT INTO meta (key, value) VALUES ('schema_version', '18');
         PRAGMA user_version = 18;",
    )
    .expect("stamp v18");
}

/// The current `requests` DDL cut after its v17 set, so the v18 tail can be
/// appended by `ALTER TABLE` in a chosen shape.
fn requests_ddl_through_v17() -> String {
    const LAST_V17_COLUMN: &str = "    prefix_epoch_event INTEGER,";
    let (head, _) = CREATE_REQUESTS_TABLE
        .split_once(LAST_V17_COLUMN)
        .expect("the DDL still ends its v17 set with prefix_epoch_event");
    format!("{head}    prefix_epoch_event INTEGER\n)")
}

/// The eight v18 columns as `ALTER TABLE ... ADD COLUMN` definitions.
fn v18_tail() -> Vec<String> {
    V18_REQUESTS_COLUMNS
        .iter()
        .map(|(name, declared_type)| format!("{name} {declared_type}"))
        .collect()
}

#[test]
fn downgrades_a_fresh_v18_db_to_17_keeping_every_column_row_and_table() {
    // Arrange
    let (_dir, path) = temp_db_path();
    fresh_v18_with_rows(&path);
    let before = file_state(&path);
    assert!(
        before
            .schema
            .iter()
            .any(|(kind, name, _)| kind == "index" && name == "idx_ledger_gaps_ts_ms"),
        "sanity: the fresh file carries the gap index the downgrade must keep"
    );

    // Act
    downgrade_to(&path, 17).expect("downgrade");

    // Assert: only the two version values changed.
    let after = file_state(&path);
    assert_eq!(after.user_version, 17);
    assert_eq!(after.meta_version, "17");
    assert_eq!(after.schema, before.schema);
    assert_eq!(after.requests, before.requests);
    assert_eq!(after.ledger_gaps, Some(1));
}

#[test]
fn downgrades_a_ledger_that_climbed_every_step_from_v1() {
    for target in [17, 16] {
        // Arrange: a v1 file with one row, migrated through the whole ladder.
        let (_dir, path) = temp_db_path();
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(V1_REQUESTS_DDL).expect("v1 requests");
        conn.execute_batch(
            "CREATE INDEX idx_requests_ts_start ON requests (ts_start);
             CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '1');
             INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count) \
                 VALUES (1, 2, 'v1-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0);
             PRAGMA user_version = 1;",
        )
        .expect("seed v1");
        assert_eq!(migrate_to_current(&conn, 0).expect("climb"), SCHEMA_VERSION);
        drop(conn);
        let before = file_state(&path);

        // Act
        downgrade_to(&path, target).expect("a ladder-built file downgrades");

        // Assert
        let after = file_state(&path);
        assert_eq!(after.user_version, target, "--to {target}");
        assert_eq!(after.meta_version, target.to_string(), "--to {target}");
        assert_eq!(after.schema, before.schema, "--to {target}");
        assert_eq!(after.requests, 1, "--to {target}");
    }
}

#[test]
fn a_v17_writer_inserts_into_a_downgraded_file_and_reopening_re_upgrades_it() {
    // Arrange: a downgraded file a v17 binary then wrote to.
    let (_dir, path) = temp_db_path();
    fresh_v18_with_rows(&path);
    let fresh_columns = requests_column_names(&Connection::open(&path).expect("reopen"));
    downgrade_to(&path, 17).expect("downgrade");
    insert_as_v17_writer(&Connection::open(&path).expect("reopen"), "v17-row");

    // Act: the current binary opens it again.
    let db = open(&path).expect("re-upgrade");

    // Assert: v18 again, the same column list with no duplicate, both request
    // rows present with the v17-written row's tail NULL, and the one gap row.
    assert_eq!(user_version(db.conn()), 18);
    assert_eq!(meta_version(db.conn()), "18");
    assert_eq!(requests_column_names(db.conn()), fresh_columns);
    assert_eq!(count(db.conn(), "requests"), Some(2));
    assert_eq!(count(db.conn(), "ledger_gaps"), Some(1));
    let operation: Option<String> = db
        .conn()
        .query_row(
            "SELECT operation FROM requests WHERE request_id = 'v17-row'",
            [],
            |r| r.get(0),
        )
        .expect("v17 row");
    assert_eq!(operation, None);
    drop(db);
    downgrade_to(&path, 17).expect("the re-upgraded file is still exactly additive");
}

/// One refusal row: the `requests` DDL, columns appended to it, the
/// `ledger_gaps` DDL (absent when `None`), and the table and reason the
/// refusal must name.
type ShapeCase<'a> = (
    &'a str,
    &'a str,
    &'a [&'a str],
    Option<&'a str>,
    &'static str,
    NotAdditiveReason,
);

#[test]
fn refuses_a_file_that_is_not_exactly_the_additive_v18_shape() {
    use NotAdditiveReason::{ColumnCount, ColumnShape};
    let tail = v18_tail();
    let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
    let through_v17 = requests_ddl_through_v17();
    let full = CREATE_REQUESTS_TABLE;
    let gaps = Some(CREATE_LEDGER_GAPS_TABLE);
    let with_extra: Vec<&str> = tail.iter().copied().chain(["extra_col TEXT"]).collect();
    let mut swapped = tail.clone();
    swapped.swap(0, 1);
    let probe = "    probe           INTEGER,";
    let defaulted = rewrite_once(full, probe, "    probe INTEGER DEFAULT 0,");
    let not_null = rewrite_once(full, probe, "    probe INTEGER NOT NULL,");
    let wrong_type = rewrite_once(full, probe, "    probe TEXT,");
    let v17_retyped = rewrite_once(
        full,
        "    input_tokens    INTEGER,",
        "    input_tokens TEXT,",
    );
    let gaps_ddl = CREATE_LEDGER_GAPS_TABLE;
    let gaps_nullable = rewrite_once(gaps_ddl, "    cause   TEXT    NOT NULL,", "    cause TEXT,");
    let gaps_extra = rewrite_once(
        gaps_ddl,
        "    dropped INTEGER NOT NULL\n",
        "    dropped INTEGER NOT NULL,\n    extra TEXT\n",
    );
    let gaps_view = "CREATE VIEW ledger_gaps AS SELECT 1 AS ts_ms, 'x' AS cause, 1 AS dropped";
    let cases: &[ShapeCase<'_>] = &[
        (
            "an extra trailing column",
            &through_v17,
            &with_extra,
            gaps,
            "requests",
            ColumnCount,
        ),
        (
            "a missing tail column",
            &through_v17,
            &tail[..7],
            gaps,
            "requests",
            ColumnCount,
        ),
        (
            "the tail out of order",
            &through_v17,
            &swapped,
            gaps,
            "requests",
            ColumnShape,
        ),
        (
            "a tail column with a default",
            &defaulted,
            &[],
            gaps,
            "requests",
            ColumnShape,
        ),
        (
            "a tail column NOT NULL",
            &not_null,
            &[],
            gaps,
            "requests",
            ColumnShape,
        ),
        (
            "a tail column of the wrong type",
            &wrong_type,
            &[],
            gaps,
            "requests",
            ColumnShape,
        ),
        (
            "a v17 column of the wrong type",
            &v17_retyped,
            &[],
            gaps,
            "requests",
            ColumnShape,
        ),
        (
            "no ledger_gaps table",
            full,
            &[],
            None,
            "ledger_gaps",
            ColumnCount,
        ),
        (
            "a nullable ledger_gaps column",
            full,
            &[],
            Some(&gaps_nullable),
            "ledger_gaps",
            ColumnShape,
        ),
        (
            "an extra ledger_gaps column",
            full,
            &[],
            Some(&gaps_extra),
            "ledger_gaps",
            ColumnCount,
        ),
        (
            "ledger_gaps as a view",
            full,
            &[],
            Some(gaps_view),
            "ledger_gaps",
            ColumnCount,
        ),
    ];
    for &(name, ddl, added, gaps_ddl, table, reason) in cases {
        for target in [17, 16] {
            // Arrange
            let (_dir, path) = temp_db_path();
            v18_file_from(&path, ddl, added, gaps_ddl);
            let before = file_state(&path);

            // Act
            let result = downgrade_to(&path, target);

            // Assert
            assert!(
                matches!(
                    result,
                    Err(DowngradeError::NotAdditive { table: t, reason: r })
                        if t == table && r == reason
                ),
                "{name} --to {target}: {result:?}"
            );
            assert_eq!(file_state(&path), before, "{name} --to {target}");
        }
    }
}

#[test]
fn the_exact_additive_v18_shape_is_accepted_by_the_shape_fixture() {
    // Positive controls for the refusal table: the same builder with the real
    // DDL, and with the v17 DDL plus the exact tail, both downgrade.
    let tail = v18_tail();
    let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
    let through_v17 = requests_ddl_through_v17();
    let cases: &[(&str, &str, &[&str])] = &[
        ("the real v18 DDL", CREATE_REQUESTS_TABLE, &[]),
        ("v17 DDL plus the appended tail", &through_v17, &tail),
    ];
    for &(name, ddl, added) in cases {
        let (_dir, path) = temp_db_path();
        v18_file_from(&path, ddl, added, Some(CREATE_LEDGER_GAPS_TABLE));

        downgrade_to(&path, 16).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(file_state(&path).user_version, 16, "{name}");
    }
}

#[test]
fn a_refusal_in_the_second_step_leaves_the_file_at_18() {
    // Arrange: a v18 file whose v18 shape is exact but whose
    // `capability_events` carries an extra column, so only 17 -> 16 refuses.
    let (_dir, path) = temp_db_path();
    fresh_v18_with_rows(&path);
    Connection::open(&path)
        .expect("reopen")
        .execute_batch("ALTER TABLE capability_events ADD COLUMN extra TEXT")
        .expect("extra column");
    let before = file_state(&path);

    // Act
    let result = downgrade_to(&path, 16);

    // Assert: refused by the second step, and the first step's re-stamp was
    // rolled back with it.
    assert!(
        matches!(
            result,
            Err(DowngradeError::NotAdditive {
                table: "capability_events",
                reason: NotAdditiveReason::ColumnCount
            })
        ),
        "{result:?}"
    );
    assert_eq!(file_state(&path), before);
    downgrade_to(&path, 17).expect("the first step alone accepts the same file");
    assert_eq!(file_state(&path).user_version, 17);
}

#[test]
fn refuses_a_v18_file_whose_meta_mirror_disagrees() {
    // Arrange
    let (_dir, path) = temp_db_path();
    fresh_v18_with_rows(&path);
    Connection::open(&path)
        .expect("reopen")
        .execute_batch("UPDATE meta SET value = '17' WHERE key = 'schema_version'")
        .expect("tamper meta");

    // Act
    let result = downgrade_to(&path, 17);

    // Assert
    assert!(
        matches!(result, Err(DowngradeError::MetaMismatch { .. })),
        "{result:?}"
    );
    assert_eq!(file_state(&path).user_version, 18);
}

#[test]
fn the_v18_shape_check_reads_no_row_of_the_tables_it_checks() {
    // Arrange: a connection whose authorizer denies reading any column of the
    // two tables the step covers.
    let (_dir, path) = temp_db_path();
    fresh_v18_with_rows(&path);
    let conn = Connection::open(&path).expect("reopen");
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Read {
            table_name: "requests" | "ledger_gaps",
            ..
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .expect("authorizer");
    assert!(
        conn.query_row("SELECT ts_ms FROM ledger_gaps", [], |r| r.get::<_, i64>(0))
            .is_err(),
        "control: a row read is denied"
    );

    // Act + Assert
    verify_additive_v18(&conn).expect("the shape check reads only the schema");
}
