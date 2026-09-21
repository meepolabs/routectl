//! Bounded graceful-shutdown joins for the reload-side tasks.
//!
//! Its own module because the deadline it owns is a SHUTDOWN property, not a
//! reload one: the coordinator decides what to publish, while this decides how
//! long the daemon waits for those tasks to notice they are done.

/// Upper bound on how long graceful shutdown waits for a single
/// reload-side task (file watcher, SIGHUP listener, coordinator) to
/// observe the shutdown signal and return. Each task selects on the
/// shutdown `watch` channel and exits promptly, so this is a safety
/// cap, not the expected wait.
const RELOAD_TASK_SHUTDOWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Await each reload-side task, under a bounded per-task deadline, and
/// GUARANTEE it is finished before returning.
///
/// The guarantee is the point, not the wait. These tasks own clones of the
/// daemon's shared state -- the coordinator carries a `UsageHandle`, the probe
/// driver reads the router `ArcSwap`, and through it the paid-probe accounting
/// adapter's own handle -- so this barrier is what the shutdown sequence relies
/// on when it releases the router and drains the writer next. A task still
/// running past here would still hold those clones, and the drain would wait out
/// its abandon deadline on a producer nothing is going to use again.
///
/// So the deadline ABORTS rather than detaches. The handle is borrowed into the
/// timeout (`&mut`) instead of moved, which keeps ownership here on expiry;
/// `abort` then cancels the task and the second await runs to the cancellation
/// itself. That await is what makes the barrier real: `abort` only REQUESTS
/// cancellation, so returning on the request alone would leave exactly the
/// detached task this function exists to rule out. It cannot hang -- an aborted
/// task resolves as cancelled at its next suspension point, and these tasks are
/// all `select!` loops.
///
/// A `JoinError` is logged and skipped: a panicked task has already released
/// everything it owned, which is all this barrier needs.
pub(super) async fn await_reload_tasks(handles: Vec<tokio::task::JoinHandle<()>>) {
    for mut handle in handles {
        match tokio::time::timeout(RELOAD_TASK_SHUTDOWN_DEADLINE, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "reload task join failed during shutdown");
            }
            Err(_) => {
                tracing::warn!(
                    deadline_secs = RELOAD_TASK_SHUTDOWN_DEADLINE.as_secs(),
                    "reload task did not stop within deadline; cancelling it",
                );
                handle.abort();
                // AWAITED, not merely requested -- see this function's docs.
                let _ = handle.await;
            }
        }
    }
}

#[cfg(test)]
#[path = "reload_shutdown_tests.rs"]
mod reload_shutdown_tests;
