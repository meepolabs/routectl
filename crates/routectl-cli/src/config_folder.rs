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

use routectl_core::config_dir::config_dir_from;

const CONFIG_FILE: &str = "config.toml";

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

/// `<base>/routectl` when `base` is absolute, by the shared folder rule's own
/// test, so an empty or relative base counts as unset here exactly as it does
/// there.
fn app_folder_under(base: &Path) -> Option<PathBuf> {
    config_dir_from(Some(base.as_os_str()), None)
}

/// Every old-folder candidate for these variable values, with no environment
/// access, in checking order: `<appdata>/routectl` when `appdata` (`APPDATA`)
/// is an absolute path, then `<home>/Library/Application Support/routectl`
/// when `home` (`HOME`) is. An empty or relative value counts as unset.
pub fn legacy_config_dirs(home: Option<&OsStr>, appdata: Option<&OsStr>) -> Vec<LegacyConfigDir> {
    let appdata = appdata
        .and_then(|appdata| app_folder_under(Path::new(appdata)))
        .map(|path| LegacyConfigDir {
            path,
            display: LEGACY_APPDATA_DISPLAY,
        });
    let library = home
        .and_then(|home| {
            app_folder_under(&Path::new(home).join("Library").join("Application Support"))
        })
        .map(|path| LegacyConfigDir {
            path,
            display: LEGACY_LIBRARY_DISPLAY,
        });
    appdata.into_iter().chain(library).collect()
}

/// [`legacy_config_dirs`] over the process's `HOME` and `APPDATA`.
pub fn legacy_config_dirs_from_env() -> Vec<LegacyConfigDir> {
    legacy_config_dirs(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("APPDATA").as_deref(),
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
}

impl fmt::Display for ConfigFolderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stranded { old, new } => {
                let old_dir = old.parent().unwrap_or(old);
                let new_dir = new.parent().unwrap_or(new);
                write!(
                    f,
                    "the config is in the old folder: found {old}, but routectl now reads \
                     {new}, which does not exist. routectl moved nothing. Move the files in \
                     {old_dir} to {new_dir}, or pass --config \"{old}\"",
                    old = old.display(),
                    new = new.display(),
                    old_dir = old_dir.display(),
                    new_dir = new_dir.display(),
                )
            }
            Self::Inspect { path, source } => write!(
                f,
                "could not check {path} while looking for a config left in the old folder: \
                 {source}. routectl moved nothing. Fix the path, or pass --config",
                path = path.display(),
            ),
        }
    }
}

impl std::error::Error for ConfigFolderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Stranded { .. } => None,
            Self::Inspect { source, .. } => Some(source),
        }
    }
}

fn exists(path: &Path) -> Result<bool, ConfigFolderError> {
    path.try_exists()
        .map_err(|source| ConfigFolderError::Inspect {
            path: path.to_path_buf(),
            source,
        })
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
