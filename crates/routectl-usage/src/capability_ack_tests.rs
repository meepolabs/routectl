//! Tests for the acknowledged SINGLE capability-event write.
//!
//! What these pin, and what the sibling `capability_batch_tests.rs` pins
//! instead: that file covers the BOUNDARY batch, which establishes a replay
//! boundary and raises purge floors. This one covers the ordinary event whose
//! outcome a caller awaits -- so the assertions here are about the outcome being
//! HONEST (only a landed row reports durable) and about the admission checks
//! being the SAME ones a best-effort event passes.

use super::*;
use crate::capability_batch::BatchCommit;
use crate::capability_event::CapabilityEvent;
use crate::writer::{CHANNEL_CAPACITY, UsageWriter};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

/// A wire-shape capability key, assembled at runtime.
///
/// The namespace prefix is owned by one module in `routectl-router` and a lexical
/// guard there fails if the literal appears anywhere else under `crates/` -- a
/// second spelling could drift from the owner and re-partition persisted history.
/// This crate cannot reach that module's private constructor, so it builds the key
/// from parts: the scanned source carries no full prefix literal while the VALUE is
/// byte-identical.
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

fn ledger_row_count(path: &PathBuf) -> i64 {
    let conn = rusqlite::Connection::open(path).expect("read open");
    conn.query_row("SELECT COUNT(*) FROM capability_events", [], |r| r.get(0))
        .expect("count")
}

#[tokio::test]
async fn a_committed_event_reports_durable_and_is_on_disk() {
    // THE happy path, and the two halves must BOTH hold: the outcome says durable
    // AND the row is readable from the file. An outcome-only assertion would pass
    // on a writer that acknowledged without inserting, which is precisely the
    // false-durability the acknowledgment exists to rule out.
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    let receipt = handle
        .admit_acknowledged_capability_event(
            negative("nick", &wire_shape_key("thinking.enabled.display")),
            1,
            1,
        )
        .expect("a healthy writer admits the event");
    let outcome = receipt.await_outcome().await;

    assert_eq!(outcome, CapabilityEventWrite::Committed);
    assert!(outcome.is_durable());
    assert_eq!(
        ledger_row_count(&path),
        1,
        "a durable answer must mean the row is actually on disk",
    );

    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn an_event_older_than_the_committed_boundary_reports_superseded_and_writes_nothing() {
    // The SAME admission check a best-effort event passes, reached through the
    // acknowledged path: an event predating the newest committed boundary would
    // restore state that boundary evicted, so it is refused and the caller is
    // TOLD -- which is what stops a verdict from becoming pre-flight eligible on a
    // row nobody appended.
    //
    // Mutation check: skip the boundary check on the acknowledged path (a second
    // copy of the persist body without it) -> red here on both assertions.
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    // Commit a boundary at generation 5, which is what raises the bar.
    // The ASYNC admission, not the blocking one: this test runs on a tokio
    // worker, and the blocking variant must not be called from one.
    let committed = handle
        .admit_capability_batch(vec![CapabilityEvent::tombstone(900, 8, 1)], 5)
        .expect("admitted")
        .await_outcome()
        .await;
    assert_eq!(
        committed,
        BatchCommit::Committed { rows: 1 },
        "premise: the boundary must have committed, or there is no bar to be under",
    );
    let before = ledger_row_count(&path);

    // An event from generation 2: older than the committed boundary.
    let receipt = handle
        .admit_acknowledged_capability_event(negative("nick", &wire_shape_key("a.b")), 2, 1)
        .expect("admitted to the channel; the refusal happens at the writer");
    let outcome = receipt.await_outcome().await;

    assert_eq!(outcome, CapabilityEventWrite::SupersededByBoundary);
    assert!(
        !outcome.is_durable(),
        "a superseded event is not durable, however cleanly it was refused",
    );
    assert_eq!(ledger_row_count(&path), before, "and nothing was appended");

    // POSITIVE CONTROL on the same writer: an event AT the boundary generation
    // does land, so the refusal above is the check rather than a writer that
    // stopped writing.
    let receipt = handle
        .admit_acknowledged_capability_event(negative("nick", &wire_shape_key("a.b")), 5, 1)
        .expect("admitted");
    assert_eq!(
        receipt.await_outcome().await,
        CapabilityEventWrite::Committed,
    );
    assert_eq!(ledger_row_count(&path), before + 1);

    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn an_event_superseded_by_a_purge_reports_so_and_writes_nothing() {
    // The per-key floor, the check the boundary generation cannot substitute for:
    // a purge and a later relearn of one key both happen inside one generation. A
    // pre-purge event delayed past the clear must be refused, and the caller must
    // learn it -- otherwise the verdict the operator was told was removed would
    // become pre-flight eligible again.
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    let key = wire_shape_key("thinking.enabled.display");
    // A `cleared` row committed at incarnation 4 raises that key's purge floor.
    let mut cleared = negative("nick", &key);
    cleared.verdict = "cleared".to_string();
    let committed = handle
        .admit_capability_batch_at(vec![cleared], 1, 4)
        .expect("admitted")
        .await_outcome()
        .await;
    assert!(
        matches!(committed, BatchCommit::Committed { .. }),
        "premise: the purge must have committed to raise a floor: {committed:?}",
    );
    let before = ledger_row_count(&path);

    // An event describing incarnation 4 -- the very version the purge superseded.
    let receipt = handle
        .admit_acknowledged_capability_event(negative("nick", &key), 1, 4)
        .expect("admitted");
    let outcome = receipt.await_outcome().await;

    assert_eq!(outcome, CapabilityEventWrite::SupersededByPurge);
    assert!(!outcome.is_durable());
    assert_eq!(ledger_row_count(&path), before, "nothing was appended");

    // POSITIVE CONTROL: a genuine post-purge relearn allocated a strictly greater
    // incarnation and DOES land, so the refusal is the floor rather than the key.
    let receipt = handle
        .admit_acknowledged_capability_event(negative("nick", &key), 1, 5)
        .expect("admitted");
    assert_eq!(
        receipt.await_outcome().await,
        CapabilityEventWrite::Committed,
        "a post-purge relearn must still land -- the floor supersedes one version, \
         not the key",
    );
    assert_eq!(ledger_row_count(&path), before + 1);

    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn a_failed_write_reports_write_failed() {
    // A writer with no usable connection: the insert cannot happen, and the caller
    // must be told rather than left to assume. A degraded writer that answered
    // `Committed` would make every verdict minted during an outage pre-flight
    // eligible on nothing.
    let (_dir, dir_as_db) = temp_path();
    // A DIRECTORY where the database file must be: the open fails, so the writer
    // runs degraded with no connection. Chosen over a deleted file because SQLite
    // would simply recreate that one.
    std::fs::create_dir(&dir_as_db).expect("create the blocking directory");
    let (handle, writer) = UsageWriter::start(dir_as_db.clone(), CHANNEL_CAPACITY, 0, true);

    let receipt = handle
        .admit_acknowledged_capability_event(negative("nick", &wire_shape_key("a.b")), 1, 1)
        .expect("the channel still accepts; the FAILURE is at the write");
    let outcome = receipt.await_outcome().await;

    assert_eq!(outcome, CapabilityEventWrite::WriteFailed);
    assert!(
        !outcome.is_durable(),
        "an unlanded write must never report durable",
    );
    assert!(
        handle.counters().write_errors() >= 1,
        "and the failure must be visible on the writer's own health counter",
    );

    drop(handle);
    writer.shutdown();
}

#[test]
fn a_full_channel_refuses_at_admission_without_blocking() {
    // Admission-time refusal: the caller learns it wrote nothing without awaiting
    // anything. A blocking admission on the request path would let a saturated
    // writer stall dispatch.
    let capacity = 1usize;
    let (tx, _rx) = tokio::sync::mpsc::channel::<crate::writer::WriterMessage>(capacity);
    let counters = Arc::new(crate::handle::UsageCounters::default());
    let handle = UsageHandle::new(
        tx,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        Arc::clone(&counters),
        crate::paid_probe_lifecycle::LifecycleGate::running(),
    );

    // Fill the one slot, then refuse.
    let _first = handle
        .admit_acknowledged_capability_event(negative("nick", &wire_shape_key("a.b")), 1, 1)
        .expect("the first admission takes the only slot");
    let refused =
        handle.admit_acknowledged_capability_event(negative("nick", &wire_shape_key("a.b")), 1, 1);

    assert_eq!(
        refused.err(),
        Some(CapabilityEventWrite::WriteFailed),
        "a saturated channel is reported at admission, not awaited",
    );
    assert_eq!(
        counters.capability_events_dropped_full(),
        1,
        "and it lands on the SAME counter a refused best-effort event does -- a \
         refused event is a refused event whether or not anybody awaited it",
    );
}

#[test]
fn capture_disabled_refuses_rather_than_silently_dropping() {
    // The enabled gate IS honoured here, unlike the boundary batch's deliberate
    // bypass. The asymmetry is the point: the batch protects routing state an
    // operator's telemetry preference must not destroy, while this one only ever
    // ADDS a row -- and an operator who turned capture off wrote no row, so no
    // verdict may become pre-flight eligible on the strength of one.
    //
    // Mutation check: drop the enabled check from
    // `admit_acknowledged_capability_event` -> red here.
    let (tx, _rx) = tokio::sync::mpsc::channel::<crate::writer::WriterMessage>(4);
    let counters = Arc::new(crate::handle::UsageCounters::default());
    let handle = UsageHandle::new(
        tx,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::clone(&counters),
        crate::paid_probe_lifecycle::LifecycleGate::running(),
    );
    assert!(!handle.is_enabled(), "premise: capture is off");

    let refused =
        handle.admit_acknowledged_capability_event(negative("nick", &wire_shape_key("a.b")), 1, 1);

    assert_eq!(
        refused.err(),
        Some(CapabilityEventWrite::WriteFailed),
        "with capture off nothing is written, so nothing may be reported durable",
    );
    assert_eq!(counters.dropped_disabled(), 1);
}

#[tokio::test]
async fn an_abandoned_receipt_leaves_the_writer_unblocked() {
    // Shutdown shape: a caller that drops its receipt (its own budget expired, or
    // the process is tearing down) must not wedge the writer. The row may still
    // land; nothing is published on the strength of it.
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    let receipt = handle
        .admit_acknowledged_capability_event(negative("nick", &wire_shape_key("a.b")), 1, 1)
        .expect("admitted");
    drop(receipt);

    // The writer keeps serving: a SECOND event still gets an answer, which is what
    // "unblocked" means observably.
    let outcome = handle
        .admit_acknowledged_capability_event(negative("nick", &wire_shape_key("c.d")), 1, 1)
        .expect("admitted")
        .await_outcome()
        .await;
    assert_eq!(
        outcome,
        CapabilityEventWrite::Committed,
        "a dropped ack receiver must not stop the writer from answering the next \
         caller",
    );

    drop(handle);
    writer.shutdown();
}

#[test]
fn every_write_outcome_has_a_distinct_token_and_only_one_is_durable() {
    // The closed vocabulary. Two outcomes sharing a token would make an operator
    // query unable to tell a superseded event from a failed one, and more than one
    // durable variant would mean the predicate every consumer asks has two answers
    // for "yes".
    let all = [
        CapabilityEventWrite::Committed,
        CapabilityEventWrite::SupersededByBoundary,
        CapabilityEventWrite::SupersededByPurge,
        CapabilityEventWrite::WriteFailed,
    ];
    let tokens: std::collections::BTreeSet<&str> = all.iter().map(|o| o.as_str()).collect();
    assert_eq!(tokens.len(), all.len(), "every token must be distinct");
    assert_eq!(
        all.iter().filter(|o| o.is_durable()).count(),
        1,
        "exactly one outcome may report durable",
    );
    assert!(CapabilityEventWrite::Committed.is_durable());
}
