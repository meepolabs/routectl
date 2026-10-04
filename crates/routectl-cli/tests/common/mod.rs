//! Thin re-export of the shared contract-test fixture builders and the
//! testkit secret-ref helper, plus the cli-only `replay` harness and
//! `readiness` polling submodules.
//!
//! The single source of truth for the canonical-request /
//! canonical-response builders lives in `routectl_core::test_utils`
//! (gated behind the `test-utils` feature, enabled here as a
//! dev-dependency). This shim exists so the ingress contract tests can
//! keep referring to `common::scenarios::*` and the bare helpers
//! (`common::user_msg`, etc.) unchanged, while `replay` stays local to
//! this crate (it depends on cli-side fixtures and harness code).

pub mod readiness;
pub mod replay;

/// A `file://` secret ref that resolves to `value`; see
/// `routectl_testkit::secret_file_ref`.
#[allow(unused_imports)]
pub use routectl_testkit::secret_file_ref as file_ref;

// Not every test binary uses the scenario builders (e.g. the replay
// binaries touch only `common::replay`); the glob re-export is dead in
// those compilation units, which is expected for a shared module.
#[allow(unused_imports)]
pub use routectl_core::test_utils::*;

/// Return a copy of `config` whose `usage.db_path` points at a unique,
/// per-process path so a booted server's usage writer NEVER touches the
/// real `~/.config/routectl/usage.db` (the `UsageConfig` default).
///
/// The base dir is created once per test process (`OnceLock`) under
/// `$TMPDIR/routectl-usage-test-<pid>-<nonce>`, created exclusively so this
/// process never adopts a path that already exists, and the per-call filename is
/// made unique by an atomic counter. The dir is deliberately persistent and
/// leaked rather than guarded by a `tempfile::TempDir`: each server runs
/// detached for the whole test process and the tests never await its
/// shutdown, so a scoped guard could drop (and delete the path) while the
/// writer still holds the open DB handle. Instead, the first call in each
/// process reaps the dirs of earlier, dead processes
/// (`routectl_testkit::temp_reaper`), which bounds what accumulates.
#[allow(dead_code)]
pub fn isolate_usage_db(
    config: std::sync::Arc<routectl_router::Config>,
) -> std::sync::Arc<routectl_router::Config> {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    static BASE: OnceLock<std::path::PathBuf> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let base = BASE.get_or_init(|| {
        routectl_testkit::temp_reaper::reap_stale_test_dirs();
        routectl_testkit::temp_reaper::create_usage_dir()
    });
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = base.join(format!("usage-{n}.db"));

    let mut cfg = (*config).clone();
    cfg.usage.db_path = path;
    std::sync::Arc::new(cfg)
}
