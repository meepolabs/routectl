use std::collections::BTreeSet;

use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::db::open;
use crate::schema::CREATE_ACCOUNT_READINGS_TABLE;

/// The quoted tokens of `column`'s own `CHECK (column IN (...))` in the DDL.
fn ddl_check_tokens(column: &str) -> BTreeSet<String> {
    let marker = format!("CHECK ({column} IN (");
    let line = CREATE_ACCOUNT_READINGS_TABLE
        .lines()
        .find(|line| line.trim_start().starts_with(column) && line.contains(&marker))
        .unwrap_or_else(|| panic!("{column} carries its CHECK on its own line"));
    let (_, list) = line.split_once(&marker).expect("marker present");
    let (list, _) = list.split_once(')').expect("closed list");
    list.split(',')
        .map(|token| token.trim().trim_matches('\'').to_string())
        .collect()
}

fn tokens(values: impl IntoIterator<Item = &'static str>) -> BTreeSet<String> {
    values.into_iter().map(str::to_string).collect()
}

#[test]
fn ddl_check_tokens_equal_the_rust_as_str_values() {
    let cases = [
        (
            "source",
            tokens(AccountReadingSource::ALL.map(AccountReadingSource::as_str)),
        ),
        (
            "report",
            tokens(AccountReadingReport::ALL.map(AccountReadingReport::as_str)),
        ),
        (
            "length_class",
            tokens(WindowLengthClass::ALL.map(WindowLengthClass::as_str)),
        ),
        (
            "started",
            tokens(WindowStarted::ALL.map(WindowStarted::as_str)),
        ),
    ];
    for (column, rust) in cases {
        assert_eq!(ddl_check_tokens(column), rust, "{column}");
    }
}

#[test]
fn the_report_token_follows_whether_the_row_carries_a_window() {
    let mut row = AccountReadingRow {
        ts_ms: 1,
        account: "a".to_string(),
        source: AccountReadingSource::Poll,
        window: None,
    };
    assert_eq!(row.report(), AccountReadingReport::NoWindows);
    row.window = Some(AccountReadingWindow {
        window_secs: 18_000,
        scope: None,
        length_class: WindowLengthClass::Short,
        used: 0.5,
        reset_ms: Some(2),
        started: WindowStarted::Yes,
    });
    assert_eq!(row.report(), AccountReadingReport::Windows);
}

/// `(name, report, window_secs, scope, length_class, used, reset_ms, started)`.
type RowCase<'a> = (
    &'a str,
    &'a str,
    Option<i64>,
    Option<&'a str>,
    Option<&'a str>,
    Option<f64>,
    Option<i64>,
    Option<&'a str>,
);

fn insert(conn: &Connection, source: &str, case: &RowCase<'_>) -> rusqlite::Result<usize> {
    let &(_, report, window_secs, scope, length_class, used, reset_ms, started) = case;
    conn.execute(
        "INSERT INTO account_readings (ts_ms, account, source, report, window_secs, scope, \
         length_class, used, reset_ms, started) VALUES (1, 'acct', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            source,
            report,
            window_secs,
            scope,
            length_class,
            used,
            reset_ms,
            started
        ],
    )
}

#[test]
fn the_table_accepts_well_formed_rows_and_refuses_a_mixed_or_unknown_shape() {
    // Arrange
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open fresh");
    let accepted: &[RowCase<'_>] = &[
        (
            "a running window",
            "windows",
            Some(18_000),
            None,
            Some("short"),
            Some(0.4),
            Some(9),
            Some("yes"),
        ),
        (
            "an unstarted scoped window with no reset",
            "windows",
            Some(604_800),
            Some("opus"),
            Some("long"),
            Some(0.0),
            None,
            Some("no"),
        ),
        (
            "no windows",
            "no_windows",
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    ];
    let refused: &[RowCase<'_>] = &[
        (
            "no_windows carrying a length",
            "no_windows",
            Some(18_000),
            None,
            None,
            None,
            None,
            None,
        ),
        (
            "no_windows carrying a scope",
            "no_windows",
            None,
            Some("opus"),
            None,
            None,
            None,
            None,
        ),
        (
            "no_windows carrying a reset",
            "no_windows",
            None,
            None,
            None,
            None,
            Some(9),
            None,
        ),
        (
            "windows without a used fraction",
            "windows",
            Some(18_000),
            None,
            Some("short"),
            None,
            Some(9),
            Some("yes"),
        ),
        (
            "windows without a started state",
            "windows",
            Some(18_000),
            None,
            Some("short"),
            Some(0.4),
            Some(9),
            None,
        ),
        (
            "an unknown report token",
            "partial",
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            "an unknown length class",
            "windows",
            Some(18_000),
            None,
            Some("medium"),
            Some(0.4),
            Some(9),
            Some("yes"),
        ),
        (
            "an unknown started token",
            "windows",
            Some(18_000),
            None,
            Some("short"),
            Some(0.4),
            Some(9),
            Some("maybe"),
        ),
    ];

    // Act + Assert
    for case in accepted {
        insert(db.conn(), "poll", case).unwrap_or_else(|e| panic!("{}: {e}", case.0));
    }
    for case in refused {
        assert!(insert(db.conn(), "poll", case).is_err(), "{}", case.0);
    }
    assert!(
        insert(db.conn(), "header", &accepted[0]).is_err(),
        "an unknown source token"
    );
    let rows: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM account_readings", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 3);
}
