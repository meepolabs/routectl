//! Non-behavioral import baseline sidecar: `catalog_import_state.json`.
//!
//! Persists the last successful import's row counts (per-source,
//! per-family -- see [`crate::catalog_import::ShrinkCounts`]), the fetch
//! date, and a per-source content hash, so the next import's shrink
//! guard (`crate::catalog_import::shrink_guard`, PURE, takes these
//! counts as plain input) has something to compare against. This module
//! is I/O-only: it never decides whether a shrink is acceptable, it only
//! loads and persists the numbers the pure decision function consumes.
//!
//! SAME warn-and-rebuild posture as `crate::catalog_state`'s
//! `catalog_state.json` (NEVER `crate::catalog_overlay`'s fail-closed
//! posture -- this file carries no behavior): a missing, corrupt, or
//! too-new-to-understand file falls back to the caller-supplied
//! baked-table baseline rather than blocking the import. Losing this
//! file costs exactly one degraded shrink-guard comparison on the next
//! import -- there is nothing here worth failing an import over.
//!
//! Writer discipline mirrors `crate::catalog_state`'s exactly: temp file
//! in the same directory, `0o600` set before the write, `fsync` the temp
//! file, atomic `rename`, `0o600` re-set after, then `fsync` the PARENT
//! DIRECTORY so the rename itself survives a crash. No revision check --
//! this is rebuildable observability data, so a last-write-wins race
//! between two imports is harmless.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use routectl_auth::atomic_write::{FsyncPolicy, write_0600_atomic_with_policy};
use serde::{Deserialize, Serialize};

use crate::catalog_import::ShrinkCounts;

/// Schema version this build understands for `catalog_import_state.json`.
/// A file whose `schema_version` exceeds this is treated exactly like a
/// corrupt file by [`load_baseline`]: warn once, fall back to the
/// caller's baked-table baseline.
pub const CATALOG_IMPORT_STATE_SCHEMA_VERSION: u32 = 1;

/// On-disk import baseline: the last successful import's counts, fetch
/// date, and per-source content hashes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogImportState {
    /// Schema version of the persisted state file.
    pub schema_version: u32,
    /// Fetch date (`YYYY-MM-DD`) of the last successful import.
    pub last_import_date: String,
    /// Row count per source (provider kind) at the last import.
    pub per_source_counts: BTreeMap<String, usize>,
    /// Row count per family (vendor grouping) at the last import.
    pub per_family_counts: BTreeMap<String, usize>,
    /// Content hash per source at the last import.
    pub source_hashes: BTreeMap<String, String>,
}

/// Errors from loading `catalog_import_state.json`. Every variant is
/// folded into the SAME "warn once, fall back to the baked baseline"
/// posture by [`load_baseline`] -- see the module doc.
#[derive(Debug, thiserror::Error)]
pub enum CatalogImportStateError {
    /// The file exists but is corrupt or does not parse.
    #[error("catalog import state {path}: corrupt or invalid: {reason}")]
    Corrupt {
        /// Path to the state file.
        path: String,
        /// What made the file unreadable.
        reason: String,
    },

    /// The file's schema version is newer than this build supports.
    #[error(
        "catalog import state {path}: schema_version {found} is newer than the {current} this build supports"
    )]
    VersionTooNew {
        /// Path to the state file.
        path: String,
        /// Schema version found in the file.
        found: u32,
        /// Schema version this build supports.
        current: u32,
    },

    /// The file could not be read from disk.
    #[error("catalog import state {path}: {reason}")]
    Io {
        /// Path to the state file.
        path: String,
        /// Underlying I/O error message.
        reason: String,
    },
}

/// Resolve the state file's default path: `catalog_import_state.json`
/// inside `routectl_config_dir()`, sibling to `catalog_overlay.json` and
/// `catalog_state.json`.
#[must_use]
pub fn default_path() -> PathBuf {
    crate::config::routectl_config_dir().join("catalog_import_state.json")
}

/// Load `catalog_import_state.json` at `path`.
///
/// - missing file -> `Ok(None)` (first run; not an error).
/// - corrupt / invalid JSON -> `Err(CatalogImportStateError::Corrupt)`.
/// - `schema_version` newer than this build understands ->
///   `Err(CatalogImportStateError::VersionTooNew)`.
/// - any other I/O failure -> `Err(CatalogImportStateError::Io)`.
fn load(path: &Path) -> Result<Option<CatalogImportState>, CatalogImportStateError> {
    let display = path.display().to_string();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CatalogImportStateError::Io {
                path: display,
                reason: e.to_string(),
            });
        }
    };

    let state: CatalogImportState =
        serde_json::from_slice(&bytes).map_err(|e| CatalogImportStateError::Corrupt {
            path: display.clone(),
            reason: e.to_string(),
        })?;

    if state.schema_version > CATALOG_IMPORT_STATE_SCHEMA_VERSION {
        return Err(CatalogImportStateError::VersionTooNew {
            path: display,
            found: state.schema_version,
            current: CATALOG_IMPORT_STATE_SCHEMA_VERSION,
        });
    }

    Ok(Some(state))
}

/// Persist `state` atomically as an owner-only (`0o600`) file via the
/// shared secret-file writer. Parent-directory fsync stays BEST-EFFORT:
/// this state file is a rebuildable cache, so a parent-fsync error must
/// not fail the save. No revision check -- see the module doc.
fn save(path: &Path, state: &CatalogImportState) -> Result<(), CatalogImportStateError> {
    let display = path.display().to_string();
    let json = serde_json::to_vec_pretty(state).map_err(|e| CatalogImportStateError::Io {
        path: display.clone(),
        reason: format!("serialize: {e}"),
    })?;
    write_0600_atomic_with_policy(path, &json, FsyncPolicy::BestEffort).map_err(|reason| {
        CatalogImportStateError::Io {
            path: display,
            reason,
        }
    })
}

/// Load the baseline the shrink guard compares a fresh candidate
/// against: the persisted state's counts, or -- on a missing file
/// (first run) or ANY load failure (corrupt JSON, a too-new
/// `schema_version`) -- the caller-supplied baked-table fallback
/// (`crate::catalog_import::baked_shrink_counts`). NEVER fails: a load
/// failure is warned once and folded into the fallback, same posture as
/// `crate::catalog_state::check_drift_and_persist_state`. A missing
/// file is the ordinary first-run case and is NOT warned.
#[must_use]
pub fn load_baseline(path: &Path, baked_fallback: ShrinkCounts) -> ShrinkCounts {
    match load(path) {
        Ok(None) => baked_fallback,
        Ok(Some(state)) => ShrinkCounts {
            per_source: state.per_source_counts,
            per_family: state.per_family_counts,
        },
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                reason = %e,
                "catalog_import_state.json is corrupt, unreadable, or from a newer routectl \
                 build; falling back to the baked-table baseline for this import's shrink guard",
            );
            baked_fallback
        }
    }
}

/// Persist the just-completed import's counts, fetch date, and
/// per-source content hashes as the NEW baseline for the next import's
/// shrink guard. Warns (never propagates) on write failure -- losing
/// this write only degrades the next import's shrink-guard comparison.
pub fn persist_baseline(
    path: &Path,
    last_import_date: &str,
    counts: &ShrinkCounts,
    source_hashes: BTreeMap<String, String>,
) {
    let state = CatalogImportState {
        schema_version: CATALOG_IMPORT_STATE_SCHEMA_VERSION,
        last_import_date: last_import_date.to_string(),
        per_source_counts: counts.per_source.clone(),
        per_family_counts: counts.per_family.clone(),
        source_hashes,
    };
    if let Err(e) = save(path, &state) {
        tracing::warn!(
            path = %path.display(),
            reason = %e,
            "failed to persist catalog_import_state.json; the next import's shrink-guard \
             baseline is degraded, but this import is unaffected",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_import::baked_shrink_counts;

    fn sample_counts() -> ShrinkCounts {
        let mut per_source = BTreeMap::new();
        per_source.insert("anthropic-api".to_string(), 7);
        let mut per_family = BTreeMap::new();
        per_family.insert("anthropic-api".to_string(), 7);
        ShrinkCounts {
            per_source,
            per_family,
        }
    }

    // -----------------------------------------------------------------------
    // Round-trip + atomic write shape: 0600, no leftover tempfiles.
    // -----------------------------------------------------------------------

    #[test]
    fn default_path_uses_catalog_import_state_json_basename() {
        let path = default_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("catalog_import_state.json")
        );
    }

    #[test]
    fn persist_and_load_baseline_round_trips_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_import_state.json");
        let counts = sample_counts();
        let mut source_hashes = BTreeMap::new();
        source_hashes.insert("litellm".to_string(), "deadbeef".to_string());

        persist_baseline(&path, "2026-07-11", &counts, source_hashes.clone());

        let loaded = load(&path)
            .unwrap()
            .expect("state must exist after persist");
        assert_eq!(loaded.schema_version, CATALOG_IMPORT_STATE_SCHEMA_VERSION);
        assert_eq!(loaded.last_import_date, "2026-07-11");
        assert_eq!(loaded.per_source_counts, counts.per_source);
        assert_eq!(loaded.per_family_counts, counts.per_family);
        assert_eq!(loaded.source_hashes, source_hashes);

        let baseline = load_baseline(&path, ShrinkCounts::default());
        assert_eq!(baseline, counts);
    }

    #[cfg(unix)]
    #[test]
    fn persisted_state_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_import_state.json");
        persist_baseline(&path, "2026-07-11", &sample_counts(), BTreeMap::new());

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "catalog_import_state.json must be 0600"
        );
    }

    #[test]
    fn persist_baseline_leaves_no_leftover_tempfiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_import_state.json");
        persist_baseline(&path, "2026-07-11", &sample_counts(), BTreeMap::new());

        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "left tempfiles: {leftover:?}");
    }

    // -----------------------------------------------------------------------
    // Fail-open first run: missing file -> the baked-table fallback,
    // quietly (no warning -- this is the ordinary first-import case).
    // -----------------------------------------------------------------------

    #[test]
    fn load_baseline_first_run_uses_the_baked_fallback_without_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_import_state.json");
        let fallback = baked_shrink_counts();

        let events = routectl_testkit::capture_events(|| {
            let baseline = load_baseline(&path, fallback.clone());
            assert_eq!(baseline, fallback);
        });

        assert!(
            events.is_empty(),
            "a missing file on first run must not warn: {events:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Warn-and-rebuild: corrupt file / too-new schema fall back to the
    // baked baseline, warning exactly once, never blocking the import.
    // -----------------------------------------------------------------------

    #[test]
    fn load_baseline_corrupt_file_warns_once_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_import_state.json");
        std::fs::write(&path, b"not json {{{").unwrap();
        let fallback = sample_counts();

        let events = routectl_testkit::capture_events(|| {
            let baseline = load_baseline(&path, fallback.clone());
            assert_eq!(baseline, fallback);
        });

        assert_eq!(
            events.len(),
            1,
            "corrupt file must warn exactly once: {events:?}"
        );
        assert!(events[0].message.contains("corrupt"));
    }

    #[test]
    fn load_baseline_newer_schema_version_warns_once_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_import_state.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{},"last_import_date":"2026-01-01","per_source_counts":{{}},"per_family_counts":{{}},"source_hashes":{{}}}}"#,
                CATALOG_IMPORT_STATE_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();
        let fallback = sample_counts();

        let events = routectl_testkit::capture_events(|| {
            let baseline = load_baseline(&path, fallback.clone());
            assert_eq!(baseline, fallback);
        });

        assert_eq!(
            events.len(),
            1,
            "too-new schema must warn exactly once: {events:?}"
        );
    }

    #[test]
    fn load_missing_file_returns_ok_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert!(load(&path).unwrap().is_none());
    }
}
