//! The config left behind in the folder routectl used on macOS before every
//! OS shared one config folder.
//!
//! routectl once resolved `$HOME/Library/Application Support/routectl` on
//! macOS. It now uses the one folder `routectl_core::config_dir` resolves on
//! every OS and moves nothing. A config still only in the old folder would
//! otherwise read as "no config" (and `init` would scaffold a fresh one), so
//! the CLI refuses to run until the operator moves the files or names a
//! config explicitly.
//!
//! The old folder is derived from `HOME` on every OS, not only macOS, so the
//! refusal runs (and is tested) the same way everywhere; elsewhere it costs
//! one stat of a path routectl never wrote.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "config.toml";

/// The folder routectl used on macOS before the shared config folder:
/// `$HOME/Library/Application Support/routectl`. `None` when `HOME` is unset
/// or empty.
pub fn legacy_config_dir(home: Option<&OsStr>) -> Option<PathBuf> {
    let home = home.filter(|home| !home.is_empty())?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("routectl"),
    )
}

/// A config file present only in the old folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrandedConfig {
    /// The config file in the old folder.
    pub old: PathBuf,
    /// The config file routectl reads now, which does not exist.
    pub new: PathBuf,
}

impl StrandedConfig {
    /// The refusal for this stranded config.
    pub fn into_error(self) -> StrandedConfigError {
        StrandedConfigError {
            old: self.old,
            new: self.new,
        }
    }
}

/// Decide whether the config is stranded: `new_cfg` does not exist and
/// `legacy_dir` holds a `config.toml`. A path that cannot be inspected is an
/// error, never read as absent.
pub fn stranded_config(new_cfg: &Path, legacy_dir: &Path) -> io::Result<Option<StrandedConfig>> {
    if new_cfg.try_exists()? {
        return Ok(None);
    }
    let old = legacy_dir.join(CONFIG_FILE);
    if !old.try_exists()? {
        return Ok(None);
    }
    Ok(Some(StrandedConfig {
        old,
        new: new_cfg.to_path_buf(),
    }))
}

/// The refusal printed when the config is stranded in the old folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrandedConfigError {
    /// The config file in the old folder.
    pub old: PathBuf,
    /// The config file routectl reads now, which does not exist.
    pub new: PathBuf,
}

impl fmt::Display for StrandedConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let old_dir = self.old.parent().unwrap_or(&self.old);
        let new_dir = self.new.parent().unwrap_or(&self.new);
        write!(
            f,
            "the config is in the old folder: found {old}, but routectl now reads {new}, \
             which does not exist. routectl moved nothing. Move the files in {old_dir} to \
             {new_dir}, or pass --config \"{old}\"",
            old = self.old.display(),
            new = self.new.display(),
            old_dir = old_dir.display(),
            new_dir = new_dir.display(),
        )
    }
}

impl std::error::Error for StrandedConfigError {}

/// The old folder could not be inspected, so routectl cannot tell whether
/// the config was left there.
#[derive(Debug)]
pub struct LegacyInspectError {
    /// The old folder.
    pub legacy_dir: PathBuf,
    /// The config file routectl reads now.
    pub new: PathBuf,
    /// The underlying failure.
    pub source: io::Error,
}

impl fmt::Display for LegacyInspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "could not check {new} or the old config folder {old} for a config: {err}. \
             Fix the permissions, or pass --config",
            new = self.new.display(),
            old = self.legacy_dir.display(),
            err = self.source,
        )
    }
}

impl std::error::Error for LegacyInspectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Refuse to run when the default config is stranded in the old folder.
/// `new_cfg` is the default config path; `home` is `HOME`. A stranded config
/// is a [`StrandedConfigError`]; an uninspectable path is a
/// [`LegacyInspectError`]. With no usable `HOME` there is no old folder and
/// nothing to refuse.
pub fn refuse_stranded_config(
    new_cfg: &Path,
    home: Option<&OsStr>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(legacy_dir) = legacy_config_dir(home) else {
        return Ok(());
    };
    match stranded_config(new_cfg, &legacy_dir) {
        Ok(None) => Ok(()),
        Ok(Some(stranded)) => Err(Box::new(stranded.into_error())),
        Err(source) => Err(Box::new(LegacyInspectError {
            legacy_dir,
            new: new_cfg.to_path_buf(),
            source,
        })),
    }
}

#[cfg(test)]
#[path = "config_folder_tests.rs"]
mod tests;
