//! Test-only resolvable secret refs.
//!
//! `literal:` refs are rejected at parse and resolve, so unit tests that need
//! a build-time resolvable provider key reference a 0600 temp file instead.
//! The files are kept alive for the test process so the path stays valid --
//! race-free, unlike mutating a shared process env var.

use std::io::Write;
use std::sync::Mutex;

use tempfile::NamedTempFile;

static KEEP_ALIVE: Mutex<Vec<NamedTempFile>> = Mutex::new(Vec::new());

/// A `file://` secret ref that resolves to `value`. Drop-in replacement for
/// the former `literal:<value>` test fixture; the resolved value is preserved
/// exactly so value assertions still hold.
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
