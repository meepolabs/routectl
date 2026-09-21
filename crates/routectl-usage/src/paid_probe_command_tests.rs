//! Tests for the acknowledged paid-probe reservation.
//!
//! Every case that asserts durability drives a REAL writer over a REAL
//! file-backed database, and reads the committed state back through a
//! connection the writer does not own -- an in-memory or single-connection
//! fixture cannot represent either the ack-after-commit ordering or the
//! cross-process cap.

use super::*;
use crate::handle::{UsageCounters, UsageHandle};
use crate::paid_probe::{reservation_key, utc_day_from_epoch_ms};
use crate::record::UsageRecord;
use crate::writer::{CHANNEL_CAPACITY, UsageWriter, WriterMessage};
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Wall-clock epoch milliseconds, as the writer itself samples them.
///
/// The tests bracket a reservation with two readings rather than pinning one,
/// because the writer samples its OWN clock -- a test that assumed a single day
/// would be a rare failure at a UTC midnight, and pinning the day would mean
/// passing it across a boundary that deliberately does not carry it.
fn wall_clock_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_millis(),
    )
    .expect("representable")
}

/// A temp directory plus the database path inside it.
fn temp_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

/// The committed unit count for one provider-day, read through a connection
/// the writer does not own.
///
/// Reads the RAW control key deliberately: the day never crosses the public
/// boundary, so a test that wants to assert which day the writer chose has to
/// name the key itself.
fn stored_units(path: &Path, utc_day: i64, provider: &str) -> Option<String> {
    let reader = crate::db::open_readonly(path).expect("read-only open");
    let key = reservation_key(utc_day, provider);
    reader
        .conn()
        .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .expect("read stored units")
}

/// Every reservation key present in the database, whatever day it names.
fn reservation_keys(path: &Path) -> Vec<String> {
    let reader = crate::db::open_readonly(path).expect("read-only open");
    let mut stmt = reader
        .conn()
        .prepare("SELECT key FROM meta WHERE key LIKE 'paid_probe_reservation:%' ORDER BY key")
        .expect("prepare");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("keys")
}

/// Admit one reservation and await its outcome, failing loudly if the writer
/// refused admission -- the cases that expect a refusal assert on the
/// admission directly instead.
async fn reserve_through(handle: &UsageHandle, provider: &str, cap: u32) -> PaidProbeCommit {
    match handle.admit_paid_probe_reservation(provider, cap) {
        PaidProbeAdmission::Admitted(receipt) => receipt.await_outcome().await,
        PaidProbeAdmission::Saturated => panic!("a live writer refused admission as saturated"),
        PaidProbeAdmission::Unavailable => panic!("a live writer refused admission as unavailable"),
    }
}

/// A handle over a channel the caller owns, for the admission cases a live
/// writer cannot produce.
fn handle_over(sender: tokio::sync::mpsc::Sender<WriterMessage>) -> UsageHandle {
    UsageHandle::new(
        sender,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        Arc::new(UsageCounters::default()),
        crate::paid_probe_lifecycle::LifecycleGate::running(),
    )
}

/// Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
/// consumer's recv loop to end, which only happens once EVERY sender is gone.
/// A live handle makes it pay the full drain deadline and then detach.
fn stop(handle: UsageHandle, writer: UsageWriter) {
    drop(handle);
    writer.shutdown();
}

/// The happy path, and the ordering the whole feature rests on: when the
/// caller is told a unit committed, that unit is ALREADY readable by another
/// connection. An answer produced before the transaction would let a paid call
/// go out against a unit that never landed.
#[tokio::test]
async fn a_committed_unit_is_readable_from_another_connection_when_the_caller_is_told() {
    // Arrange
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    let before_day = utc_day_from_epoch_ms(wall_clock_ms());

    // Act
    let outcome = reserve_through(&handle, "anthropic", 3).await;
    let after_day = utc_day_from_epoch_ms(wall_clock_ms());

    // Assert
    assert_eq!(outcome, PaidProbeCommit::Committed { used: 1, cap: 3 });
    let mut days = vec![before_day, after_day];
    days.dedup();
    let visible = days
        .iter()
        .filter_map(|day| stored_units(&path, *day, "anthropic"))
        .collect::<Vec<_>>();
    assert_eq!(
        visible,
        vec!["1".to_string()],
        "the committed unit must be durable the moment the caller is told",
    );

    stop(handle, writer);
}

/// The writer samples the clock itself, so the day a unit lands in is the
/// writer's own -- asserted through the RAW control key, because the day is
/// deliberately absent from every public type.
#[tokio::test]
async fn the_writer_commits_into_its_own_current_utc_day() {
    // Arrange
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // Act: bracket the call, so the acceptable day set is exactly the days the
    // call could have spanned.
    let before_day = utc_day_from_epoch_ms(wall_clock_ms());
    let outcome = reserve_through(&handle, "anthropic", 2).await;
    let after_day = utc_day_from_epoch_ms(wall_clock_ms());

    // Assert
    assert_eq!(outcome, PaidProbeCommit::Committed { used: 1, cap: 2 });
    let keys = reservation_keys(&path);
    assert_eq!(keys.len(), 1, "exactly one provider-day bucket: {keys:?}");
    let acceptable = [
        reservation_key(before_day, "anthropic"),
        reservation_key(after_day, "anthropic"),
    ];
    assert!(
        acceptable.contains(&keys[0]),
        "the key must name the writer's own current day: {keys:?} not in {acceptable:?}",
    );

    stop(handle, writer);
}

/// A spend ceiling must not be silenced by a telemetry preference: with usage
/// capture disabled the reservation still commits, while an ordinary usage
/// record on the SAME handle is still dropped at the gate. Both halves matter
/// -- the first is the bypass, the second is proof the bypass did not widen.
#[tokio::test]
async fn a_reservation_commits_while_usage_capture_is_disabled() {
    // Arrange: capture starts disabled.
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, false);
    assert!(!handle.is_enabled());
    let dropped_before = handle.counters().dropped_disabled();

    // Act
    let outcome = reserve_through(&handle, "anthropic", 2).await;
    handle.try_send(UsageRecord::default());

    // Assert: the control write landed even with capture off.
    assert_eq!(outcome, PaidProbeCommit::Committed { used: 1, cap: 2 });

    // Assert: the ordinary usage row did not, so the bypass is scoped to the
    // reservation.
    assert_eq!(
        handle.counters().dropped_disabled(),
        dropped_before + 1,
        "an ordinary usage record is still dropped at the disabled gate",
    );
    assert_eq!(handle.counters().enqueued(), 0);

    stop(handle, writer);
}

/// A saturated channel is refused AT ADMISSION and authorizes nothing. Made
/// deterministic by holding a receiver that is never polled -- a live writer
/// would race the fill.
///
/// The admission runs on a blocking task under a bounded wait, so an
/// implementation that queued or waited instead of shedding fails with a
/// diagnosis rather than hanging the suite.
#[tokio::test]
async fn a_saturated_channel_is_refused_without_queueing_or_blocking() {
    // Arrange: a capacity-1 channel whose only slot is already taken, and
    // whose receiver is held but never drained.
    let (tx, _rx) = tokio::sync::mpsc::channel::<WriterMessage>(1);
    tx.try_send(WriterMessage::request(Box::<UsageRecord>::default()))
        .expect("the empty slot accepts one message");

    // Assert the premise: the channel really is full, so the refusal below is
    // saturation and not a broken fixture.
    assert!(
        tx.try_send(WriterMessage::request(Box::<UsageRecord>::default()))
            .is_err(),
        "the fixture channel must be full before the reservation is offered",
    );
    let handle = handle_over(tx);

    // Act
    let admission = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            matches!(
                handle.admit_paid_probe_reservation("anthropic", 3),
                PaidProbeAdmission::Saturated
            )
        }),
    )
    .await
    .expect("admission must not block on a full channel")
    .expect("admission task");

    // Assert
    assert!(
        admission,
        "a full channel must be refused as saturated, authorizing nothing",
    );
}

/// A closed channel is its own refusal: nothing was submitted and nothing can
/// be, so no paid call may follow.
///
/// The receiver is dropped directly rather than by shutting a writer down:
/// `UsageWriter::shutdown` cannot close the channel while a handle still holds
/// a sender clone, so a shutdown-based fixture would leave the channel OPEN and
/// the reservation would commit -- passing for the wrong reason.
#[tokio::test]
async fn a_closed_channel_is_refused_as_unavailable() {
    // Arrange
    let (tx, rx) = tokio::sync::mpsc::channel::<WriterMessage>(4);
    drop(rx);
    let handle = handle_over(tx);

    // Act
    let admission = handle.admit_paid_probe_reservation("anthropic", 3);

    // Assert
    assert!(matches!(admission, PaidProbeAdmission::Unavailable));
}

/// One writer holds the cap under contention: many concurrent callers, exactly
/// `cap` commits, every other answer an exhausted cap and no other class.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_against_one_writer_commit_exactly_the_cap() {
    // Arrange
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    let cap = 4_u32;
    let callers = 24_usize;

    // Act
    let mut tasks = Vec::with_capacity(callers);
    for _ in 0..callers {
        let handle = handle.clone();
        tasks.push(tokio::spawn(async move {
            reserve_through(&handle, "anthropic", cap).await
        }));
    }
    let mut outcomes = Vec::with_capacity(callers);
    for task in tasks {
        outcomes.push(task.await.expect("reservation task"));
    }

    // Assert
    let committed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, PaidProbeCommit::Committed { .. }))
        .count();
    assert_eq!(
        committed, cap as usize,
        "exactly the cap may commit: {outcomes:?}",
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == PaidProbeCommit::CapExhausted)
            .count(),
        callers - cap as usize,
        "every refusal must be an exhausted cap: {outcomes:?}",
    );
    let mut used: Vec<u32> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            PaidProbeCommit::Committed { used, .. } => Some(*used),
            _ => None,
        })
        .collect();
    used.sort_unstable();
    assert_eq!(
        used,
        (1..=cap).collect::<Vec<_>>(),
        "each commit reports its own place in the day's count",
    );

    stop(handle, writer);
}

/// Two writers sharing one database never exceed the cap between them -- the
/// per-provider-day ceiling is a property of the STORED state, not of one
/// process's bookkeeping, which is what makes it survive a second daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_writers_on_one_database_commit_at_most_the_cap() {
    // Arrange: migrate the file ONCE before either writer opens it, so the two
    // writers cannot race each other's migration transactions.
    let (_dir, path) = temp_path();
    drop(crate::db::open(&path).expect("migrating open"));
    let (first, first_writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    let (second, second_writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    let cap = 5_u32;

    // Act: both writers are driven concurrently against the same cap.
    let mut tasks = Vec::new();
    for handle in [&first, &second] {
        for _ in 0..8 {
            let handle = handle.clone();
            tasks.push(tokio::spawn(async move {
                reserve_through(&handle, "anthropic", cap).await
            }));
        }
    }
    let mut outcomes = Vec::new();
    for task in tasks {
        outcomes.push(task.await.expect("reservation task"));
    }

    // Assert
    let committed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, PaidProbeCommit::Committed { .. }))
        .count();
    assert!(
        committed <= cap as usize,
        "two writers must not exceed one day's cap: {committed} of {cap} in {outcomes:?}",
    );
    assert!(
        committed >= 1,
        "the premise: at least one unit must commit, or the bound is vacuous",
    );
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    let stored: u32 = stored_units(&path, day, "anthropic")
        .or_else(|| stored_units(&path, day - 1, "anthropic"))
        .expect("a stored count")
        .parse()
        .expect("canonical stored count");
    assert!(
        stored <= cap,
        "the stored count is the ceiling that survives a second process: {stored}",
    );

    stop(first, first_writer);
    stop(second, second_writer);
}

/// A restart continues a day's accounting rather than starting it over: the
/// budget lives in the database, so a crash loop cannot re-spend a spent cap.
#[tokio::test]
async fn a_restarted_writer_continues_the_days_committed_count() {
    // Arrange: spend two of three units, then shut the writer down entirely.
    let (_dir, path) = temp_path();
    let cap = 3_u32;
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    assert_eq!(
        reserve_through(&handle, "anthropic", cap).await,
        PaidProbeCommit::Committed { used: 1, cap }
    );
    assert_eq!(
        reserve_through(&handle, "anthropic", cap).await,
        PaidProbeCommit::Committed { used: 2, cap }
    );
    stop(handle, writer);

    // Act: a fresh writer over the same file.
    let (restarted, restarted_writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    let third = reserve_through(&restarted, "anthropic", cap).await;
    let fourth = reserve_through(&restarted, "anthropic", cap).await;

    // Assert: the day resumes at three, not at one.
    assert_eq!(third, PaidProbeCommit::Committed { used: 3, cap });
    assert_eq!(fourth, PaidProbeCommit::CapExhausted);

    stop(restarted, restarted_writer);
}

/// Each configured provider holds its own budget: one provider exhausting its
/// day must not spend another's.
#[tokio::test]
async fn providers_hold_independent_budgets() {
    // Arrange
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // Act: spend the first provider's single unit, then ask again for both.
    let first = reserve_through(&handle, "anthropic", 1).await;
    let first_again = reserve_through(&handle, "anthropic", 1).await;
    let other = reserve_through(&handle, "openai", 1).await;

    // Assert
    assert_eq!(first, PaidProbeCommit::Committed { used: 1, cap: 1 });
    assert_eq!(first_again, PaidProbeCommit::CapExhausted);
    assert_eq!(other, PaidProbeCommit::Committed { used: 1, cap: 1 });

    stop(handle, writer);
}

/// Dropping the receipt loses the ANSWER and nothing else: the unit stays
/// committed, the writer stays healthy, and a later reservation is served
/// normally. Nothing is rolled back or given back, ever.
#[tokio::test]
async fn an_abandoned_receipt_keeps_its_unit_and_leaves_the_writer_healthy() {
    // Arrange
    let (_dir, path) = temp_path();
    let cap = 4_u32;
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // Act: admit, then abandon the wait before the outcome can be read.
    match handle.admit_paid_probe_reservation("anthropic", cap) {
        PaidProbeAdmission::Admitted(receipt) => drop(receipt),
        other => panic!(
            "a live writer must admit: {}",
            match other {
                PaidProbeAdmission::Saturated => "saturated",
                _ => "unavailable",
            }
        ),
    }
    let next = reserve_through(&handle, "anthropic", cap).await;

    // Assert: the abandoned unit was kept, so the next caller is the SECOND
    // unit of the day -- not the first.
    assert_eq!(next, PaidProbeCommit::Committed { used: 2, cap });
    assert_eq!(
        handle.counters().write_errors(),
        0,
        "abandoning a wait is not a writer fault",
    );

    stop(handle, writer);
}

/// A writer that could not open its database cannot establish any accounting
/// state, so it refuses. The fail-closed direction: no paid call, never a
/// guessed count.
#[tokio::test]
async fn a_writer_with_no_database_reports_a_failed_write() {
    // Arrange: a path whose parent is a file, so the open fails.
    let dir = TempDir::new().expect("tempdir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").expect("write blocker");
    let (handle, writer) = UsageWriter::start(blocker.join("usage.db"), CHANNEL_CAPACITY, 0, true);

    // Act
    let outcome = reserve_through(&handle, "anthropic", 3).await;

    // Assert
    assert_eq!(outcome, PaidProbeCommit::WriteFailed);
    assert!(
        handle.counters().write_errors() >= 1,
        "a refused reservation on a broken database is counted as a write error",
    );

    stop(handle, writer);
}

/// A writer whose ack sender is dropped without answering resolves as
/// unavailable rather than hanging or being read as permission.
///
/// `Unavailable` rather than `WriteFailed` because a vanished writer is a
/// lifecycle fact, not a storage fault, and the caller's required action is the
/// same either way: no paid call. Both fail closed; this one names the cause
/// correctly.
#[tokio::test]
async fn an_unanswered_reservation_resolves_as_unavailable() {
    // Arrange: a channel whose consumer takes the message and drops it,
    // which is what a writer dying mid-message looks like to the caller.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WriterMessage>(2);
    let handle = handle_over(tx);
    let admission = handle.admit_paid_probe_reservation("anthropic", 3);

    // Act
    let taken = rx.recv().await.expect("the reservation was queued");
    drop(taken);
    let outcome = match admission {
        PaidProbeAdmission::Admitted(receipt) => receipt.await_outcome().await,
        _ => panic!("an open channel must admit"),
    };

    // Assert
    assert_eq!(outcome, PaidProbeCommit::Unavailable);
}

include!("paid_probe_command_storage_tests.rs");
include!("paid_probe_command_surface_tests.rs");
include!("paid_probe_command_lifecycle_tests.rs");
include!("paid_probe_command_refund_guard_tests.rs");
include!("paid_probe_command_gate_tests.rs");
