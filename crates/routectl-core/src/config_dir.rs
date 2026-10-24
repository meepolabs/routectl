//! The one rule for where routectl keeps its per-user files.
//!
//! The folder is `$XDG_CONFIG_HOME/routectl` when `XDG_CONFIG_HOME` is set
//! and non-empty, else `$HOME/.config/routectl` when `HOME` is set and
//! non-empty, on every OS. An empty variable counts as unset. Each caller
//! picks its own behavior when no folder resolves: the router falls back to
//! a relative path through [`config_dir_or_relative`], while the secret and
//! credential stores refuse rather than write under the working directory.

use std::ffi::OsStr;
use std::path::PathBuf;

const APP_DIR: &str = "routectl";

/// Resolve the routectl config folder from the two variable values, with
/// no environment access. `xdg` is `XDG_CONFIG_HOME` and `home` is `HOME`;
/// `None` and an empty value are treated alike. Returns `None` when neither
/// variable yields a base.
pub fn config_dir_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = match xdg.filter(|xdg| !xdg.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home.filter(|home| !home.is_empty())?).join(".config"),
    };
    Some(base.join(APP_DIR))
}

/// Resolve the routectl config folder from the process environment
/// (`XDG_CONFIG_HOME`, then `HOME`); see [`config_dir_from`] for the rule.
/// Returns `None` when neither variable is set non-empty.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// [`config_dir()`], or the relative path `.config/routectl` (resolved
/// against the working directory) when no folder resolves. For callers that
/// must always produce a path; the relative result is not absolute.
pub fn config_dir_or_relative() -> PathBuf {
    config_dir().unwrap_or_else(|| PathBuf::from(".config").join(APP_DIR))
}

#[cfg(test)]
#[path = "config_dir_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "config_dir_census_tests.rs"]
mod census_tests;
