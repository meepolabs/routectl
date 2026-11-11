//! The config left behind in the folder routectl used before every OS shared
//! one config folder.
//!
//! routectl once resolved `%APPDATA%\routectl` on Windows and
//! `$HOME/Library/Application Support/routectl` on macOS. It now uses the one
//! folder `routectl_core::config_dir` resolves on every OS and moves nothing.
//! A config still only in the old folder would otherwise read as "no config"
//! (and `init` would scaffold a fresh one), so the CLI refuses to run until
//! the operator moves the files or names a config explicitly.
//!
//! The old folders are derived from the variables alone, not from the OS, so
//! the refusal runs (and is tested) the same way everywhere; elsewhere it
//! costs a stat of each path routectl never wrote.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "config.toml";
const APP_FOLDER: &str = "routectl";

/// How doctor names the old Windows folder without printing a home path.
pub const LEGACY_APPDATA_DISPLAY: &str = "%APPDATA%\\routectl";
/// How doctor names the old macOS folder without printing a home path.
pub const LEGACY_LIBRARY_DISPLAY: &str = "~/Library/Application Support/routectl";

/// A folder routectl used before the shared config folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyConfigDir {
    /// The folder on disk.
    pub path: PathBuf,
    /// A home-free name for the folder, safe to print.
    pub display: &'static str,
}

/// `<base>/<under..>/routectl` when `base` is an absolute path; an empty or
/// relative base counts as unset.
fn app_folder_under(base: Option<&OsStr>, under: &[&str]) -> Option<PathBuf> {
    let base = Path::new(base?);
    if !base.is_absolute() {
        return None;
    }
    let mut folder = base.to_path_buf();
    folder.extend(under);
    folder.push(APP_FOLDER);
    Some(folder)
}

/// Every old-folder candidate for these variable values, with no environment
/// access, in checking order: `<appdata>/routectl` when `appdata` (`APPDATA`)
/// is an absolute path, then `<home>/Library/Application Support/routectl`
/// when `home` (`HOME`) is. An empty or relative value counts as unset. A
/// folder both variables name is listed once, under the first.
pub fn legacy_config_dirs(appdata: Option<&OsStr>, home: Option<&OsStr>) -> Vec<LegacyConfigDir> {
    let candidates = [
        app_folder_under(appdata, &[]).map(|path| LegacyConfigDir {
            path,
            display: LEGACY_APPDATA_DISPLAY,
        }),
        app_folder_under(home, &["Library", "Application Support"]).map(|path| LegacyConfigDir {
            path,
            display: LEGACY_LIBRARY_DISPLAY,
        }),
    ];
    let mut dirs: Vec<LegacyConfigDir> = Vec::with_capacity(candidates.len());
    for candidate in candidates.into_iter().flatten() {
        if !dirs.iter().any(|dir| dir.path == candidate.path) {
            dirs.push(candidate);
        }
    }
    dirs
}

/// [`legacy_config_dirs`] over the process's `APPDATA` and `HOME`.
pub fn legacy_config_dirs_from_env() -> Vec<LegacyConfigDir> {
    legacy_config_dirs(
        std::env::var_os("APPDATA").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Why the CLI will not run with the default config.
#[derive(Debug)]
pub enum ConfigFolderError {
    /// A config file is present only in the old folder.
    Stranded {
        /// The config file in the old folder.
        old: PathBuf,
        /// The config file routectl reads now, which does not exist.
        new: PathBuf,
    },
    /// `path` could not be inspected, so routectl cannot tell whether the
    /// config was left in the old folder.
    Inspect {
        /// The path whose existence check failed.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// Neither `XDG_CONFIG_HOME` nor the home folder is an absolute path,
    /// so there is no default config to read.
    NoFolder,
}

impl fmt::Display for ConfigFolderError {
    #[expect(
        clippy::unnecessary_debug_formatting,
        reason = "the paths come from environment variables; Debug escapes control \
                  characters and keeps non-ASCII names copyable"
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stranded { old, new } => {
                let old_dir = old.parent().unwrap_or(old);
                let new_dir = new.parent().unwrap_or(new);
                write!(
                    f,
                    "the config is in the old folder: found {old:?}, but routectl now reads \
                     {new:?}, which does not exist. routectl moved nothing. Move the files in \
                     {old_dir:?} to {new_dir:?}, or pass --config {old:?}",
                )
            }
            Self::Inspect { path, source } => write!(
                f,
                "could not check {path:?} while looking for a config left in the old folder: \
                 {source}. routectl moved nothing. Fix the path, or pass --config",
            ),
            Self::NoFolder => f.write_str(
                "could not resolve an absolute config folder; set HOME or pass --config",
            ),
        }
    }
}

impl std::error::Error for ConfigFolderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Stranded { .. } | Self::NoFolder => None,
            Self::Inspect { source, .. } => Some(source),
        }
    }
}

/// Whether anything is at `path`, the path itself rather than what it links
/// to: a link whose target is missing is still something the operator put
/// there, so it is never read as absent.
fn exists(path: &Path) -> Result<bool, ConfigFolderError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ConfigFolderError::Inspect {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Refuse to run when the default config `new_cfg` does not exist and one
/// of `legacy_dirs` (usually [`legacy_config_dirs_from_env`]) holds a
/// `config.toml`. The candidates are checked in order and the first stranded
/// or uninspectable one wins. A path that cannot be inspected is an
/// [`ConfigFolderError::Inspect`] naming it, never read as absent. With no
/// candidate there is nothing to refuse and nothing is checked.
pub fn refuse_stranded_config(
    new_cfg: &Path,
    legacy_dirs: &[LegacyConfigDir],
) -> Result<(), ConfigFolderError> {
    if legacy_dirs.is_empty() || exists(new_cfg)? {
        return Ok(());
    }
    for legacy in legacy_dirs {
        let old = legacy.path.join(CONFIG_FILE);
        if exists(&old)? {
            return Err(ConfigFolderError::Stranded {
                old,
                new: new_cfg.to_path_buf(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "config_folder_tests.rs"]
mod tests;
