//! Tests for the downgrade step that undoes the `account_readings` schema
//! step, and for chaining it into the older steps.

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::Value;

use super::tests::{meta_version, rewrite_once, temp_db_path, user_version};
use super::v18_tests::{FileState, count, file_state, fresh_with_rows};
use super::*;
use crate::db::open;
use crate::schema::{CREATE_ACCOUNT_READINGS_TABLE, SCHEMA_VERSION};

/// The version the step re-stamps a current file to.
const PREVIOUS: i64 = SCHEMA_VERSION - 1;

/// Every `account_readings` row, every column, in id order.
fn history(conn: &Connection) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare("SELECT * FROM account_readings ORDER BY id")
        .expect("prepare");
    let width = stmt.column_count();
    stmt.query_map([], |r| (0..width).map(|i| r.get::<_, Value>(i)).collect())
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("history")
}

/// A fresh current file holding a request row, a gap row, and one history
/// row of each report kind.
fn fresh_with_history(path: &std::path::Path) {
    fresh_with_rows(path);
    Connection::open(path)
        .expect("reopen")
        .execute_batch(
            "INSERT INTO account_readings (ts_ms, account, source, report, window_secs, scope, \
             length_class, used, reset_ms, started) \
             VALUES (10, 'acct-a', 'poll', 'windows', 18000, NULL, 'short', 0.25, 20, 'yes');
             INSERT INTO account_readings (ts_ms, account, source, report) \
             VALUES (11, 'acct-b', 'poll', 'no_windows');",
        )
        .expect("history rows");
}

#[test]
fn the_step_restamps_only_and_a_reopen_keeps_the_history() {
    // Arrange
    let (_dir, path) = temp_db_path();
    fresh_with_history(&path);
    let before = file_state(&path);
    let current = file_state(&path);
    let rows = history(&Connection::open(&path).expect("reopen"));
    assert_eq!(rows.len(), 2, "premise: two history rows");

    // Act
    downgrade_to(&path, PREVIOUS).expect("downgrade");

    // Assert: only the two version values changed; every history row stays.
    assert_eq!(
        file_state(&path),
        FileState {
            user_version: PREVIOUS,
            meta_version: PREVIOUS.to_string(),
            ..before
        }
    );
    assert_eq!(history(&Connection::open(&path).expect("reopen")), rows);

    // Act: the current binary opens it again.
    let db = open(&path).expect("re-upgrade");

    // Assert: current again with the same schema and the same rows, and the
    // re-stamped file is still exactly the additive shape.
    assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
    assert_eq!(meta_version(db.conn()), SCHEMA_VERSION.to_string());
    assert_eq!(history(db.conn()), rows);
    drop(db);
    assert_eq!(file_state(&path), current, "nothing re-created or re-added");
    downgrade_to(&path, PREVIOUS).expect("the re-upgraded file steps down again");
}

#[test]
fn downgrades_through_every_step_keeping_the_history() {
    for target in (*SUPPORTED_TARGETS.start()..=PREVIOUS).rev() {
        // Arrange
        let (_dir, path) = temp_db_path();
        fresh_with_history(&path);
        let before = file_state(&path);
        let rows = history(&Connection::open(&path).expect("reopen"));

        // Act
        downgrade_to(&path, target).expect("downgrade");

        // Assert
        assert_eq!(
            file_state(&path),
            FileState {
                user_version: target,
                meta_version: target.to_string(),
                ..before
            },
            "--to {target}"
        );
        assert_eq!(
            history(&Connection::open(&path).expect("reopen")),
            rows,
            "--to {target}"
        );
    }
}

/// A fresh current file whose `account_readings` is replaced by
/// `replacement` (absent when `None`), with one history row when the
/// replacement accepts it.
fn current_file_with_history_table(path: &std::path::Path, replacement: Option<&str>) {
    drop(open(path).expect("open fresh"));
    let conn = Connection::open(path).expect("reopen");
    conn.execute_batch("DROP TABLE account_readings")
        .expect("drop");
    if let Some(sql) = replacement {
        conn.execute_batch(sql).expect("replacement");
        conn.execute_batch(
            "INSERT INTO account_readings (ts_ms, account, source, report) \
             VALUES (1, 'acct', 'poll', 'no_windows')",
        )
        .ok();
    }
}

fn with_extra_column() -> String {
    format!(
        "{CREATE_ACCOUNT_READINGS_TABLE};\nALTER TABLE account_readings ADD COLUMN extra INTEGER"
    )
}

#[test]
fn refuses_a_file_whose_account_readings_is_not_exactly_the_additive_shape() {
    use NotAdditiveReason::{ColumnCount, ColumnShape};
    let ddl = CREATE_ACCOUNT_READINGS_TABLE;
    let cases: &[(&str, Option<String>, NotAdditiveReason)] = &[
        ("the table is absent", None, ColumnCount),
        (
            "a view in its place",
            Some(
                "CREATE VIEW account_readings AS SELECT 1 AS id, 1 AS ts_ms, 'a' AS account, \
                 'poll' AS source, 'windows' AS report, 1 AS window_secs, NULL AS scope, \
                 'short' AS length_class, 0.0 AS used, NULL AS reset_ms, 'no' AS started"
                    .to_string(),
            ),
            ColumnCount,
        ),
        ("an extra column", Some(with_extra_column()), ColumnCount),
        (
            "a missing column",
            Some(rewrite_once(
                &rewrite_once(ddl, "    reset_ms     INTEGER,\n", ""),
                " AND reset_ms IS NULL",
                "",
            )),
            ColumnCount,
        ),
        (
            "a retyped used column",
            Some(rewrite_once(
                ddl,
                "    used         REAL,",
                "    used         TEXT,",
            )),
            ColumnShape,
        ),
        (
            "a nullable account",
            Some(rewrite_once(
                ddl,
                "    account      TEXT    NOT NULL,",
                "    account      TEXT,",
            )),
            ColumnShape,
        ),
        (
            "a defaulted window length",
            Some(rewrite_once(
                ddl,
                "    window_secs  INTEGER,",
                "    window_secs  INTEGER DEFAULT 0,",
            )),
            ColumnShape,
        ),
        (
            "an id that is not the key",
            Some(rewrite_once(
                ddl,
                "    id           INTEGER PRIMARY KEY,",
                "    id           INTEGER,",
            )),
            ColumnShape,
        ),
        (
            "two columns swapped",
            Some(rewrite_once(
                ddl,
                "    window_secs  INTEGER,\n    scope        TEXT,",
                "    scope        TEXT,\n    window_secs  INTEGER,",
            )),
            ColumnShape,
        ),
    ];
    for (name, replacement, reason) in cases {
        for target in (*SUPPORTED_TARGETS.start()..=PREVIOUS).rev() {
            // Arrange
            let (_dir, path) = temp_db_path();
            current_file_with_history_table(&path, replacement.as_deref());
            let before = file_state(&path);
            let rows_before = count(
                &Connection::open(&path).expect("reopen"),
                "account_readings",
            );

            // Act
            let result = downgrade_to(&path, target);

            // Assert: refused on `account_readings`, the file exactly as it was.
            assert!(
                matches!(
                    result,
                    Err(DowngradeError::NotAdditive { table: "account_readings", reason: r })
                        if r == *reason
                ),
                "{name} --to {target}: {result:?}"
            );
            assert_eq!(file_state(&path), before, "{name} --to {target}");
            assert_eq!(
                count(
                    &Connection::open(&path).expect("reopen"),
                    "account_readings"
                ),
                rows_before,
                "{name} --to {target}"
            );
        }
    }
}

#[test]
fn the_exact_shape_is_accepted_by_the_refusal_fixture() {
    // Positive control for the refusal table: the same builder with the real
    // DDL downgrades to every target.
    for target in (*SUPPORTED_TARGETS.start()..=PREVIOUS).rev() {
        let (_dir, path) = temp_db_path();
        current_file_with_history_table(&path, Some(CREATE_ACCOUNT_READINGS_TABLE));

        downgrade_to(&path, target).unwrap_or_else(|e| panic!("--to {target}: {e:?}"));
        assert_eq!(file_state(&path).user_version, target, "--to {target}");
        assert_eq!(
            count(
                &Connection::open(&path).expect("reopen"),
                "account_readings"
            ),
            Some(1),
            "--to {target}"
        );
    }
}

#[test]
fn the_shape_check_reads_no_history_row() {
    // Arrange: a connection whose authorizer denies reading the table.
    let (_dir, path) = temp_db_path();
    fresh_with_history(&path);
    let mut conn = Connection::open(&path).expect("reopen");
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Read {
            table_name: "account_readings",
            ..
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .expect("authorizer");
    assert!(
        conn.query_row("SELECT ts_ms FROM account_readings", [], |r| r
            .get::<_, i64>(0))
            .is_err(),
        "control: a row read is denied"
    );

    // Act + Assert
    verify_additive_v23(&conn).expect("the shape check reads only the schema");
}
