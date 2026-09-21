//! Tests for the shutdown barrier over the reload-side tasks.
//!
//! The property under test is OWNERSHIP, not timing: when this function
//! returns, no reload-side task may still be holding the daemon's shared state.
//! Ownership is therefore asserted DIRECTLY -- a drop acknowledgement, or an
//! `Arc` count -- rather than inferred from how quickly the writer's drain
//! finishes. The drain appears here only as a separate end-to-end bound.

use super::*;

use std::sync::Arc;
use std::time::Duration;

use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};

use crate::server::test_support::begin_writer_drain;

/// End-to-end ceiling for a drain with nothing left to wait for.
///
/// AT the writer's own documented abandon deadline, deliberately: this is not
/// the ownership evidence (the drop acknowledgements above each use are), so it
/// does not need to discriminate a held handle from a slow scheduler. It exists
/// to catch a drain that never finishes at all, and a generous bound makes it
/// immune to the load the 512-thread runs put on the blocking pool.
const DRAIN_CEILING: Duration = Duration::from_secs(5);

/// A live writer over an isolated ledger.
///
/// The file is brought to the current schema BEFORE the writer starts, matching
/// the sibling adapter fixture: the writer runs its own migrating open on its
/// thread, so anything that reads or opens the same path concurrently would
/// otherwise race that migration and see an absent or older-schema file rather
/// than an empty ledger.
fn live_writer() -> (tempfile::TempDir, UsageHandle, UsageWriter) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("usage.db");
    drop(routectl_usage::open(&path).expect("migrating open"));
    let (handle, writer) = UsageWriter::start(path, CHANNEL_CAPACITY, 0, true);
    (dir, handle, writer)
}

/// Owns a value and SIGNALS when it is dropped.
///
/// The ownership question these tests ask -- "does anything still hold this
/// after the barrier returned?" -- has an exact answer, and this is how it is
/// read: the acknowledgement fires from `Drop`, so requiring it after
/// `await_reload_tasks` returns is a direct observation rather than an
/// inference from how fast some downstream step ran. A timing proxy (drain the
/// writer, see whether it finishes quickly) answers the same question only
/// probabilistically, and under a hostile thread count it answers it wrong.
struct DropSignalling<T> {
    held: Option<T>,
    dropped: Option<tokio::sync::oneshot::Sender<()>>,
}

impl<T> Drop for DropSignalling<T> {
    fn drop(&mut self) {
        // The held value goes FIRST, so the acknowledgement cannot be observed
        // before the thing it reports on is actually released.
        drop(self.held.take());
        if let Some(ack) = self.dropped.take() {
            let _ = ack.send(());
        }
    }
}

impl<T> DropSignalling<T> {
    /// Wrap `held`, returning the wrapper and the receiver its drop signals.
    fn wrap(held: T) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (ack, rx) = tokio::sync::oneshot::channel::<()>();
        (
            Self {
                held: Some(held),
                dropped: Some(ack),
            },
            rx,
        )
    }
}

/// Require that a drop acknowledgement has ALREADY fired.
///
/// `try_recv` rather than an awaited receive with a timeout: the barrier has
/// returned by the time this is called, so the drop either happened before that
/// return or the barrier failed to make it happen. Waiting would turn a
/// definite question into a scheduling one and let a late release pass.
fn assert_dropped(rx: &mut tokio::sync::oneshot::Receiver<()>, what: &str) {
    assert!(
        rx.try_recv().is_ok(),
        "{what} must have been dropped by the time the barrier returned",
    );
}

/// A task that holds `held` and never returns on its own, plus the signal that
/// reports when it is genuinely running.
///
/// The start signal is sent from INSIDE the task: a handle that exists is not a
/// task that has been polled, and a barrier test whose task never ran would
/// pass for the wrong reason.
fn stalled_task_holding<T: Send + 'static>(
    held: T,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _held = held;
        let _ = started_tx.send(());
        // Far past every bound in the shutdown path: only cancellation ends
        // this, which is exactly what the barrier has to perform.
        std::future::pending::<()>().await;
    });
    (handle, started_rx)
}

/// A reload-side task that ignores the shutdown signal must not survive the
/// barrier holding the daemon's accounting handle.
///
/// This is the case the abort exists for. A detached task keeps its
/// `UsageHandle` clone, and the drain that runs next then waits out the
/// writer's abandon deadline on a producer nothing will use again.
///
/// The ownership assertion is the DROP ACKNOWLEDGEMENT, required to have
/// already fired when the barrier returns; the drain that follows is a separate
/// end-to-end bound, not the evidence.
#[tokio::test]
async fn a_stalled_reload_task_is_cancelled_so_it_releases_the_accounting_handle() {
    // Arrange: a task holding the only surviving producer clone, provably
    // running before the barrier is entered.
    let (_dir, handle, writer) = live_writer();
    let (held, mut dropped) = DropSignalling::wrap(handle.clone());
    let (task, started) = stalled_task_holding(held);
    started.await.expect("the stalled task must start");
    drop(handle);
    assert!(
        dropped.try_recv().is_err(),
        "the premise: the task must still hold the handle before the barrier",
    );

    // Act: the barrier, then the drain -- the serve loop's own order.
    let waited_at = std::time::Instant::now();
    await_reload_tasks(vec![task]).await;
    let barrier_wait = waited_at.elapsed();

    // Assert: released by the time the barrier returned, exactly.
    assert_dropped(&mut dropped, "a cancelled task's accounting handle");
    assert!(
        barrier_wait >= RELOAD_TASK_SHUTDOWN_DEADLINE,
        "the premise: the stalled task must have forced the barrier's deadline, \
         got {barrier_wait:?}",
    );

    // And end to end, the drain then finishes rather than hanging.
    let mut drain = begin_writer_drain(writer).await;
    assert!(
        drain.completes_within(DRAIN_CEILING).await,
        "the writer must finish its drain once nothing holds its channel",
    );
}

/// A reload-side task that DOES observe shutdown is awaited normally: no
/// deadline is paid, and its state is released.
///
/// The paired control. Without it, a barrier that aborted every task
/// immediately -- cancelling work that was about to finish cleanly -- would
/// satisfy the case above.
#[tokio::test]
async fn a_cooperative_reload_task_is_awaited_without_paying_the_deadline() {
    // Arrange: a task that returns as soon as it is told to.
    let (_dir, handle, writer) = live_writer();
    let (held, mut dropped) = DropSignalling::wrap(handle);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let (started_tx, started) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _held = held;
        let _ = started_tx.send(());
        let _ = stop_rx.await;
    });
    started.await.expect("the cooperative task must start");
    assert!(
        dropped.try_recv().is_err(),
        "the premise: the task must still hold the handle before the barrier",
    );

    // Act
    let _ = stop_tx.send(());
    let waited_at = std::time::Instant::now();
    await_reload_tasks(vec![task]).await;
    let barrier_wait = waited_at.elapsed();

    // Assert: awaited (no deadline paid) AND its state released.
    assert_dropped(&mut dropped, "an awaited task's accounting handle");
    assert!(
        barrier_wait < RELOAD_TASK_SHUTDOWN_DEADLINE,
        "a cooperative task must be awaited, not made to wait out the deadline: \
         {barrier_wait:?}",
    );

    let mut drain = begin_writer_drain(writer).await;
    assert!(
        drain.completes_within(DRAIN_CEILING).await,
        "the writer must finish its drain once nothing holds its channel",
    );
}

/// The barrier releases a stalled task's ROUTER reference too, so the published
/// Router -- and the paid-probe accounting adapter it carries -- can be dropped
/// by the shutdown sequence that follows.
///
/// The probe driver holds the router `ArcSwap` rather than a usage handle, so
/// the adapter's producer clone reaches the barrier through it. Asserted on the
/// `Arc` count -- a direct ownership reading, like the drop acknowledgements
/// above: after the barrier, the test is the only owner, which is what lets the
/// serve loop's `drop(router_swap)` actually release the adapter.
#[tokio::test]
async fn a_stalled_task_holding_the_router_releases_it_at_the_barrier() {
    // Arrange
    let (_dir, handle, writer) = live_writer();
    let router = crate::server::paid_probe_ledger::install_paid_probe_ledger(
        routectl_router::Router::new(Arc::new(routectl_router::Config::default())),
        &handle,
    );
    let router_swap = Arc::new(arc_swap::ArcSwap::from_pointee(router));
    let (task, started) = stalled_task_holding(Arc::clone(&router_swap));
    started.await.expect("the stalled task must start");
    drop(handle);
    assert_eq!(
        Arc::strong_count(&router_swap),
        2,
        "the premise: the task must hold a second reference",
    );

    // Act
    await_reload_tasks(vec![task]).await;

    // Assert: sole ownership, so releasing it here really releases the adapter.
    assert_eq!(
        Arc::strong_count(&router_swap),
        1,
        "a task surviving the barrier would keep the published Router alive",
    );
    drop(router_swap);
    let mut drain = begin_writer_drain(writer).await;
    assert!(
        drain.completes_within(DRAIN_CEILING).await,
        "with the Router released, the writer must finish its drain",
    );
}
