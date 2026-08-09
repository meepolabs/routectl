//! Shared unit-test helpers used by more than one server sidecar.

use std::sync::Arc;

use routectl_router::{CatalogOverlay, Config};

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
