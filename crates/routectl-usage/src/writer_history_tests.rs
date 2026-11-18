//! Tests for how an account-reading history write feeds the writer's shared
//! health: a database that stopped taking writes turns it red, a refused row
//! does not.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;

use super::*;
use crate::account_reading::{
    AccountReadingRow, AccountReadingSource, AccountReadingWindow, WindowLengthClass, WindowStarted,
};

/// A writer state over `conn`, healthy, as a successful open leaves it.
fn state_over(conn: Connection) -> WriterState {
    WriterState {
        conn: Some(conn),
        degraded: false,
        boundary_generation: 0,
        purge_floors: fresh_purge_floors(),
        gaps: GapTracker::default(),
    }
}

fn row(account: String, used: f64) -> AccountReadingRow {
    AccountReadingRow {
        ts_ms: 1,
        account,
        source: AccountReadingSource::Poll,
        window: Some(AccountReadingWindow {
            window_secs: 18_000,
            scope: None,
            length_class: WindowLengthClass::Short,
            used,
            reset_ms: Some(2),
            started: WindowStarted::Yes,
        }),
    }
}

/// One observation large enough to need pages the file does not have yet.
fn observation_needing_new_pages() -> Vec<AccountReadingRow> {
    (0..16)
        .map(|i| row(format!("{i}-{}", "a".repeat(4_000)), 0.5))
        .collect()
}

fn fresh_db(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("usage.db");
    drop(crate::db::open(&path).expect("open fresh"));
    path
}

/// The writer's own connection, capped at the pages the file already has, so
/// any insert that needs a new page fails with SQLITE_FULL.
fn full_disk_connection(path: &Path) -> Connection {
    let conn = crate::db::open(path).expect("open").into_conn();
    let pages: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("page_count");
    let capped: i64 = conn
        .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |r| {
            r.get(0)
        })
        .expect("max_page_count");
    assert_eq!(capped, pages, "premise: the cap took");
    conn
}

fn read_only_connection(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("read-only open")
}

/// `(name, how the writer's connection is opened, the observation sent)`.
type StorageCase = (
    &'static str,
    fn(&Path) -> Connection,
    Vec<AccountReadingRow>,
);

#[test]
fn a_storage_failure_on_a_history_write_reaches_the_shared_write_errors() {
    let cases: [StorageCase; 2] = [
        (
            "disk full",
            full_disk_connection,
            observation_needing_new_pages(),
        ),
        (
            "read-only database",
            read_only_connection,
            vec![row("acct".to_string(), 0.5)],
        ),
    ];
    for (name, connect, rows) in cases {
        // Arrange
        let dir = TempDir::new().expect("tempdir");
        let path = fresh_db(&dir);
        let mut state = state_over(connect(&path));
        let counters = Arc::new(UsageCounters::default());

        // Act
        state.persist_account_readings(&rows, &counters);

        // Assert: counted on both counters, and the writer is degraded.
        assert_eq!(counters.account_readings_write_errors(), 1, "{name}");
        assert_eq!(counters.write_errors(), 1, "{name}");
        assert!(state.degraded, "{name}");
        assert_eq!(counters.account_readings_persisted(), 0, "{name}");
    }
}

#[test]
fn a_row_the_table_refuses_stays_off_the_shared_write_errors() {
    let cases = [
        ("used above one", 1.5),
        ("NaN used, stored as NULL", f64::NAN),
    ];
    for (name, used) in cases {
        // Arrange
        let dir = TempDir::new().expect("tempdir");
        let path = fresh_db(&dir);
        let mut state = state_over(crate::db::open(&path).expect("open").into_conn());
        let counters = Arc::new(UsageCounters::default());

        // Act
        state.persist_account_readings(&[row("acct".to_string(), used)], &counters);

        // Assert
        assert_eq!(counters.account_readings_write_errors(), 1, "{name}");
        assert_eq!(counters.write_errors(), 0, "{name}");
        assert!(!state.degraded, "{name}");
    }
}

#[test]
fn a_committed_history_write_marks_a_degraded_writer_healthy() {
    // Arrange
    let dir = TempDir::new().expect("tempdir");
    let path = fresh_db(&dir);
    let mut state = state_over(crate::db::open(&path).expect("open").into_conn());
    state.degraded = true;
    let counters = Arc::new(UsageCounters::default());

    // Act
    state.persist_account_readings(&[row("acct".to_string(), 0.5)], &counters);

    // Assert
    assert_eq!(counters.account_readings_persisted(), 1);
    assert!(!state.degraded);
}
