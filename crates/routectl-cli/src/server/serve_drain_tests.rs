//! A usage writer stopped while a producer handle is still alive reports its
//! drain as timed out, says whether rows were left queued, and the daemon's
//! closing line never claims the writer drained.

use std::path::Path;
use std::time::{Duration, Instant};

use routectl_testkit::{CapturedEvent, capture_events};
use routectl_usage::{CHANNEL_CAPACITY, ShutdownOutcome, UsageHandle, UsageRecord, UsageWriter};
use rusqlite::Connection;

use super::log_usage_drain_outcome;

const TEST_DRAIN_DEADLINE: Duration = Duration::from_millis(200);
const QUEUED_ROWS: u64 = 3;

fn row(request_id: &str) -> UsageRecord {
    UsageRecord {
        request_id: request_id.to_string(),
        ..UsageRecord::default()
    }
}

fn wait_persisted(handle: &UsageHandle, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.counters().persisted() < want {
        assert!(
            Instant::now() < deadline,
            "the writer never persisted a row"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A writer whose consumer is parked in `blocking_recv`: its database is open
/// and one row is on disk, so nothing is left queued.
fn writer_with_an_empty_queue(path: &Path) -> (UsageHandle, UsageWriter, Option<Connection>) {
    let (handle, writer) = UsageWriter::start(path.to_path_buf(), CHANNEL_CAPACITY, 0, true);
    handle.try_send(row("before-shutdown"));
    wait_persisted(&handle, 1);
    (handle, writer, None)
}

/// A writer whose consumer cannot reach the channel: another connection holds
/// the write lock, so its startup work waits in the busy handler, and every
/// row sent stays queued.
fn writer_with_queued_rows(path: &Path) -> (UsageHandle, UsageWriter, Option<Connection>) {
    drop(routectl_usage::open(path).expect("seed db"));
    let lock = Connection::open(path).expect("lock connection");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");
    let (handle, writer) = UsageWriter::start(path.to_path_buf(), CHANNEL_CAPACITY, 1, true);
    for i in 0..QUEUED_ROWS {
        handle.try_send(row(&format!("queued-{i}")));
    }
    (handle, writer, Some(lock))
}

fn messages(events: &[CapturedEvent]) -> Vec<&str> {
    events.iter().map(|e| e.message.as_str()).collect()
}

#[test]
fn shutdown_with_a_held_sender_reports_timed_out_and_never_logs_drained() {
    type Arrange = fn(&Path) -> (UsageHandle, UsageWriter, Option<Connection>);
    let rows: [(&str, Arrange, &str, Option<String>); 2] = [
        (
            "queued rows",
            writer_with_queued_rows,
            "usage writer drain deadline exceeded -- queued rows abandoned",
            Some(QUEUED_ROWS.to_string()),
        ),
        (
            "empty queue",
            writer_with_an_empty_queue,
            "usage writer drain deadline exceeded -- senders still open, queue empty",
            None,
        ),
    ];

    for (name, arrange, expected_warn, expected_abandoned) in rows {
        // Arrange: the handle stays alive across the shutdown.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("usage.db");
        let (handle, writer, lock) = arrange(&path);
        let writer = writer.with_drain_deadline(TEST_DRAIN_DEADLINE);

        // Act
        let mut outcome = None;
        let events = capture_events(|| {
            let result = writer.shutdown();
            log_usage_drain_outcome(result);
            outcome = Some(result);
        });
        drop(lock);
        drop(handle);

        // Assert
        assert_eq!(outcome, Some(ShutdownOutcome::TimedOut), "{name}");
        let warn = events
            .iter()
            .find(|e| e.message == expected_warn)
            .unwrap_or_else(|| panic!("{name}: no `{expected_warn}` in {:?}", messages(&events)));
        assert_eq!(
            warn.field("abandoned").map(str::to_string),
            expected_abandoned,
            "{name}: abandoned count"
        );
        assert!(
            messages(&events)
                .contains(&"usage writer stopped at its drain deadline; queued rows may be lost"),
            "{name}: the daemon must log the deadline stop, got {:?}",
            messages(&events)
        );
        assert!(
            !messages(&events)
                .iter()
                .any(|m| m.starts_with("usage writer drained")),
            "{name}: a timed-out drain must never log drained, got {:?}",
            messages(&events)
        );
    }
}
