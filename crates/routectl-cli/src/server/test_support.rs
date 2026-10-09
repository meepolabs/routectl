//! Shared unit-test helpers used by more than one server sidecar.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use routectl_router::{CatalogOverlay, Config};
use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};

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

/// Shut `writer` down and wait for its drain to finish, so the tempdir guard
/// returned by [`isolate_usage_db`] can be dropped without racing the writer
/// thread's database open or its last flush.
///
/// `UsageWriter::shutdown` blocks, so it runs on the blocking pool rather than
/// a runtime worker. Every `UsageHandle` clone must be dropped first: a live
/// handle keeps the channel open and the drain waits out its full deadline.
pub(super) async fn drain_usage_writer(writer: UsageWriter) {
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .expect("the usage writer drain must not panic");
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

/// A live writer over a fresh database, plus the tempdir guard and the path a
/// second connection reads through.
///
/// The file is brought to its settled state HERE, by a full start-and-drain
/// cycle of a throwaway writer, before the returned writer starts. The writer
/// performs its migrating open and its one-time open steps on its own thread,
/// which a reader racing it sees as an absent, older-schema, or still-changing
/// file -- so a fixture that skipped this would fail on the read, or count a
/// row the open wrote as one the behavior under test added. After the cycle,
/// the returned writer's open changes nothing a second connection can see.
///
/// Shared by the accounting-adapter sidecars and the composed end-to-end proof:
/// both need the same real writer over a real file, and a second copy of this
/// setup is a second thing that can drift from the schema the writer opens.
pub(super) fn live_usage_writer() -> (tempfile::TempDir, PathBuf, UsageHandle, UsageWriter) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let (settle_handle, settle_writer) =
        UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    drop(settle_handle);
    settle_writer.shutdown();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    (dir, path, handle, writer)
}

/// Every control row the database holds, read through a connection the writer
/// does not own.
///
/// Returns the whole table rather than one key on purpose: the storage key a
/// paid-probe reservation lands under is the usage crate's own encoding and
/// deliberately never crosses the boundary, so callers DERIVE the key from what
/// a committed reservation adds instead of restating it. A hardcoded key here
/// would be a second copy of a private encoding, and would keep passing if the
/// reservation started writing somewhere else entirely.
pub(super) fn usage_control_rows(path: &Path) -> BTreeMap<String, String> {
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
pub(super) fn added_control_rows(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    after
        .iter()
        .filter(|(key, _)| !before.contains_key(*key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The drain returns only once a queued row is durable: a reader opening the
/// database right after it sees the row, with no polling in between.
#[tokio::test]
async fn drain_usage_writer_persists_a_queued_row_before_returning() {
    // Arrange: a writer over a database it has not created yet, one row queued,
    // and the only producer handle released so the channel can close.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("usage.db");
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    handle.try_send(routectl_usage::UsageRecord {
        request_id: "drained-row".to_string(),
        outcome: routectl_usage::Outcome::Ok,
        ..routectl_usage::UsageRecord::default()
    });
    drop(handle);

    // Act
    drain_usage_writer(writer).await;

    // Assert
    let db = routectl_usage::open_readonly(&path).expect("read-only open");
    let ids: Vec<String> = db
        .conn()
        .prepare("SELECT request_id FROM requests")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("request ids");
    assert_eq!(ids, vec!["drained-row".to_string()]);
}
