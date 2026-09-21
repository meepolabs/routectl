//! Tests for the CLI-side paid-probe accounting adapter.
//!
//! Every case that asserts an accounting OUTCOME drives a REAL usage writer
//! over a REAL file-backed database and reads committed state back through a
//! connection the writer does not own. The two admission refusals a live writer
//! cannot be made to produce on demand (a full channel, a closed one) are
//! driven through the mapping itself, which is why that mapping is a function
//! rather than an inline match.

use super::*;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};
use tempfile::TempDir;

use crate::server::test_support::begin_writer_drain;

/// A live writer over a fresh database, plus the tempdir guard and the path a
/// second connection reads through.
///
/// The file is brought to the current schema HERE, before the writer starts, so
/// a second connection can read it from the first instant. The writer performs
/// its own migrating open on its thread, which a reader racing it sees as an
/// absent or older-schema file rather than as an empty one -- so a fixture that
/// skipped this would fail on the read rather than on the behavior.
fn live_writer() -> (TempDir, PathBuf, UsageHandle, UsageWriter) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    drop(routectl_usage::open(&path).expect("migrating open"));
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    (dir, path, handle, writer)
}

/// One reservation through the adapter, as the Router would ask for it.
async fn reserve(
    ledger: &Arc<UsagePaidProbeLedger>,
    provider: &str,
    daily_cap: u32,
) -> PaidProbeReservation {
    ledger.reserve_paid_probe_unit(provider, daily_cap).await
}

/// Every control row the database holds, read through a connection the writer
/// does not own.
///
/// Returns the whole table rather than one key on purpose: the storage key the
/// reservation lands under is the usage crate's own encoding and deliberately
/// never crosses the boundary, so these tests DERIVE the key from what a
/// committed reservation adds instead of restating it. A hardcoded key here
/// would be a second copy of a private encoding, and would keep passing if the
/// reservation started writing somewhere else entirely.
fn control_rows(path: &Path) -> BTreeMap<String, String> {
    let db = routectl_usage::open_readonly(path).expect("read-only open");
    let mut stmt = db
        .conn()
        .prepare("SELECT key, value FROM meta")
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .expect("query")
    .collect::<Result<BTreeMap<_, _>, _>>()
    .expect("control rows")
}

/// The rows present in `after` and absent from `before`.
fn added_rows(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    after
        .iter()
        .filter(|(key, _)| !before.contains_key(*key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Release the producer side, then stop the writer.
///
/// The handle (and every adapter holding a clone of it) must go FIRST:
/// `UsageWriter::shutdown` waits for the consumer loop to end, which happens
/// only once every sender is gone, so a live clone makes it pay the full drain
/// deadline and detach. That ordering is the whole point of
/// `a_retained_adapter_holds_the_writer_channel_open_until_it_is_dropped`
/// below, and the reason the serve loop releases the router before draining.
fn stop(ledger: Arc<UsagePaidProbeLedger>, handle: UsageHandle, writer: UsageWriter) {
    drop(ledger);
    drop(handle);
    writer.shutdown();
}

/// The ordering the whole feature rests on, at the CLI boundary: when the
/// Router is told a unit committed, that unit is ALREADY readable by another
/// connection.
#[tokio::test]
async fn a_committed_unit_is_readable_from_a_second_connection() {
    // Arrange
    let (_dir, path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    let before = control_rows(&path);

    // Act
    let outcome = reserve(&ledger, "anthropic", 3).await;

    // Assert
    assert_eq!(outcome, PaidProbeReservation::Committed { used: 1, cap: 3 });
    let added = added_rows(&before, &control_rows(&path));
    assert_eq!(
        added.values().collect::<Vec<_>>(),
        vec!["1"],
        "exactly one control row, holding this day's single unit: {added:?}",
    );

    stop(ledger, handle, writer);
}

/// One reservation asks the accounting actor EXACTLY once.
///
/// Observable without counting calls: a second ask inside one reservation would
/// spend a second unit, so the first answer under a cap of one could not be the
/// day's first unit. Both halves are asserted -- the first answer, and that the
/// day is then spent -- so neither an extra ask nor a missing one passes.
#[tokio::test]
async fn one_reservation_spends_exactly_one_unit() {
    // Arrange
    let (_dir, _path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);

    // Act
    let first = reserve(&ledger, "anthropic", 1).await;
    let second = reserve(&ledger, "anthropic", 1).await;

    // Assert
    assert_eq!(
        first,
        PaidProbeReservation::Committed { used: 1, cap: 1 },
        "a single reservation must spend the day's FIRST unit, not two",
    );
    assert_eq!(second, PaidProbeReservation::CapExhausted);

    stop(ledger, handle, writer);
}

/// A cap of two commits exactly twice and then reports the day spent.
#[tokio::test]
async fn a_cap_of_two_commits_twice_then_reports_the_day_spent() {
    // Arrange
    let (_dir, _path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);

    // Act
    let outcomes = [
        reserve(&ledger, "anthropic", 2).await,
        reserve(&ledger, "anthropic", 2).await,
        reserve(&ledger, "anthropic", 2).await,
    ];

    // Assert
    assert_eq!(
        outcomes,
        [
            PaidProbeReservation::Committed { used: 1, cap: 2 },
            PaidProbeReservation::Committed { used: 2, cap: 2 },
            PaidProbeReservation::CapExhausted,
        ],
    );

    stop(ledger, handle, writer);
}

/// The provider the Router named is the provider the unit is spent against, so
/// each configured provider draws on its OWN day budget.
///
/// The sentinel is a cap of one per provider, which makes a substituted or
/// constant provider name impossible to miss: provider A spends its single
/// unit, provider B must still get its own (a shared key would report A's day
/// already spent), and A must then be exhausted (a per-call-unique key would
/// hand A a second unit). Neither half passes on its own, and the stored rows
/// are counted alongside -- two providers, two buckets -- without this test
/// restating the key encoding, which is the usage crate's private business.
#[tokio::test]
async fn each_provider_spends_against_its_own_budget() {
    // Arrange
    let (_dir, path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    let before = control_rows(&path);

    // Act
    let first = reserve(&ledger, "anthropic", 1).await;
    let other = reserve(&ledger, "openai", 1).await;
    let first_again = reserve(&ledger, "anthropic", 1).await;

    // Assert
    assert_eq!(
        first,
        PaidProbeReservation::Committed { used: 1, cap: 1 },
        "the first provider spends its own single unit",
    );
    assert_eq!(
        other,
        PaidProbeReservation::Committed { used: 1, cap: 1 },
        "a second provider must get its OWN unit, not read the first's day as spent",
    );
    assert_eq!(
        first_again,
        PaidProbeReservation::CapExhausted,
        "the first provider's day is spent, so its own cap must refuse",
    );
    let added = added_rows(&before, &control_rows(&path));
    assert_eq!(
        added.len(),
        2,
        "two providers must hold two separate accounting rows: {added:?}",
    );
    let mut counts = added.values().cloned().collect::<Vec<_>>();
    counts.sort();
    assert_eq!(
        counts,
        vec!["1".to_string(), "1".to_string()],
        "each provider's bucket holds exactly its own one unit: {added:?}",
    );

    stop(ledger, handle, writer);
}

/// The cap the Router passed in reaches the accounting actor UNCHANGED: no
/// default, no widening, no re-read of configuration, no unit conversion.
///
/// The sentinel is a DIFFERING cap on the same provider-day. Two units are
/// spent under a cap of two -- which exhausts that cap -- and the next
/// reservation arrives with a cap of five. A substituted cap (the earlier one, a
/// default, a re-read value) refuses; only the caller's own number commits, and
/// the answer reports it back.
#[tokio::test]
async fn the_caller_cap_reaches_the_accounting_actor_unchanged() {
    // Arrange: spend a small cap out entirely.
    let (_dir, _path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    assert_eq!(
        reserve(&ledger, "anthropic", 2).await,
        PaidProbeReservation::Committed { used: 1, cap: 2 },
    );
    assert_eq!(
        reserve(&ledger, "anthropic", 2).await,
        PaidProbeReservation::Committed { used: 2, cap: 2 },
    );
    assert_eq!(
        reserve(&ledger, "anthropic", 2).await,
        PaidProbeReservation::CapExhausted,
        "the premise: the smaller cap is spent, so it would refuse if substituted",
    );

    // Act: the same provider-day, a larger caller cap.
    let wider = reserve(&ledger, "anthropic", 5).await;
    let widest = reserve(&ledger, "anthropic", u32::MAX).await;

    // Assert
    assert_eq!(
        wider,
        PaidProbeReservation::Committed { used: 3, cap: 5 },
        "the caller's cap is what the reservation is checked against and reported under",
    );
    assert_eq!(
        widest,
        PaidProbeReservation::Committed {
            used: 4,
            cap: u32::MAX,
        },
        "a cap at the type's ceiling travels unchanged, neither clamped nor converted",
    );

    stop(ledger, handle, writer);
}

/// A cap of zero refuses every reservation, and the SAME adapter commits under
/// a nonzero cap -- so the refusal is the cap rather than a broken fixture.
#[tokio::test]
async fn a_zero_cap_refuses_while_a_nonzero_cap_commits() {
    // Arrange
    let (_dir, path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    let before = control_rows(&path);

    // Act
    let refused = reserve(&ledger, "anthropic", 0).await;
    let rows_after_refusal = control_rows(&path);
    let committed = reserve(&ledger, "anthropic", 1).await;

    // Assert
    assert_eq!(refused, PaidProbeReservation::CapExhausted);
    assert!(
        added_rows(&before, &rows_after_refusal).is_empty(),
        "a refused reservation must leave no accounting row",
    );
    assert_eq!(
        committed,
        PaidProbeReservation::Committed { used: 1, cap: 1 },
        "the control: the same adapter commits under a nonzero cap",
    );

    stop(ledger, handle, writer);
}

/// Accounting state the writer would never have written is refused as
/// malformed, not read as zero.
///
/// The corrupted key is DERIVED from what a real commit added, so the test never
/// restates the private storage encoding -- and the commit that produced it is
/// the paired control proving the fixture can also succeed.
#[tokio::test]
async fn malformed_accounting_state_refuses_as_malformed() {
    // Arrange: one real commit, so the key this day's units live under is known
    // from the database rather than from a hardcoded string.
    let (_dir, path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    let before = control_rows(&path);
    assert_eq!(
        reserve(&ledger, "anthropic", 4).await,
        PaidProbeReservation::Committed { used: 1, cap: 4 },
        "the control: a canonical count commits",
    );
    let added = added_rows(&before, &control_rows(&path));
    let (key, _) = added
        .iter()
        .next()
        .expect("a committed reservation adds exactly one control row");

    // Act: corrupt that row through a second connection, then ask again.
    {
        let conn = rusqlite::Connection::open(&path).expect("writable open");
        conn.execute(
            "UPDATE meta SET value = ?2 WHERE key = ?1",
            rusqlite::params![key, "not-a-count"],
        )
        .expect("corrupt the stored count");
    }
    let outcome = reserve(&ledger, "anthropic", 4).await;

    // Assert
    assert_eq!(outcome, PaidProbeReservation::MalformedState);
    assert!(
        !matches!(outcome, PaidProbeReservation::Committed { .. }),
        "unreadable accounting state must never authorize a paid call",
    );

    stop(ledger, handle, writer);
}

/// A writer that could not open its database establishes no accounting state,
/// so the adapter reports a failed write rather than a guessed count.
#[tokio::test]
async fn a_writer_without_a_database_reports_a_failed_write() {
    // Arrange: a path whose parent is a regular file, so the open fails.
    let dir = TempDir::new().expect("tempdir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").expect("write blocker");
    let (handle, writer) = UsageWriter::start(blocker.join("usage.db"), CHANNEL_CAPACITY, 0, true);
    let ledger = paid_probe_ledger(&handle);

    // Act
    let outcome = reserve(&ledger, "anthropic", 3).await;

    // Assert
    assert_eq!(outcome, PaidProbeReservation::WriteFailed);

    stop(ledger, handle, writer);
}

/// Once the accounting subsystem has begun going away, no reservation is
/// authorized -- and the same adapter committed a unit moments before, so the
/// refusal is the shutdown rather than a fixture that never worked.
#[tokio::test]
async fn a_shutting_down_accounting_subsystem_refuses_as_unavailable() {
    // Arrange
    let (_dir, _path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    assert_eq!(
        reserve(&ledger, "anthropic", 3).await,
        PaidProbeReservation::Committed { used: 1, cap: 3 },
        "the control: this adapter commits while the subsystem is running",
    );

    // Act: the owning writer goes away, which begins shutdown.
    drop(writer);
    let outcome = reserve(&ledger, "anthropic", 3).await;

    // Assert
    assert_eq!(outcome, PaidProbeReservation::Unavailable);
    assert!(
        !matches!(outcome, PaidProbeReservation::Committed { .. }),
        "a reservation must never be authorized after shutdown has begun",
    );
}

/// A saturated accounting channel is LOAD, reported mechanism-neutrally as an
/// overloaded accounting layer -- never as a fault and never as permission.
///
/// Driven through the mapping rather than through a live writer: filling a
/// bounded channel against a draining consumer is a race, and a fixture that
/// only sometimes saturates could not fail reliably.
#[tokio::test]
async fn a_saturated_admission_maps_to_an_overloaded_accounting_layer() {
    // Arrange / Act
    let outcome = reservation_for(PaidProbeAdmission::Saturated).await;

    // Assert
    assert_eq!(outcome, PaidProbeReservation::Overloaded);
}

/// A closed accounting channel is reported as no accounting being reachable.
#[tokio::test]
async fn an_unavailable_admission_maps_to_an_unreachable_accounting_layer() {
    // Arrange / Act
    let outcome = reservation_for(PaidProbeAdmission::Unavailable).await;

    // Assert
    assert_eq!(outcome, PaidProbeReservation::Unavailable);
}

/// Every refusal the accounting actor can report maps onto its own router-side
/// outcome, and NONE of them permits a paid call.
///
/// The commit half is covered by the real-writer cases above, and cannot be
/// covered here: the committing answer is deliberately impossible to construct
/// outside the crate that spends the unit.
#[test]
fn every_reportable_refusal_maps_onto_its_own_outcome_and_permits_nothing() {
    // Arrange: written as pairs so a refusal folded onto a neighbor's outcome
    // fails here rather than surfacing as a lane sitting on the wrong diagnosis.
    let pairs = [
        (
            PaidProbeCommit::CapExhausted,
            PaidProbeReservation::CapExhausted,
        ),
        (
            PaidProbeCommit::MalformedState,
            PaidProbeReservation::MalformedState,
        ),
        (
            PaidProbeCommit::WriteFailed,
            PaidProbeReservation::WriteFailed,
        ),
        (
            PaidProbeCommit::Unavailable,
            PaidProbeReservation::Unavailable,
        ),
    ];

    // Act / Assert
    for (commit, expected) in pairs {
        // Exhaustively matched so a refusal added on the accounting side cannot
        // reach this set unnoticed: it lands here as a non-exhaustive match.
        match commit {
            PaidProbeCommit::CapExhausted
            | PaidProbeCommit::MalformedState
            | PaidProbeCommit::WriteFailed
            | PaidProbeCommit::Unavailable => {}
            PaidProbeCommit::Committed { .. } => {
                panic!("the refusal set must not contain a commit")
            }
        }
        let mapped = reservation_for_commit(commit);
        assert_eq!(mapped, expected, "{commit:?} must map onto {expected:?}");
        assert!(
            !matches!(mapped, PaidProbeReservation::Committed { .. }),
            "{commit:?} must not authorize a paid call",
        );
    }
}

/// Many concurrent reservations resolve while OTHER work on the same
/// multi-threaded runtime keeps making progress: awaiting a receipt parks the
/// task rather than blocking its worker.
///
/// A blocking await would let the reservations starve the runtime, so the
/// concurrent ticker is the assertion that matters; the commit count alongside
/// it proves the reservations really ran rather than being refused early.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_runtime_keeps_running_other_work_while_receipts_are_awaited() {
    // Arrange
    let (_dir, _path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    let cap = 4_u32;
    let callers = 32_usize;
    let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ticking = {
        let ticks = Arc::clone(&ticks);
        tokio::spawn(async move {
            for _ in 0..50 {
                ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };

    // Act
    let mut tasks = Vec::with_capacity(callers);
    for _ in 0..callers {
        let ledger = Arc::clone(&ledger);
        tasks.push(tokio::spawn(async move {
            reserve(&ledger, "anthropic", cap).await
        }));
    }
    let mut outcomes = Vec::with_capacity(callers);
    for task in tasks {
        outcomes.push(task.await.expect("reservation task"));
    }

    // Assert
    tokio::time::timeout(Duration::from_secs(5), ticking)
        .await
        .expect("the runtime must keep polling other tasks while receipts await")
        .expect("ticker task");
    assert!(
        ticks.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the concurrent task must have made progress",
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, PaidProbeReservation::Committed { .. }))
            .count(),
        cap as usize,
        "exactly the cap may commit: {outcomes:?}",
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == PaidProbeReservation::CapExhausted)
            .count(),
        callers - cap as usize,
        "every refusal here must be an exhausted cap: {outcomes:?}",
    );

    stop(ledger, handle, writer);
}

include!("paid_probe_ledger_lifetime_tests.rs");
include!("paid_probe_ledger_wiring_tests.rs");
