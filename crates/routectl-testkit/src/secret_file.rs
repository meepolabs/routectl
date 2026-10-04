//! Resolvable `file://` secret refs for tests.
//!
//! `literal:` refs are rejected at parse and resolve, so a test that needs a
//! resolvable provider key references an owner-only file instead. Every file
//! this process writes lives in one per-process dir created by
//! [`crate::temp_reaper`], so the files stay valid for the whole test process
//! (no handle has to be kept alive) and a later test process reaps them once
//! this one has exited.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::temp_reaper;

/// A `file://` secret ref that resolves to exactly `value`.
pub fn secret_file_ref(value: &str) -> String {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = DIR.get_or_init(|| {
        temp_reaper::reap_stale_test_dirs();
        temp_reaper::create_secret_dir()
    });
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("secret-{n}"));
    write_owner_only(&path, value);
    format!("file://{}", path.display())
}

fn write_owner_only(path: &std::path::Path, value: &str) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .unwrap_or_else(|err| panic!("create test secret file {}: {err}", path.display()));
    file.write_all(value.as_bytes())
        .and_then(|()| file.flush())
        .unwrap_or_else(|err| panic!("write test secret file {}: {err}", path.display()));
}

#[cfg(test)]
#[path = "secret_file_tests.rs"]
mod tests;
