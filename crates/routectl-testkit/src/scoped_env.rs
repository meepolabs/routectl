//! Restore-on-drop process-environment guard for tests.
//!
//! [`ScopedEnv`] sets or unsets one environment variable and restores its
//! prior value (or absence) when the guard drops, so a panicking assertion
//! cannot leak a mutated variable into a sibling test. std-only: no
//! `serial_test` or `tempfile` dependency.
//!
//! Process-environment mutation is unsynchronized across threads. Every
//! test that constructs a `ScopedEnv` MUST carry `#[serial_test::serial]`
//! so that no sibling test reads or writes the environment concurrently
//! while `set`/`unset`/the `Drop` restore runs. The guard cannot enforce
//! this on its own -- it is the caller's contract.

use std::ffi::{OsStr, OsString};

/// Sets or unsets one environment variable for the guard's lifetime and
/// restores the prior state on drop. See the module docs for the
/// `#[serial_test::serial]` requirement at call sites.
pub struct ScopedEnv {
    key: OsString,
    prev: Option<OsString>,
}

impl ScopedEnv {
    /// Set `key` to `value`, recording the prior value for restoration on
    /// drop.
    pub fn set(key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        let key = key.as_ref().to_os_string();
        let prev = std::env::var_os(&key);
        // SAFETY: the caller runs under `#[serial_test::serial]` (module
        // contract), so no other thread reads or writes the environment
        // while this mutation runs.
        unsafe { std::env::set_var(&key, value) };
        Self { key, prev }
    }

    /// Remove `key` from the environment, recording the prior value for
    /// restoration on drop.
    pub fn unset(key: impl AsRef<OsStr>) -> Self {
        let key = key.as_ref().to_os_string();
        let prev = std::env::var_os(&key);
        // SAFETY: see `set` -- serialized by the caller's #[serial] attr.
        unsafe { std::env::remove_var(&key) };
        Self { key, prev }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        match self.prev.take() {
            // SAFETY: see `ScopedEnv::set` -- restore runs under the same
            // serialized test that created the guard.
            Some(v) => unsafe { std::env::set_var(&self.key, v) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}
