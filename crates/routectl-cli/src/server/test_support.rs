//! Shared unit-test helpers used by more than one server sidecar.

use std::sync::Arc;
use std::time::Duration;

use routectl_router::{CatalogOverlay, Config};
use routectl_usage::UsageWriter;

/// Point a config's usage DB at a per-test tempdir so server tests
/// never touch the real `~/.config/routectl/usage.db` (the
/// `UsageConfig` default). Returns the `TempDir` guard the caller
/// MUST keep alive for the test's duration. Isolating the path --
/// rather than disabling usage -- keeps the writer wiring exercised.
pub(super) fn isolate_usage_db(config: &mut Config) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("usage tempdir");
    config.usage.db_path = dir.path().join("usage.db");
    dir
}

/// An otherwise-empty overlay stamped at `revision`, for the reload /
/// capability-boundary tests that turn on the REVISION a Router was built
/// against and not on any cell content.
pub(super) fn overlay_at_revision(revision: u64) -> Arc<CatalogOverlay> {
    Arc::new(CatalogOverlay {
        revision,
        ..CatalogOverlay::default()
    })
}

/// A writer drain that is PROVABLY under way, and can be asked whether it has
/// finished without ever using a sleep as evidence.
///
/// The problem this solves: "the drain is still waiting on a retained producer
/// handle" is only meaningful once the drain has actually STARTED. Sleeping and
/// then reading `JoinHandle::is_finished` cannot distinguish a blocked drain
/// from one the blocking pool has not scheduled yet, so a test written that way
/// reports the same PASS whether or not the thing it names is true -- on a
/// loaded machine it is measuring the scheduler.
///
/// So the start signal is sent from INSIDE the blocking closure, immediately
/// before the drain call, and [`begin_writer_drain`] does not return until it
/// arrives. Everything after that observes a drain that is genuinely in the
/// writer's shutdown path.
pub(super) struct WriterDrain {
    handle: tokio::task::JoinHandle<()>,
}

impl WriterDrain {
    /// Whether the drain finishes within `within`.
    ///
    /// The handle is BORROWED into the timeout rather than moved, so a drain
    /// that has not finished is still owned here and the SAME handle can be
    /// awaited again after the caller releases whatever was holding the
    /// writer's channel open. That is what makes a retained-then-released pair
    /// one continuous observation of one drain instead of two separate ones.
    pub(super) async fn completes_within(&mut self, within: Duration) -> bool {
        match tokio::time::timeout(within, &mut self.handle).await {
            Ok(joined) => {
                joined.expect("the drain task must not panic");
                true
            }
            Err(_) => false,
        }
    }

    /// Await the drain to completion under a generous ceiling, failing loudly
    /// rather than hanging the suite if it never finishes.
    pub(super) async fn finish(self) {
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("the drain must finish once nothing holds the writer's channel")
            .expect("the drain task must not panic");
    }
}

/// Start `writer`'s blocking drain and return once it has genuinely begun.
///
/// Dispatched via `spawn_blocking` exactly as the serve loop dispatches it, so
/// what a test observes is the same shutdown path production runs.
pub(super) async fn begin_writer_drain(writer: UsageWriter) -> WriterDrain {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::task::spawn_blocking(move || {
        // Sent from inside the closure, immediately before the drain: the
        // signal means the blocking pool has picked this task up and the next
        // thing it does is enter the writer's shutdown.
        let _ = started_tx.send(());
        writer.shutdown();
    });
    tokio::time::timeout(Duration::from_secs(10), started_rx)
        .await
        .expect("the blocking pool must start the drain")
        .expect("the drain task must not drop its start signal");
    WriterDrain { handle }
}
