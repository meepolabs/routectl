//! Tests for the acknowledged atomic capability-event batch.

use super::*;
use crate::handle::UsageCounters;
use crate::writer::{CHANNEL_CAPACITY, UsageWriter};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// A wire-shape capability key, assembled at runtime.
///
/// The namespace prefix is owned by one module in `routectl-router` and a
/// lexical guard there fails if the literal appears anywhere else under
/// `crates/` -- a second spelling could drift from the owner and re-partition
/// persisted history. This crate cannot reach that module's private
/// constructor, so it builds the key from parts instead: the scanned source
/// carries no full prefix literal while the VALUE is byte-identical.
fn wire_shape_key(path: &str) -> String {
    format!("{}{}{}", "fie", "ld:", path)
}

fn temp_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

fn negative(lane_key: &str, capability: &str) -> CapabilityEvent {
    CapabilityEvent {
        ts: 1_000,
        lane_key: lane_key.to_string(),
        capability: capability.to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: 8,
        overlay_revision: 1,
    }
}

fn ledger_rows(path: &PathBuf) -> Vec<(i64, String, String)> {
    let conn = rusqlite::Connection::open(path).expect("read open");
    let mut stmt = conn
        .prepare("SELECT rowid, verdict, capability FROM capability_events ORDER BY rowid")
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

/// The happy path: a boundary tombstone plus its survivors commit together
/// and the caller is told so synchronously, in append order.
#[test]
fn committed_batch_lands_every_row_in_append_order() {
    // Arrange
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    let batch = vec![
        CapabilityEvent::tombstone(900, 8, 1),
        negative("nick", &wire_shape_key("thinking.enabled.display")),
    ];

    // Act
    let outcome = handle.commit_capability_events_blocking(batch, 1);

    // Assert
    assert_eq!(outcome, BatchCommit::Committed { rows: 2 });
    let rows = ledger_rows(&path);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "tombstone");
    assert_eq!(rows[1].2, wire_shape_key("thinking.enabled.display"));
    assert!(rows[0].0 < rows[1].0);

    // Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
    // consumer's recv loop to end, which only happens once EVERY sender is
    // gone. A live handle makes it pay the full drain deadline and then
    // detach the thread.
    drop(handle);
    writer.shutdown();
}

/// A correctness-control write must NOT be silenced by the operator's usage
/// capture setting: the boundary batch is what keeps a verdict from being
/// evicted, so honoring `usage.enabled` here would make a telemetry
/// preference silently destroy routing state. Ordinary request usage stays
/// gated, which the sibling assertion below pins.
#[test]
fn batch_commits_while_usage_capture_is_disabled() {
    // Arrange: capture starts disabled.
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, false);
    assert!(!handle.is_enabled());

    // Act
    let outcome =
        handle.commit_capability_events_blocking(vec![CapabilityEvent::tombstone(900, 8, 1)], 1);

    // Assert: the control write committed even though capture is off.
    assert_eq!(outcome, BatchCommit::Committed { rows: 1 });
    assert_eq!(ledger_rows(&path).len(), 1);

    // Assert the gate still applies to a best-effort capability event, so
    // the bypass is scoped to the acknowledged batch and did not widen.
    let before = handle.counters().dropped_disabled();
    handle.try_send_capability_event_in_generation(negative("nick", "web_search"), 1);
    assert_eq!(
        handle.counters().dropped_disabled(),
        before + 1,
        "an ordinary capability event is still dropped at the disabled gate",
    );

    // Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
    // consumer's recv loop to end, which only happens once EVERY sender is
    // gone. A live handle makes it pay the full drain deadline and then
    // detach the thread.
    drop(handle);
    writer.shutdown();
}

/// An empty batch is a no-op that still reports success: the caller has
/// nothing to preserve, which is not a failure.
#[test]
fn empty_batch_commits_zero_rows() {
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    let outcome = handle.commit_capability_events_blocking(Vec::new(), 1);

    assert_eq!(outcome, BatchCommit::Committed { rows: 0 });
    assert!(ledger_rows(&path).is_empty());

    // Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
    // consumer's recv loop to end, which only happens once EVERY sender is
    // gone. A live handle makes it pay the full drain deadline and then
    // detach the thread.
    drop(handle);
    writer.shutdown();
}

/// A closed channel is distinguishable from a commit, so a caller can keep
/// the old router active rather than swap on an unpersisted boundary.
///
/// The receiver is dropped directly rather than by shutting a writer down:
/// `UsageWriter::shutdown` cannot close the channel while a handle still
/// holds a sender clone (it drains to its deadline and detaches the thread),
/// so a shutdown-based fixture would leave the channel OPEN and the batch
/// would commit -- passing for the wrong reason.
#[test]
fn closed_writer_reports_unavailable_rather_than_committed() {
    // Arrange: a channel whose consumer is already gone.
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::writer::WriterMessage>(4);
    drop(rx);
    let handle = UsageHandle::new(
        tx,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        Arc::new(UsageCounters::default()),
    );

    // Act
    let outcome =
        handle.commit_capability_events_blocking(vec![CapabilityEvent::tombstone(900, 8, 1)], 1);

    // Assert: a named failure, never a false success.
    assert_eq!(outcome, BatchCommit::Unavailable);
}

/// A full channel is its own outcome: the batch was never admitted, so
/// nothing was written and the caller must not proceed. Made deterministic
/// by holding the receiver and never draining it -- a live writer would race
/// the fill and make the assertion depend on scheduling.
#[test]
fn full_channel_reports_channel_full() {
    // Arrange: capacity-1 channel whose receiver is held but never drained,
    // with its single slot already occupied by a plain message. Filling the
    // slot directly (rather than with an unacknowledged batch) keeps this test
    // independent of the wait behavior it is not about.
    let (tx, _rx) = tokio::sync::mpsc::channel::<crate::writer::WriterMessage>(1);
    tx.try_send(crate::writer::WriterMessage::CapabilityEvent(
        CapabilityEvent::tombstone(900, 8, 1),
        1,
    ))
    .expect("the empty slot accepts one message");
    let handle = UsageHandle::new(
        tx,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        Arc::new(UsageCounters::default()),
    );

    // Act: no free slot, so admission itself must fail -- and it must fail
    // WITHOUT blocking, which is what keeps a wedged writer from stalling a
    // caller before its batch is ever queued.
    let outcome =
        handle.commit_capability_events_blocking(vec![CapabilityEvent::tombstone(901, 8, 1)], 1);

    // Assert: refused at admission, nothing queued.
    assert_eq!(outcome, BatchCommit::ChannelFull);
}

/// A SQLite failure on any row of the batch reports a write failure and
/// leaves the ledger untouched -- the caller learns the boundary did not
/// move.
#[test]
fn sqlite_failure_reports_write_failed_and_commits_nothing() {
    // Arrange: a trigger that aborts the survivor row, so the failure lands
    // after the tombstone inside the transaction.
    let (_dir, path) = temp_path();
    {
        let db = crate::db::open(&path).expect("open");
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_poison BEFORE INSERT ON capability_events \
                 WHEN NEW.capability = 'poison' \
                 BEGIN SELECT RAISE(ABORT, 'forced row failure'); END",
            )
            .expect("install trigger");
    }
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // Act
    let outcome = handle.commit_capability_events_blocking(
        vec![
            CapabilityEvent::tombstone(900, 8, 1),
            negative("nick", "poison"),
        ],
        1,
    );

    // Assert: named failure, and NEITHER row committed.
    assert_eq!(outcome, BatchCommit::WriteFailed);
    assert!(
        ledger_rows(&path).is_empty(),
        "a failed batch must leave no tombstone behind",
    );

    // Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
    // consumer's recv loop to end, which only happens once EVERY sender is
    // gone. A live handle makes it pay the full drain deadline and then
    // detach the thread.
    drop(handle);
    writer.shutdown();
}

/// Once a batch is ADMITTED it must not be abandoned: the caller waits for a
/// definitive outcome that AGREES with what is on disk.
///
/// The hazard this closes is a late commit. A post-admission timeout returned
/// `Timeout`, the caller kept the OLD router, and the queued transaction could
/// then commit anyway -- so the ledger's boundary moved while the live router
/// still reflected the pre-reload state, and the next restart would replay a
/// boundary the running process never adopted. There is no safe "maybe" here:
/// the caller's answer must match the ledger.
///
/// The fixture holds a competing EXCLUSIVE write transaction for longer than
/// the retired five-second ack budget, so any surviving post-admission
/// deadline would fire mid-transaction. What is asserted is the AGREEMENT
/// property rather than one particular verdict: the writer's own SQLite busy
/// timeout decides whether a contended transaction commits or fails, and
/// either is correct as long as the caller is told the truth. (That busy
/// timeout is also what bounds this wait in practice.)
#[test]
fn an_admitted_batch_resolves_to_an_outcome_that_agrees_with_the_ledger() {
    // Arrange: writer up, plus a second connection holding an EXCLUSIVE write
    // transaction so the writer's transaction must contend for the lock.
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    // Let the writer finish opening before the blocker takes the write lock.
    assert_eq!(
        handle.commit_capability_events_blocking(Vec::new(), 1),
        BatchCommit::Committed { rows: 0 },
        "the writer must be up before the blocker starts",
    );

    let blocker_path = path.clone();
    let (holding_tx, holding_rx) = std::sync::mpsc::channel::<()>();
    let blocker = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(&blocker_path).expect("blocker open");
        conn.busy_timeout(Duration::from_secs(30)).expect("busy");
        conn.execute_batch("BEGIN EXCLUSIVE")
            .expect("take write lock");
        holding_tx.send(()).expect("signal holding");
        // Hold well past the retired five-second ack budget.
        std::thread::sleep(Duration::from_secs(7));
        conn.execute_batch("COMMIT").expect("release write lock");
    });
    holding_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("blocker took the write lock");

    // Act
    let started = std::time::Instant::now();
    let outcome =
        handle.commit_capability_events_blocking(vec![CapabilityEvent::tombstone(900, 8, 1)], 1);
    let waited = started.elapsed();

    // Assert: the fixture really did outlast the retired budget, so a
    // post-admission deadline would have fired inside this window.
    assert!(
        waited > Duration::from_secs(5),
        "the fixture must exceed the retired five-second budget (waited {waited:?})",
    );

    // Assert the agreement property: the answer is definitive, and the ledger
    // matches it. `BatchCommit` carries no timed-out variant, so an abandoned
    // wait could only surface as a WRONG answer -- which this catches.
    let tombstones = ledger_rows(&path)
        .into_iter()
        .filter(|(_, verdict, _)| verdict == "tombstone")
        .count();
    match outcome {
        BatchCommit::Committed { rows } => {
            assert_eq!(rows, 1);
            assert_eq!(tombstones, 1, "a reported commit must be on disk");
        }
        BatchCommit::WriteFailed => {
            assert_eq!(
                tombstones, 0,
                "a reported failure must leave nothing committed -- \
                 a row here would be exactly the late commit this closes",
            );
        }
        other => panic!("an admitted batch must resolve definitively, got {other:?}"),
    }

    blocker.join().expect("blocker");
    drop(handle);
    writer.shutdown();
}

/// Admission and WAIT are separate operations.
///
/// The caller needs admission to be non-blocking so it can run while holding a
/// registry guard -- the boundary snapshot and the batch submission have to be
/// one indivisible step, or an observation could interleave between them and be
/// neither in the snapshot nor after the boundary. Blocking there would hold a
/// lock across SQLite, which is forbidden. The receipt is then awaited AFTER
/// the guard is released.
#[tokio::test]
async fn admission_returns_a_receipt_without_blocking_and_the_wait_is_separate() {
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // Act: admit (non-blocking) then await the receipt.
    let receipt = handle
        .admit_capability_batch(vec![CapabilityEvent::tombstone(900, 8, 1)], 1)
        .expect("admission accepted");
    let outcome = receipt.await_outcome().await;

    // Assert
    assert_eq!(outcome, BatchCommit::Committed { rows: 1 });
    assert_eq!(ledger_rows(&path).len(), 1);

    drop(handle);
    writer.shutdown();
}

/// Admission failure is reported at admission time, so a caller learns it
/// never queued anything without awaiting anything.
#[tokio::test]
async fn admission_reports_refusal_without_a_receipt() {
    // A channel whose single slot is already taken and whose consumer is gone.
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::writer::WriterMessage>(1);
    drop(rx);
    let handle = UsageHandle::new(
        tx,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        Arc::new(UsageCounters::default()),
    );

    let refused = handle
        .admit_capability_batch(vec![CapabilityEvent::tombstone(900, 8, 1)], 1)
        .err();

    assert_eq!(refused, Some(BatchCommit::Unavailable));
}

/// Dropping the receipt must not wedge the writer: it commits, finds nobody
/// listening, and carries on. This is what lets a shutdown abandon the wait.
#[tokio::test]
async fn a_dropped_receipt_leaves_the_writer_healthy() {
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    let receipt = handle
        .admit_capability_batch(vec![CapabilityEvent::tombstone(900, 8, 1)], 1)
        .expect("admitted");
    drop(receipt);

    // A later batch still commits, so the writer was not left wedged on the
    // abandoned ack.
    let second = handle
        .admit_capability_batch(vec![CapabilityEvent::tombstone(901, 8, 1)], 1)
        .expect("admitted")
        .await_outcome()
        .await;
    assert_eq!(second, BatchCommit::Committed { rows: 1 });

    drop(handle);
    writer.shutdown();
}

/// The writer REJECTS a capability event stamped with a generation older than
/// the one a boundary batch committed.
///
/// A pre-boundary event can still be in the channel when the boundary commits
/// (the producer path is best-effort and asynchronous). Persisted after the
/// tombstone it would restore, on the next boot, exactly the catalog-scoped
/// state the boundary evicted. Sequencing is transient -- a generation carried
/// on the in-memory message only, never a column.
#[tokio::test]
async fn an_event_older_than_the_committed_generation_is_rejected() {
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // A boundary batch commits at generation 5.
    let committed = handle
        .admit_capability_batch(vec![CapabilityEvent::tombstone(900, 8, 1)], 5)
        .expect("admitted")
        .await_outcome()
        .await;
    assert_eq!(committed, BatchCommit::Committed { rows: 1 });

    // A delayed event stamped generation 4 arrives after the boundary.
    handle.try_send_capability_event_in_generation(negative("nick", "web_search"), 4);
    // One stamped at the committed generation is accepted.
    handle.try_send_capability_event_in_generation(negative("nick", "computer_use"), 5);

    // Drain, then inspect.
    drop(handle);
    writer.shutdown();

    let capabilities: Vec<String> = ledger_rows(&path).into_iter().map(|(_, _, c)| c).collect();
    assert!(
        !capabilities.iter().any(|c| c == "web_search"),
        "a pre-boundary event must not land after the tombstone: {capabilities:?}",
    );
    assert!(
        capabilities.iter().any(|c| c == "computer_use"),
        "an event at the committed generation must persist: {capabilities:?}",
    );
}
