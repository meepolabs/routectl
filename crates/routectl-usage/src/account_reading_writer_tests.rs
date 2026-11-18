use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::mpsc;

use super::*;
use crate::handle::{UsageCounters, UsageHandle};
use crate::writer::{CHANNEL_CAPACITY, UsageWriter, WriterMessage};

/// Bound on any wait or send loop below: a regression that blocks or never
/// persists fails here instead of hanging the suite.
const BOUND: Duration = Duration::from_secs(5);

const DAY_MS: i64 = 86_400_000;

fn temp_db() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

fn window_row(ts_ms: i64, account: &str, window: AccountReadingWindow) -> AccountReadingRow {
    AccountReadingRow {
        ts_ms,
        account: account.to_string(),
        source: AccountReadingSource::Poll,
        window: Some(window),
    }
}

fn short_window(used: f64) -> AccountReadingWindow {
    AccountReadingWindow {
        window_secs: 18_000,
        scope: None,
        length_class: WindowLengthClass::Short,
        used,
        reset_ms: Some(1_700_000_900_000),
        started: WindowStarted::Yes,
    }
}

fn no_windows_row(ts_ms: i64, account: &str) -> AccountReadingRow {
    AccountReadingRow {
        ts_ms,
        account: account.to_string(),
        source: AccountReadingSource::Poll,
        window: None,
    }
}

/// Wait until `done` holds or `BOUND` passes; returns whether it held.
fn wait_until(done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + BOUND;
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Stop the writer after every handle clone is gone, so shutdown does not
/// wait out the drain deadline on a still-open channel.
fn stop(handle: UsageHandle, writer: UsageWriter) {
    drop(handle);
    writer.shutdown();
}

/// Every column of one stored row, in table order after `id`.
#[derive(Debug, PartialEq)]
struct StoredRow {
    ts_ms: i64,
    account: String,
    source: String,
    report: String,
    window_secs: Option<i64>,
    scope: Option<String>,
    length_class: Option<String>,
    used: Option<f64>,
    reset_ms: Option<i64>,
    started: Option<String>,
}

fn stored_rows(path: &Path) -> Vec<StoredRow> {
    let conn = Connection::open(path).expect("read open");
    let mut stmt = conn
        .prepare(
            "SELECT ts_ms, account, source, report, window_secs, scope, length_class, used, \
             reset_ms, started FROM account_readings ORDER BY id",
        )
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok(StoredRow {
            ts_ms: r.get(0)?,
            account: r.get(1)?,
            source: r.get(2)?,
            report: r.get(3)?,
            window_secs: r.get(4)?,
            scope: r.get(5)?,
            length_class: r.get(6)?,
            used: r.get(7)?,
            reset_ms: r.get(8)?,
            started: r.get(9)?,
        })
    })
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("collect")
}

#[tokio::test]
async fn writer_round_trips_every_column_and_a_no_windows_row() {
    // Arrange: every field of each row holds a value no other field shares,
    // so a swapped or dropped binding changes what reads back.
    let (_dir, path) = temp_db();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0);
    let windows = vec![
        AccountReadingRow {
            ts_ms: 1_700_000_000_001,
            account: "acct-windows".to_string(),
            source: AccountReadingSource::Response,
            window: Some(AccountReadingWindow {
                window_secs: 18_000,
                scope: None,
                length_class: WindowLengthClass::Short,
                used: 0.25,
                reset_ms: Some(1_700_000_018_002),
                started: WindowStarted::Yes,
            }),
        },
        AccountReadingRow {
            ts_ms: 1_700_000_000_003,
            account: "acct-windows-scoped".to_string(),
            source: AccountReadingSource::Poll,
            window: Some(AccountReadingWindow {
                window_secs: 604_800,
                scope: Some("opus".to_string()),
                length_class: WindowLengthClass::Long,
                used: 0.0,
                reset_ms: None,
                started: WindowStarted::No,
            }),
        },
        AccountReadingRow {
            ts_ms: 1_700_000_000_004,
            account: "acct-windows-unknown".to_string(),
            source: AccountReadingSource::Poll,
            window: Some(AccountReadingWindow {
                window_secs: 86_400,
                scope: Some("sonnet".to_string()),
                length_class: WindowLengthClass::Short,
                used: 0.75,
                reset_ms: Some(1_700_000_086_405),
                started: WindowStarted::Unknown,
            }),
        },
    ];
    let no_windows = vec![no_windows_row(1_700_000_000_006, "acct-none")];

    // Act
    handle.try_send_account_readings(windows);
    handle.try_send_account_readings(no_windows);
    let counters = Arc::clone(handle.counters());
    assert!(
        wait_until(|| counters.account_readings_persisted() == 2),
        "observations not persisted"
    );
    stop(handle, writer);

    // Assert
    let expected = vec![
        StoredRow {
            ts_ms: 1_700_000_000_001,
            account: "acct-windows".to_string(),
            source: "response".to_string(),
            report: "windows".to_string(),
            window_secs: Some(18_000),
            scope: None,
            length_class: Some("short".to_string()),
            used: Some(0.25),
            reset_ms: Some(1_700_000_018_002),
            started: Some("yes".to_string()),
        },
        StoredRow {
            ts_ms: 1_700_000_000_003,
            account: "acct-windows-scoped".to_string(),
            source: "poll".to_string(),
            report: "windows".to_string(),
            window_secs: Some(604_800),
            scope: Some("opus".to_string()),
            length_class: Some("long".to_string()),
            used: Some(0.0),
            reset_ms: None,
            started: Some("no".to_string()),
        },
        StoredRow {
            ts_ms: 1_700_000_000_004,
            account: "acct-windows-unknown".to_string(),
            source: "poll".to_string(),
            report: "windows".to_string(),
            window_secs: Some(86_400),
            scope: Some("sonnet".to_string()),
            length_class: Some("short".to_string()),
            used: Some(0.75),
            reset_ms: Some(1_700_000_086_405),
            started: Some("unknown".to_string()),
        },
        StoredRow {
            ts_ms: 1_700_000_000_006,
            account: "acct-none".to_string(),
            source: "poll".to_string(),
            report: "no_windows".to_string(),
            window_secs: None,
            scope: None,
            length_class: None,
            used: None,
            reset_ms: None,
            started: None,
        },
    ];
    assert_eq!(stored_rows(&path), expected);
    assert_eq!(counters.account_readings_enqueued(), 2);
    assert_eq!(counters.account_readings_write_errors(), 0);
    assert_eq!(
        counters.persisted(),
        0,
        "history must not count as a request row"
    );
}

#[tokio::test]
async fn a_failing_row_rolls_back_its_observation_and_the_writer_keeps_draining() {
    // Arrange: SQLite stores a NaN double as NULL, so a NaN `used` breaks the
    // `windows` shape CHECK on the observation's SECOND row, after the first
    // row has already been inserted inside the transaction.
    let (_dir, path) = temp_db();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0);
    let broken = vec![
        window_row(10, "acct-broken", short_window(0.5)),
        window_row(10, "acct-broken", short_window(f64::NAN)),
    ];
    let later = vec![window_row(20, "acct-later", short_window(0.5))];

    // Act
    handle.try_send_account_readings(broken);
    handle.try_send_account_readings(later);
    let counters = Arc::clone(handle.counters());
    assert!(
        wait_until(|| counters.account_readings_persisted() == 1),
        "the observation after the failure was not persisted"
    );
    stop(handle, writer);

    // Assert: none of the failing observation's rows landed, one write error
    // was counted for it, and the request ledger's error counter is untouched.
    let accounts: Vec<String> = stored_rows(&path).into_iter().map(|r| r.account).collect();
    assert_eq!(accounts, vec!["acct-later"]);
    assert_eq!(counters.account_readings_write_errors(), 1);
    assert_eq!(counters.write_errors(), 0);
}

#[tokio::test]
async fn a_writer_without_a_database_counts_the_observation_as_a_write_error() {
    // Arrange: a path whose parent is a file, so the open fails and the writer
    // runs degraded.
    let dir = TempDir::new().expect("tempdir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").expect("write blocker");
    let (handle, writer) = UsageWriter::start(blocker.join("usage.db"), CHANNEL_CAPACITY, 0);

    // Act
    handle.try_send_account_readings(vec![no_windows_row(1, "acct")]);
    let counters = Arc::clone(handle.counters());
    let counted = wait_until(|| counters.account_readings_write_errors() == 1);
    stop(handle, writer);

    // Assert
    assert!(counted, "the dropped observation was not counted");
    assert_eq!(counters.account_readings_persisted(), 0);
}

/// Send `n` single-row observations from another thread, failing if the loop
/// does not finish within `BOUND`, so a blocking producer is a failure rather
/// than a hang.
fn send_within_bound(handle: &UsageHandle, n: usize) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let handle = handle.clone();
    std::thread::spawn(move || {
        for i in 0..n {
            handle.try_send_account_readings(vec![no_windows_row(1, &format!("acct-{i}"))]);
        }
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(BOUND)
        .expect("the account-reading producer blocked");
}

#[test]
fn full_and_closed_channels_drop_on_distinct_counters_without_blocking() {
    // Arrange: a capacity-1 channel whose receiver is never polled.
    let (tx, rx) = mpsc::channel::<WriterMessage>(1);
    let counters = Arc::new(UsageCounters::default());
    let handle = UsageHandle::new(tx, Arc::clone(&counters));

    // Act: overfill the open channel, then close it and send again.
    send_within_bound(&handle, 3);
    let after_full = (
        counters.account_readings_enqueued(),
        counters.account_readings_dropped_full(),
        counters.account_readings_dropped_closed(),
    );
    drop(rx);
    send_within_bound(&handle, 2);

    // Assert: full and closed land on their own counters, and neither touches
    // the request-row drop counters.
    assert_eq!(after_full, (1, 2, 0));
    assert_eq!(counters.account_readings_enqueued(), 1);
    assert_eq!(counters.account_readings_dropped_full(), 2);
    assert_eq!(counters.account_readings_dropped_closed(), 2);
    assert_eq!(counters.dropped_full(), 0);
    assert_eq!(counters.dropped_closed(), 0);
}

#[tokio::test]
async fn startup_prune_drops_old_readings_and_keeps_in_window_ones() {
    // Arrange: one reading past a 30-day retention and one inside it.
    let (_dir, path) = temp_db();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis(),
    )
    .expect("epoch ms fits i64");
    {
        let db = crate::db::open(&path).expect("seed open");
        insert_account_readings(
            db.conn(),
            &[
                no_windows_row(now - 40 * DAY_MS, "acct-old"),
                no_windows_row(now - DAY_MS, "acct-new"),
            ],
        )
        .expect("seed");
    }

    // Act
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 30);
    stop(handle, writer);

    // Assert
    let accounts: Vec<String> = stored_rows(&path).into_iter().map(|r| r.account).collect();
    assert_eq!(accounts, vec!["acct-new"]);
}
