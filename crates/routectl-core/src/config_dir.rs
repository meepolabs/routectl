//! The one rule for where routectl keeps its per-user files.
//!
//! The folder is `$XDG_CONFIG_HOME/routectl` when `XDG_CONFIG_HOME` is set to
//! an absolute path, else `$HOME/.config/routectl` when the home folder is an
//! absolute path, on every OS. An empty or relative variable counts as unset,
//! as the XDG base directory spec requires. On Windows the home folder is
//! `HOME` when that is absolute, else `USERPROFILE`. Each caller picks its own
//! behavior when no folder resolves: the router and the CLI fall back to a
//! relative path through [`config_dir_or_relative`], while the secret and
//! credential stores refuse rather than write under the working directory.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const APP_DIR: &str = "routectl";

fn usable(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|value| Path::new(value).is_absolute())
}

/// Resolve the routectl config folder from the two variable values, with
/// no environment access. `xdg` is `XDG_CONFIG_HOME` and `home` is the home
/// folder; `None`, an empty value and a relative value are treated alike.
/// Returns `None` when neither variable yields a base.
pub fn config_dir_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = match usable(xdg) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(usable(home)?).join(".config"),
    };
    Some(base.join(APP_DIR))
}

/// [`config_dir_from`], or the relative path `.config/routectl` when it
/// returns `None`. The relative result resolves against the working
/// directory.
pub fn config_dir_or_relative_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> PathBuf {
    config_dir_from(xdg, home).unwrap_or_else(|| PathBuf::from(".config").join(APP_DIR))
}

#[cfg(any(windows, test))]
fn windows_home(home: Option<OsString>, userprofile: Option<OsString>) -> Option<OsString> {
    match home {
        Some(home) if usable(Some(&home)).is_some() => Some(home),
        _ => userprofile,
    }
}

#[cfg(windows)]
fn home_from_env() -> Option<OsString> {
    windows_home(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

#[cfg(not(windows))]
fn home_from_env() -> Option<OsString> {
    std::env::var_os("HOME")
}

/// Resolve the routectl config folder from the process environment
/// (`XDG_CONFIG_HOME`, then the home folder); see [`config_dir_from`] for the
/// rule. Returns `None` when neither yields an absolute path.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        home_from_env().as_deref(),
    )
}

/// [`config_dir_or_relative_from`] over the process environment. For callers
/// that must always produce a path; the result is not always absolute.
pub fn config_dir_or_relative() -> PathBuf {
    config_dir_or_relative_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        home_from_env().as_deref(),
    )
}

#[cfg(test)]
#[path = "config_dir_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "config_dir_census_tests.rs"]
mod census_tests;
