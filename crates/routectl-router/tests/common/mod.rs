//! Shared fixtures for routectl-router integration tests.

use std::io::Write;
use std::sync::Mutex;

use tempfile::NamedTempFile;

static KEEP_ALIVE: Mutex<Vec<NamedTempFile>> = Mutex::new(Vec::new());

/// A `file://` secret ref that resolves to `value`. Drop-in replacement for
/// the former `literal:<value>` test fixture now that `literal:` refs are
/// rejected; the resolved value is preserved exactly. The backing 0600 temp
/// file is leaked for the test process so the path stays valid (race-free,
/// unlike mutating a shared env var).
#[allow(dead_code)]
pub fn file_ref(value: &str) -> String {
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
