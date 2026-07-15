//! Thin re-export of the shared contract-test fixture builders, plus
//! the cli-only `replay` harness submodule.
//!
//! The single source of truth for the canonical-request /
//! canonical-response builders lives in `routectl_core::test_utils`
//! (gated behind the `test-utils` feature, enabled here as a
//! dev-dependency). This shim exists so the ingress contract tests can
//! keep referring to `common::scenarios::*` and the bare helpers
//! (`common::user_msg`, etc.) unchanged, while `replay` stays local to
//! this crate (it depends on cli-side fixtures and harness code).

pub mod replay;

/// A `file://` secret ref that resolves to `value`. Drop-in replacement for
/// the former `literal:<value>` test fixture now that `literal:` refs are
/// rejected; the resolved value is preserved exactly. The backing 0600 temp
/// file is leaked for the test process so the path stays valid (race-free,
/// unlike mutating a shared env var).
#[allow(dead_code)]
pub fn file_ref(value: &str) -> String {
    use std::io::Write;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    static KEEP_ALIVE: Mutex<Vec<NamedTempFile>> = Mutex::new(Vec::new());

    let mut f = NamedTempFile::new().expect("create test secret file");
    f.write_all(value.as_bytes()).expect("write test secret");
    f.flush().expect("flush test secret");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
    }
    let uri = format!("file://{}", f.path().display());
    KEEP_ALIVE.lock().expect("KEEP_ALIVE poisoned").push(f);
    uri
}

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
/// `$TMPDIR/routectl-usage-test-<pid>` and the per-call filename is made
/// unique by an atomic counter. The dir is deliberately persistent and
/// leaked rather than guarded by a `tempfile::TempDir`: each server runs
/// detached for the whole test process and the tests never await its
/// shutdown, so a scoped guard could drop (and delete the path) while the
/// writer still holds the open DB handle. A small per-process dir left on
/// disk is the accepted cost of avoiding that race.
#[allow(dead_code)]
pub fn isolate_usage_db(
    config: std::sync::Arc<routectl_router::Config>,
) -> std::sync::Arc<routectl_router::Config> {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    static BASE: OnceLock<std::path::PathBuf> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let base = BASE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("routectl-usage-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create per-process usage test dir");
        dir
    });
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = base.join(format!("usage-{n}.db"));

    let mut cfg = (*config).clone();
    cfg.usage.db_path = path;
    std::sync::Arc::new(cfg)
}
