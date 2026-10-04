//! Test-only usage-ledger dirs that outlive the writer thread.
//!
//! A `UsageWriter` opens its DB on its own consumer thread, and the open
//! creates the parent dir when it is missing. A `tempfile::TempDir` guard
//! dropped before that thread runs is therefore recreated (holding a
//! `usage.db`) and never removed. Every dir handed out here lives under one
//! per-process dir created by `routectl_testkit::temp_reaper`, which a later
//! test process reaps once this one has exited.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use routectl_testkit::temp_reaper;

/// A fresh, empty dir for one test's usage ledger.
pub fn usage_dir() -> PathBuf {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let base = BASE.get_or_init(|| {
        temp_reaper::reap_stale_test_dirs();
        temp_reaper::create_usage_dir()
    });
    let dir = base.join(format!(
        "ledger-{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)
        .unwrap_or_else(|err| panic!("create usage test dir {}: {err}", dir.display()));
    dir
}
