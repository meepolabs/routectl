//! Coverage for the writer-health read the status surface derives from the usage
//! writer's shared counters.

use super::*;

use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

/// A writer that has FAILED a write reports degraded.
///
/// Caused by making the writer open a database whose parent path is a regular
/// file -- the mechanism a real persistent failure takes. No forged counter: the
/// writer bumps it itself; the test controls only the filesystem layout.
///
/// Mutation check: return `false` from `writer_degraded` -> red here.
#[test]
fn a_writer_that_failed_a_write_reports_degraded() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("plant the blocker");
    let broken = blocker.join("usage.db");
    let (handle, writer) = UsageWriter::start(broken, CHANNEL_CAPACITY, 0, true);
    // The open runs on the writer's spawned thread; a busy schedule can push the
    // failure past 50ms.
    std::thread::sleep(std::time::Duration::from_millis(200));

    let view = UsageHealthView::new(std::sync::Arc::clone(handle.counters()));

    assert!(
        view.writer_degraded(),
        "a writer that could not open its database is degraded from the start",
    );
    assert!(
        handle.counters().write_errors() > 0,
        "and the write-error counter is nonzero, which is the signal the derivation \
         reads",
    );
    drop(handle);
    writer.shutdown();
}

/// A writer over a healthy database reports not degraded.
///
/// The paired control for the case above: without it, a derivation that always
/// answered degraded would pass.
///
/// Mutation check: return `true` from `writer_degraded` -> red here.
#[test]
fn a_healthy_writer_reports_not_degraded() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    drop(routectl_usage::open(&path).expect("migrating open"));
    let (handle, writer) = UsageWriter::start(path, CHANNEL_CAPACITY, 0, true);

    let view = UsageHealthView::new(std::sync::Arc::clone(handle.counters()));

    assert!(
        !view.writer_degraded(),
        "a writer with no failed write is not degraded",
    );
    drop(handle);
    writer.shutdown();
}
