//! Shared unit-test helpers used by more than one server sidecar.

use routectl_router::Config;

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
