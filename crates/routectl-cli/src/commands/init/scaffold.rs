//! The `init --scaffold` fast-path: drop the committed starter `config.toml`
//! onto a fresh machine, fresh-path only.
//!
//! The starter text is the single committed `examples/config.toml` that
//! [`super::super::config::example`] prints -- there is no second template to
//! drift against. It is validated through the shared gate BEFORE any bytes
//! land, and published with a no-clobber atomic rename so scaffold can never
//! overwrite an existing config: if one is already there (or appears in the
//! race window since the caller's existence check), this refuses with the
//! typed [`ScaffoldError::AlreadyExists`] signal the orchestrator matches to
//! route the operator onto the existing-config path.

use std::io::Write as _;
use std::path::Path;

use super::super::edit_pipeline::gate;

/// The committed starter config, single-sourced from the same file
/// `config example` embeds (its shipped-example test pins the file's schema
/// validity). This module is one directory deeper than `commands/config.rs`,
/// hence one extra `../` in the path.
const STARTER_CONFIG: &str = include_str!("../../../../../examples/config.toml");

/// Why a scaffold refused. Each variant is a distinguishable signal the
/// orchestrator matches on -- notably [`AlreadyExists`](ScaffoldError::AlreadyExists),
/// which routes the caller onto the existing-config walk rather than being a
/// terminal error.
#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    /// A config file is already present -- either the caller's earlier
    /// existence check saw it, or it appeared in the window between that
    /// check and this exclusive create. Scaffold never clobbers; the
    /// orchestrator routes this onto the existing-config path.
    #[error("config already exists; re-run `routectl init` to walk it, or edit in place")]
    AlreadyExists,

    /// The starter text failed the shared config-check gate; nothing was
    /// written. The rendered error lines are carried for the caller to
    /// surface. A belt-and-suspenders guard: the shipped example is pinned
    /// valid by its own test, so this is unreachable in practice.
    #[error("starter config failed validation ({} error(s)); nothing written", .0.len())]
    Gate(Vec<String>),

    /// A filesystem failure preparing, writing, or publishing the file.
    #[error("scaffold `{path}`: {reason}")]
    Io { path: String, reason: String },
}

/// Write the committed starter config to `config_path`, fresh-path only.
///
/// Refuses (never overwrites) if a config is already present, aborting with
/// [`ScaffoldError::AlreadyExists`]. The text is gated before any bytes land,
/// and published via a fsynced temp file plus a no-clobber atomic rename, so
/// neither a gate failure nor a mid-write error can leave a partial or invalid
/// file on disk.
pub fn scaffold_fresh(config_path: &Path) -> std::result::Result<(), ScaffoldError> {
    scaffold_from_text(config_path, STARTER_CONFIG)
}

/// Lay down the minimal seed the guided wizard writes on a fresh machine
/// before composing providers: just the schema `version`, everything else
/// serde-defaulted. The wizard needs a config on disk for `provider add` and
/// the final models/aliases write to edit through the one write path; this is
/// that anchor. Written through the SAME gated no-clobber path as the full
/// scaffold, so a file racing in between the caller's check and this create
/// routes to [`ScaffoldError::AlreadyExists`] rather than being clobbered.
pub fn scaffold_seed(config_path: &Path) -> std::result::Result<(), ScaffoldError> {
    let seed = format!("version = {}\n", routectl_router::CURRENT_CONFIG_VERSION);
    scaffold_from_text(config_path, &seed)
}

fn scaffold_from_text(config_path: &Path, text: &str) -> std::result::Result<(), ScaffoldError> {
    gate(text).map_err(ScaffoldError::Gate)?;

    let parent = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| ScaffoldError::Io {
            path: config_path.display().to_string(),
            reason: "path has no parent directory".to_string(),
        })?;
    std::fs::create_dir_all(parent).map_err(|e| ScaffoldError::Io {
        path: parent.display().to_string(),
        reason: format!("create parent directory: {e}"),
    })?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".config.tmp.")
        .suffix(".toml")
        .tempfile_in(parent)
        .map_err(|e| ScaffoldError::Io {
            path: parent.display().to_string(),
            reason: format!("tempfile: {e}"),
        })?;
    tmp.write_all(text.as_bytes())
        .map_err(|e| ScaffoldError::Io {
            path: config_path.display().to_string(),
            reason: format!("write: {e}"),
        })?;
    tmp.as_file().sync_all().map_err(|e| ScaffoldError::Io {
        path: config_path.display().to_string(),
        reason: format!("fsync: {e}"),
    })?;

    tmp.persist_noclobber(config_path).map_err(|e| {
        if e.error.kind() == std::io::ErrorKind::AlreadyExists {
            ScaffoldError::AlreadyExists
        } else {
            ScaffoldError::Io {
                path: config_path.display().to_string(),
                reason: format!("publish: {}", e.error),
            }
        }
    })?;

    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use routectl_router::parse_config;

    fn temp_residue(dir: &Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(".config.tmp."))
            })
            .collect()
    }

    #[test]
    fn scaffold_writes_a_gate_passing_config_with_the_required_sections() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Act
        scaffold_fresh(&path).expect("fresh scaffold writes");

        // Assert: the written file is structurally a valid config with each
        // required top-level section populated -- no golden byte compare.
        let written = std::fs::read_to_string(&path).unwrap();
        gate(&written).expect("scaffolded config passes the shared gate");
        let config = parse_config(&written).expect("scaffolded config parses");
        assert!(
            !config.providers.is_empty(),
            "[providers.*] section present"
        );
        assert!(!config.models.is_empty(), "[models.*] section present");
        assert!(!config.aliases.is_empty(), "[aliases] section present");
        assert!(!config.server.host.is_empty(), "[server] section present");
        assert!(temp_residue(dir.path()).is_empty(), "no temp residue");
    }

    #[test]
    fn scaffold_refuses_an_existing_file_and_leaves_it_byte_identical() {
        // Arrange: a config already sits at the target path.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "version = 3\n[server]\nhost = \"10.0.0.1\"\n";
        std::fs::write(&path, existing).unwrap();

        // Act
        let err = scaffold_fresh(&path).expect_err("existing config must refuse");

        // Assert: the distinguishable existing-config signal, file untouched.
        assert!(matches!(err, ScaffoldError::AlreadyExists), "err: {err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
        assert!(temp_residue(dir.path()).is_empty(), "no temp residue");
    }

    #[test]
    fn a_file_appearing_before_the_create_aborts_to_the_existing_signal() {
        // A file present at publish time drives the no-clobber (O_EXCL) abort
        // to the existing-config signal rather than an overwrite -- the same
        // path a race between the caller's existence check and this create
        // takes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::File::create(&path).unwrap();

        let err = scaffold_fresh(&path).expect_err("O_EXCL create must abort");

        assert!(
            matches!(err, ScaffoldError::AlreadyExists),
            "abort must route to the existing-config signal, not a generic IO error: {err}"
        );
    }

    #[test]
    fn a_gate_failure_writes_no_file() {
        // A candidate the gate rejects (a model wired to an unknown provider)
        // must leave nothing on disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let invalid = "version = 3\n[models.x]\nprovider = \"missing\"\nupstream = \"m\"\n";

        let err = scaffold_from_text(&path, invalid).expect_err("gate must reject");

        assert!(matches!(err, ScaffoldError::Gate(_)), "err: {err}");
        assert!(!path.exists(), "no file written on a gate failure");
        assert!(temp_residue(dir.path()).is_empty(), "no temp residue");
    }
}
