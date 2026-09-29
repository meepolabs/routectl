//! Best-effort reaper for the per-process temp dirs the cli integration
//! tests deliberately leave behind (`routectl-usage-test-<pid>-<nonce>` and
//! `routectl-mitm-e2e-<pid>-<nonce>-<tag>-<n>`).
//!
//! Those dirs cannot be scope-guarded (a detached server outlives its test),
//! so the ceiling is enforced at the NEXT test-process start instead: a dir
//! is removed only when its owning pid is no longer live AND the dir is older
//! than [`GRACE`]. Non-unix builds cannot probe liveness and reap nothing.
//!
//! Liveness is checked and the dir removed in separate steps, so the safety
//! argument rests on how producers create dirs, not on the reaper's timing:
//!
//! - Producers create their leaf dir exclusively ([`create_exclusive_dir`]):
//!   creation fails with `AlreadyExists` rather than adopting a path that is
//!   already there, and a fresh nonce is drawn for the retry.
//! - A path the reaper lists therefore either was created by the process
//!   whose pid it names, or is a leftover of an earlier process with the same
//!   pid and nonce. A live process cannot start using that path while it
//!   exists, so a pid reused after the liveness check cannot end up with its
//!   dir deleted.
//! - The path can be recreated only after the reaper (or a concurrent one)
//!   has removed it, and the reaper does not touch a path again after
//!   removing it. The residual case is a second reaper removing the path and
//!   a reused-pid process drawing the identical 64-bit nonce (and tag and
//!   counter) inside the first reaper's check-to-remove window; that is
//!   accepted as negligible, not excluded.
//! - [`GRACE`] additionally keeps any dir younger than itself.
//!
//! Only names in the current nonce format are ever reaped. Nonce-less names
//! (produced by older checkouts, whose producers may still be running under a
//! reused pid) are deliberately left alone.

use std::fs::DirEntry;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime};

const USAGE_PREFIX: &str = "routectl-usage-test-";
const MITM_PREFIX: &str = "routectl-mitm-e2e-";

const NONCE_HEX_LEN: usize = 16;

/// Nonce redraws before [`create_exclusive_dir`] gives up.
const MAX_CREATE_ATTEMPTS: usize = 8;

pub const GRACE: Duration = Duration::from_mins(10);

/// Random 64-bit hex nonce.
#[allow(dead_code)]
pub fn random_nonce() -> String {
    format!(
        "{:0width$x}",
        uuid::Uuid::new_v4().as_u64_pair().0,
        width = NONCE_HEX_LEN
    )
}

/// Name of a usage-db dir owned by this process.
pub fn usage_dir_name(nonce: &str) -> String {
    format!("{USAGE_PREFIX}{}-{nonce}", std::process::id())
}

/// Name of a mitm cert dir owned by this process for scenario `tag`.
pub fn mitm_dir_name(nonce: &str, tag: &str, n: u64) -> String {
    format!("{MITM_PREFIX}{}-{nonce}-{tag}-{n}", std::process::id())
}

/// Create a fresh usage-db dir under the system temp dir.
#[allow(dead_code)]
pub fn create_usage_dir() -> PathBuf {
    create_exclusive_dir(&std::env::temp_dir(), random_nonce, usage_dir_name)
}

/// Create a fresh mitm cert dir under the system temp dir.
#[allow(dead_code)]
pub fn create_mitm_dir(tag: &str, n: u64) -> PathBuf {
    create_exclusive_dir(&std::env::temp_dir(), random_nonce, |nonce| {
        mitm_dir_name(nonce, tag, n)
    })
}

/// Create `root/<name_for(nonce)>` with `create_dir` (which fails on an
/// existing path), drawing a new nonce from `next_nonce` on `AlreadyExists`.
/// `root` must already exist. Panics after [`MAX_CREATE_ATTEMPTS`] collisions
/// or on any other error.
pub fn create_exclusive_dir(
    root: &Path,
    mut next_nonce: impl FnMut() -> String,
    name_for: impl Fn(&str) -> String,
) -> PathBuf {
    for _ in 0..MAX_CREATE_ATTEMPTS {
        let dir = root.join(name_for(&next_nonce()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => panic!("create test temp dir {}: {err}", dir.display()),
        }
    }
    panic!(
        "could not create a unique test temp dir under {} after {MAX_CREATE_ATTEMPTS} attempts",
        root.display()
    );
}

/// Reap stale dirs under the system temp dir, once per test process.
#[allow(dead_code)]
pub fn reap_stale_test_dirs() {
    static REAPED: Once = Once::new();
    REAPED.call_once(|| reap_in(&std::env::temp_dir(), GRACE));
}

/// Remove dead-pid, past-grace test dirs directly under `root`. Never
/// panics; a removal that fails (e.g. another test process reaped it first)
/// is ignored.
pub fn reap_in(root: &Path, grace: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let own_pid = std::process::id();
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(owner_pid) else {
            continue;
        };
        if pid != own_pid && is_dir_older_than(&entry, now, grace) && !pid_is_live(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The pid encoded in a producer's dir name, or `None` for any other name
/// (including nonce-less names from older producers). Usage dirs are
/// `<pid>-<16 hex nonce>`; mitm dirs are `<pid>-<16 hex nonce>-<tag>-<n>`.
fn owner_pid(name: &str) -> Option<u32> {
    let digits = if let Some(tail) = name.strip_prefix(USAGE_PREFIX) {
        let (pid, nonce) = tail.split_once('-')?;
        if !is_nonce(nonce) {
            return None;
        }
        pid
    } else {
        let (pid, rest) = name.strip_prefix(MITM_PREFIX)?.split_once('-')?;
        let (nonce, tag_and_n) = rest.split_once('-')?;
        let (tag, n) = tag_and_n.rsplit_once('-')?;
        if !is_nonce(nonce) || tag.is_empty() || !is_digits(n) {
            return None;
        }
        pid
    };
    if !is_digits(digits) {
        return None;
    }
    digits.parse().ok()
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn is_nonce(text: &str) -> bool {
    text.len() == NONCE_HEX_LEN && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_dir_older_than(entry: &DirEntry, now: SystemTime, grace: Duration) -> bool {
    entry
        .metadata()
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .and_then(|meta| meta.modified().ok())
        .and_then(|mtime| now.duration_since(mtime).ok())
        .is_some_and(|age| age >= grace)
}

/// `kill(pid, 0)` semantics: only ESRCH means dead. Anything unprobeable
/// (EPERM, an out-of-range pid, a non-unix host) counts as live so the dir
/// is kept.
#[cfg(unix)]
fn pid_is_live(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    match i32::try_from(pid) {
        Ok(raw) if raw > 0 => kill(Pid::from_raw(raw), None) != Err(Errno::ESRCH),
        _ => true,
    }
}

#[cfg(not(unix))]
fn pid_is_live(_pid: u32) -> bool {
    true
}
