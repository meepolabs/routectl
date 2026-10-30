use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tokio::sync::mpsc;

use super::{GapTracker, QUEUE_FULL_CAUSE};
use crate::handle::{UsageCounters, UsageHandle};
use crate::record::UsageRecord;
use crate::schema::CREATE_LEDGER_GAPS_TABLE;
use crate::writer::{UsageWriter, WriterMessage};

/// Bound on any send loop or wait below: a regression that blocks fails here
/// instead of hanging the suite.
const BOUND: Duration = Duration::from_secs(5);

fn record(request_id: &str) -> UsageRecord {
    UsageRecord {
        request_id: request_id.to_string(),
        ..UsageRecord::default()
    }
}

fn handle_over(tx: mpsc::Sender<WriterMessage>, counters: &Arc<UsageCounters>) -> UsageHandle {
    UsageHandle::new(tx, Arc::new(AtomicBool::new(true)), Arc::clone(counters))
}

/// Send `n` records from another thread and fail if the loop does not finish
/// within `BOUND`, so a blocking `try_send` is a failure, not a hang.
fn send_within_bound(handle: &UsageHandle, prefix: &str, n: usize) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let handle = handle.clone();
    let prefix = prefix.to_string();
    std::thread::spawn(move || {
        for i in 0..n {
            handle.try_send(record(&format!("{prefix}-{i}")));
        }
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(BOUND)
        .expect("try_send blocked: the send loop did not finish within the bound");
}

fn gap_rows(path: &Path) -> Vec<(String, i64)> {
    let conn = Connection::open(path).expect("read open");
    let mut stmt = conn
        .prepare("SELECT cause, dropped FROM ledger_gaps ORDER BY rowid")
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + BOUND;
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Fill a capacity-`capacity` channel and overflow it by `overflow`, then take
/// the queued messages back out so the receiver is empty. Returns the
/// receiver with every sender dropped (closed) and the counters carrying
/// exactly `overflow` full drops.
fn overflowed_then_drained(
    capacity: usize,
    overflow: usize,
) -> (mpsc::Receiver<WriterMessage>, Arc<UsageCounters>) {
    let (tx, mut rx) = mpsc::channel::<WriterMessage>(capacity);
    let counters = Arc::new(UsageCounters::default());
    let handle = handle_over(tx, &counters);
    send_within_bound(&handle, "burst", capacity + overflow);
    drop(handle);
    while rx.try_recv().is_ok() {}
    (rx, counters)
}

#[test]
fn saturated_channel_counts_only_full_drops_without_blocking() {
    // Arrange: a capacity-N channel whose receiver is held and never polled.
    let capacity = 4;
    let overflow = 3;
    let (tx, _held_rx) = mpsc::channel::<WriterMessage>(capacity);
    let counters = Arc::new(UsageCounters::default());
    let handle = handle_over(tx, &counters);

    // Act
    send_within_bound(&handle, "sat", capacity + overflow);

    // Assert
    assert_eq!(counters.enqueued(), capacity as u64);
    assert_eq!(counters.dropped_full(), overflow as u64);
    assert_eq!(counters.dropped_closed(), 0);
}

#[test]
fn closed_channel_counts_closed_drops_and_leaves_full_unchanged() {
    // Arrange: overflow first so the full counter has a nonzero baseline,
    // then close the channel by dropping the receiver.
    let capacity = 2;
    let overflow = 3;
    let after_close = 4;
    let (tx, rx) = mpsc::channel::<WriterMessage>(capacity);
    let counters = Arc::new(UsageCounters::default());
    let handle = handle_over(tx, &counters);
    send_within_bound(&handle, "pre", capacity + overflow);
    drop(rx);

    // Act
    send_within_bound(&handle, "post", after_close);

    // Assert
    assert_eq!(counters.dropped_closed(), after_close as u64);
    assert_eq!(counters.dropped_full(), overflow as u64);
}

#[test]
fn next_drain_writes_one_gap_row_and_a_drain_without_new_drops_writes_none() {
    // Arrange: overflow a held channel before the writer consumes it.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let capacity = 4;
    let overflow = 3;
    let (tx, rx) = mpsc::channel::<WriterMessage>(capacity);
    let counters = Arc::new(UsageCounters::default());
    let handle = handle_over(tx, &counters);
    send_within_bound(&handle, "held", capacity + overflow);

    // Act 1: the writer drains the queued rows.
    let writer = UsageWriter::start_over_channel(path.clone(), 0, rx, Arc::clone(&counters));
    assert!(wait_until(|| counters.persisted() == capacity as u64));

    // Assert 1: one gap row with the whole delta, written while the writer
    // is still running (not by the shutdown check).
    assert!(
        wait_until(|| !gap_rows(&path).is_empty()),
        "no gap row while running"
    );
    let expected = vec![(QUEUE_FULL_CAUSE.to_string(), overflow as i64)];
    assert_eq!(gap_rows(&path), expected);

    // Act 2: a later drain with no new drops, then shutdown.
    handle.try_send(record("later"));
    assert!(wait_until(|| counters.persisted() == capacity as u64 + 1));
    let rows_before_shutdown = gap_rows(&path);
    drop(handle);
    writer.shutdown();

    // Assert 2: neither the later drain nor shutdown added a row.
    assert_eq!(rows_before_shutdown, expected);
    assert_eq!(gap_rows(&path), expected);
}

#[test]
fn drops_followed_by_shutdown_with_no_further_message_still_write_the_gap_row() {
    // Arrange: every queued message already taken back out, so the writer
    // receives nothing and only its after-loop check can see the drops.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let overflow = 5;
    let (rx, counters) = overflowed_then_drained(2, overflow);

    // Act
    let writer = UsageWriter::start_over_channel(path.clone(), 0, rx, Arc::clone(&counters));
    writer.shutdown();

    // Assert
    assert_eq!(
        gap_rows(&path),
        vec![(QUEUE_FULL_CAUSE.to_string(), overflow as i64)]
    );
}

#[test]
fn gap_row_survives_a_writer_restart_on_the_same_file() {
    // Arrange: a first writer records a gap and stops.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let overflow = 3;
    let (rx, counters) = overflowed_then_drained(2, overflow);
    UsageWriter::start_over_channel(path.clone(), 0, rx, counters).shutdown();
    let expected = vec![(QUEUE_FULL_CAUSE.to_string(), overflow as i64)];
    assert_eq!(
        gap_rows(&path),
        expected,
        "premise: the first writer recorded the gap"
    );

    // Act: a second writer opens the same file, writes a row, stops.
    let (handle, writer) = UsageWriter::start(path.clone(), 4, 0, true);
    handle.try_send(record("after-restart"));
    assert!(wait_until(|| writer.counters().persisted() == 1));
    drop(handle);
    writer.shutdown();

    // Assert
    assert_eq!(gap_rows(&path), expected);
}

#[test]
fn failed_gap_insert_keeps_the_drops_pending_for_the_next_check() {
    // Arrange: no ledger_gaps table yet, so the first insert fails.
    let conn = Connection::open_in_memory().expect("in-memory db");
    let mut tracker = GapTracker::default();
    assert!(tracker.persist_new_drops(&conn, 4, 1).is_err());
    conn.execute_batch(CREATE_LEDGER_GAPS_TABLE)
        .expect("create table");

    // Act
    let recorded = tracker.persist_new_drops(&conn, 6, 2).expect("insert");
    let repeat = tracker.persist_new_drops(&conn, 6, 3).expect("no-op");

    // Assert: the retry carries every drop, the failed attempt included.
    assert_eq!(recorded, Some(6));
    assert_eq!(repeat, None);
    let dropped: i64 = conn
        .query_row("SELECT SUM(dropped) FROM ledger_gaps", [], |r| r.get(0))
        .expect("sum");
    assert_eq!(dropped, 6);
}
