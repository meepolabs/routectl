//! Refusal of a config path whose default lives in the routectl config
//! folder when no folder resolves.
//!
//! `[usage] db_path` and `[mitm] cert_dir` default to a path in the config
//! folder, and to an empty path when neither `XDG_CONFIG_HOME` nor `HOME` is
//! absolute. The commands that open one of them call in here first, so an
//! empty path is refused by key instead of resolving against the working
//! directory. Commands that never open the path (`config check`, `config
//! show`) load such a config without complaint.

use std::fmt;
use std::path::{Path, PathBuf};

use routectl_router::Config;

/// The config key naming the usage database.
pub const USAGE_DB_PATH_KEY: &str = "usage.db_path";
/// The config key naming the MITM certificate directory.
pub const MITM_CERT_DIR_KEY: &str = "mitm.cert_dir";

/// A config path that is empty because no config folder resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoConfigFolder {
    /// The dotted config key whose path is empty.
    pub key: &'static str,
}

impl fmt::Display for NoConfigFolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no config folder for {key}: set HOME, or set {key}",
            key = self.key
        )
    }
}

impl std::error::Error for NoConfigFolder {}

/// `path` when it is non-empty, else [`NoConfigFolder`] naming `key`.
pub fn require_derived_path<'a>(
    path: &'a Path,
    key: &'static str,
) -> Result<&'a Path, NoConfigFolder> {
    if path.as_os_str().is_empty() {
        Err(NoConfigFolder { key })
    } else {
        Ok(path)
    }
}

/// The usage database a command opens: `explicit` (a `--db` flag) when
/// given, else `[usage] db_path`, refused when that is empty.
pub fn usage_db_path(
    explicit: Option<PathBuf>,
    config: &Config,
) -> Result<PathBuf, NoConfigFolder> {
    match explicit {
        Some(path) => Ok(path),
        None => {
            require_derived_path(&config.usage.db_path, USAGE_DB_PATH_KEY).map(Path::to_path_buf)
        }
    }
}

/// Refuse a `[mitm]` block whose `cert_dir` is empty. A config without
/// `[mitm]` passes: nothing loads the CA then.
pub fn require_mitm_cert_dir(config: &Config) -> Result<(), NoConfigFolder> {
    match &config.mitm {
        Some(mitm) => require_derived_path(&mitm.cert_dir, MITM_CERT_DIR_KEY).map(|_| ()),
        None => Ok(()),
    }
}

/// The daemon's start-up check: the usage database (opened at every start,
/// whether or not accounting is enabled) and, with `[mitm]`, the CA folder.
pub fn require_serve_paths(config: &Config) -> Result<(), NoConfigFolder> {
    require_derived_path(&config.usage.db_path, USAGE_DB_PATH_KEY)?;
    require_mitm_cert_dir(config)
}

#[cfg(test)]
#[path = "derived_path_tests.rs"]
mod tests;
