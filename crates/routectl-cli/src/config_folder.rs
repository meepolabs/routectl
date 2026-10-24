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
//! The old folder is derived from the variables alone, not from the OS, so
//! the refusal runs (and is tested) the same way everywhere; elsewhere it
//! costs one stat of a path routectl never wrote.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "config.toml";
const APP_DIR: &str = "routectl";

/// How doctor names the old Windows folder without printing a home path.
pub const LEGACY_APPDATA_DISPLAY: &str = "%APPDATA%\\routectl";
/// How doctor names the old macOS folder without printing a home path.
pub const LEGACY_LIBRARY_DISPLAY: &str = "~/Library/Application Support/routectl";

/// The folder routectl used before the shared config folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyConfigDir {
    /// The folder on disk.
    pub path: PathBuf,
    /// A home-free name for the folder, safe to print.
    pub display: &'static str,
}

fn absolute(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|value| Path::new(value).is_absolute())
}

/// The one old-folder candidate for these variable values, with no
/// environment access: `<appdata>/routectl` when `appdata` (`APPDATA`) is an
/// absolute path, else `<home>/Library/Application Support/routectl` when
/// `home` (`HOME`) is, else `None`. An empty or relative value counts as
/// unset.
pub fn legacy_config_dir(home: Option<&OsStr>, appdata: Option<&OsStr>) -> Option<LegacyConfigDir> {
    if let Some(appdata) = absolute(appdata) {
        return Some(LegacyConfigDir {
            path: Path::new(appdata).join(APP_DIR),
            display: LEGACY_APPDATA_DISPLAY,
        });
    }
    let home = absolute(home)?;
    Some(LegacyConfigDir {
        path: Path::new(home)
            .join("Library")
            .join("Application Support")
            .join(APP_DIR),
        display: LEGACY_LIBRARY_DISPLAY,
    })
}

/// [`legacy_config_dir`] over the process's `HOME` and `APPDATA`.
pub fn legacy_config_dir_from_env() -> Option<LegacyConfigDir> {
    legacy_config_dir(
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

/// Refuse when the config is stranded: `new_cfg` does not exist and
/// `legacy_dir` holds a `config.toml`. A path that cannot be inspected is an
/// [`ConfigFolderError::Inspect`] naming it, never read as absent.
pub fn stranded_config(new_cfg: &Path, legacy_dir: &Path) -> Result<(), ConfigFolderError> {
    if exists(new_cfg)? {
        return Ok(());
    }
    let old = legacy_dir.join(CONFIG_FILE);
    if !exists(&old)? {
        return Ok(());
    }
    Err(ConfigFolderError::Stranded {
        old,
        new: new_cfg.to_path_buf(),
    })
}

/// Refuse to run when the default config `new_cfg` is stranded in
/// `legacy_dir`, usually [`legacy_config_dir_from_env`]'s path. With no old
/// folder there is nothing to refuse.
pub fn refuse_stranded_config(
    new_cfg: &Path,
    legacy_dir: Option<&Path>,
) -> Result<(), ConfigFolderError> {
    legacy_dir.map_or(Ok(()), |legacy_dir| stranded_config(new_cfg, legacy_dir))
}

#[cfg(test)]
#[path = "config_folder_tests.rs"]
mod tests;
