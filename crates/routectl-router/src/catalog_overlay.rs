//! Layer-2 catalog overlay store: null-disable / value overrides on top of
//! the baked catalog table, persisted at `catalog_overlay.json`.
//!
//! EXTRACTION SEAM: this module imports only `std` plus generic infra
//! crates already in the workspace (`serde`, `serde_json`, `thiserror`,
//! `tempfile`) -- zero `routectl_core` types and zero router-specific type
//! imports (no `Config`, no `CatalogRow`, ...), so it can later `mv`
//! to a standalone crate with a Cargo.toml edit and nothing else. The one
//! router-crate touch point is [`default_path`], which calls the sibling
//! `config::routectl_config_dir()` function (a plain `PathBuf` helper, not
//! a type) to resolve the on-disk location; every other function here
//! takes its `path: &Path` as an argument.
//!
//! Semantics of a map value `Option<OverlayCell>` (see [`CatalogOverlay`]):
//! - `Some(Some(cell))` (JSON object) -> overlay value.
//! - `Some(None)` (JSON `null`) -> DISABLED (key present, value null).
//! - absent key -> fall through to the baked row.
//!
//! The overlay is behavioral (null-disable), so a corrupt or
//! too-new-to-understand file is never silently ignored: [`load`] fails
//! closed rather than risk warn-and-ignore silently re-enabling a row an
//! operator explicitly disabled. Only a genuinely missing file (first run)
//! resolves to an empty overlay.
//!
//! Writer discipline extends the OAuth credentials-file standard
//! (`routectl-auth/src/oauth/file_io.rs`) with a post-rename
//! parent-directory `fsync` (not yet backported to routectl-auth): temp
//! file in the same directory, `0o600` set before the write, `fsync`,
//! atomic `rename`, `0o600` re-set after, then `fsync` the PARENT
//! DIRECTORY -- `rename` is atomic for concurrent readers, but is not
//! durable across a crash/power loss until the directory entry pointing
//! at the new inode is flushed too (mirrors
//! `crate::config_migrate::write_config_atomic`). [`save`] additionally
//! compares the on-disk `revision` against an
//! `expected_revision` before writing: a caller working from a stale
//! snapshot gets an explicit conflict instead of a silent lost update (the
//! bug being replaced: `routectl-cli/src/commands/catalog.rs`'s sidecar
//! writer does a bare load-modify-write with no revision check and no
//! fsync/0600). NOTE: `save` on its own still holds no lock, so a caller
//! that bypasses [`with_overlay_write_lock`] and calls `load`/`save`
//! directly can still race the window between load and rename. Every
//! writer that goes through [`with_overlay_write_lock`] is closed to that
//! race: the advisory lock is held across the whole load -> mutate -> save
//! sequence, so two truly concurrent callers serialize instead of racing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use routectl_auth::atomic_write::{FsyncPolicy, ensure_dir_0700, write_0600_atomic_with_policy};
use serde::{Deserialize, Serialize};

/// Overlay schema version this build understands. [`load`] fails closed
/// (an explicit [`OverlayError::VersionTooNew`]) on a file whose
/// `schema_version` exceeds this -- a newer routectl wrote fields/semantics
/// this build cannot safely interpret.
pub const CATALOG_OVERLAY_SCHEMA_VERSION: u32 = 1;

/// On-disk overlay: a revision-checked map of per-selector overrides
/// layered on top of the baked catalog table at merge time.
///
/// NOT `deny_unknown_fields`, and NOT `#[serde(default)]` on the
/// container: a field this build doesn't know about (written by a newer
/// routectl) is ignored rather than rejected (forward compat), but every
/// field THIS struct declares must be present -- a truncated or
/// hand-edited file missing `schema_version` / `revision` / `cells`
/// (including the degenerate `{}`) surfaces as
/// [`OverlayError::Corrupt`] naming the missing field, rather than
/// silently resolving to an empty overlay that would discard every
/// disable an operator wrote. Every writer in this module always
/// serializes all three fields, so a file this module itself wrote
/// always round-trips.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogOverlay {
    pub schema_version: u32,
    pub revision: u64,
    /// Selector (row key) -> overlay value. See the module doc for the
    /// three-state `Option<Option<OverlayCell>>` semantics via
    /// `cells.get(key)`.
    pub cells: BTreeMap<String, Option<OverlayCell>>,
}

impl Default for CatalogOverlay {
    fn default() -> Self {
        Self {
            schema_version: CATALOG_OVERLAY_SCHEMA_VERSION,
            revision: 0,
            cells: BTreeMap::new(),
        }
    }
}

/// Provenance of an overlay cell: an imported legacy stamp/override
/// (migration) versus an operator-authored one (import/user verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlaySource {
    Import,
    User,
}

/// One overlay cell: sparse per-field overrides on top of a baked
/// `CatalogRow`, plus provenance. Every value field is `Option`; an
/// unset field inherits the baked value at merge time.
///
/// `source` and `verified_at` are NOT optional: a real overlay cell always
/// carries both (the writer in this module always sets them), so a JSON
/// object missing either is treated as malformed input -- the container's
/// `#[serde(default)]` does not extend to this struct, deliberately, so a
/// truncated/hand-edited cell surfaces as a load error (fail-closed) rather
/// than silently filling in a fabricated provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayCell {
    pub source: OverlaySource,
    pub verified_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wm: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rm: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_prefix_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<BTreeMap<String, bool>>,
}

/// Errors from loading or saving the overlay file. Every variant carries
/// enough context (path, and either a parse reason or the conflicting
/// revisions) for an operator to act without re-deriving it from a bare
/// `std::io::Error` or `serde_json::Error`.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    /// The file exists but is not valid JSON, or does not match
    /// [`CatalogOverlay`]'s shape.
    #[error("catalog overlay {path}: corrupt or invalid: {reason}")]
    Corrupt { path: String, reason: String },

    /// `schema_version` in the file is greater than
    /// [`CATALOG_OVERLAY_SCHEMA_VERSION`]: a newer routectl wrote this
    /// file and this build cannot safely interpret it.
    #[error(
        "catalog overlay {path}: schema_version {found} is newer than the {current} this build supports"
    )]
    VersionTooNew {
        path: String,
        found: u32,
        current: u32,
    },

    /// A filesystem-level failure (permission denied, disk full, a
    /// missing parent that could not be created, ...) distinct from a
    /// content problem.
    #[error("catalog overlay {path}: {reason}")]
    Io { path: String, reason: String },

    /// [`save`]'s `expected_revision` did not match the on-disk revision.
    /// No write occurred; the caller re-reads and retries explicitly (no
    /// auto-retry here -- see the module doc).
    #[error("catalog overlay revision conflict: expected {expected}, on-disk revision is {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
}

/// Resolve the overlay file's default path: `catalog_overlay.json` inside
/// `routectl_config_dir()`. The one call in this module that reaches into
/// the rest of the router crate -- see the module doc's extraction-seam
/// note. `pub` (not `pub(crate)`) so the shared config loader (routectl-cli)
/// can resolve the path directly; `catalog::overlay_default_path`'s
/// duplicate re-implementation of this same join is gone -- this is the one
/// path source of truth, re-exported crate-wide as `overlay_default_path`.
pub fn default_path() -> PathBuf {
    crate::config::routectl_config_dir().join("catalog_overlay.json")
}

/// Load the overlay at `path`.
///
/// FAIL-CLOSED LOAD MATRIX:
/// - missing file -> `Ok` with an empty, current-schema overlay (`revision`
///   0). NOT an error -- first run / no overlay yet.
/// - corrupt / invalid JSON -> `Err(OverlayError::Corrupt)`.
/// - `schema_version` greater than [`CATALOG_OVERLAY_SCHEMA_VERSION`] ->
///   `Err(OverlayError::VersionTooNew)`.
/// - any other I/O failure (permission denied, ...) -> `Err(OverlayError::Io)`.
///
/// Callers decide posture on error: cold startup should fail; a hot config
/// reload should reject the reload and keep the prior router live (wired
/// by the shared config loader in routectl-cli).
pub fn load(path: &Path) -> Result<CatalogOverlay, OverlayError> {
    let display = path.display().to_string();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CatalogOverlay::default());
        }
        Err(e) => {
            return Err(OverlayError::Io {
                path: display,
                reason: e.to_string(),
            });
        }
    };

    let overlay: CatalogOverlay =
        serde_json::from_slice(&bytes).map_err(|e| OverlayError::Corrupt {
            path: display.clone(),
            reason: e.to_string(),
        })?;

    if overlay.schema_version > CATALOG_OVERLAY_SCHEMA_VERSION {
        return Err(OverlayError::VersionTooNew {
            path: display,
            found: overlay.schema_version,
            current: CATALOG_OVERLAY_SCHEMA_VERSION,
        });
    }

    Ok(overlay)
}

/// Revision-checked atomic save: read the current on-disk overlay, compare
/// its `revision` against `expected_revision`, and -- only on a match --
/// atomically write `cells` at `revision + 1`.
///
/// A mismatch returns `Err(OverlayError::RevisionConflict)` and leaves the
/// on-disk file byte-unchanged; there is no auto-retry (the caller
/// re-reads and re-decides). A load failure (corrupt file, too-new schema,
/// I/O error) propagates from [`load`] unchanged -- a caller cannot write
/// blind over a file it could not validate.
///
/// Today only the migrator calls this; the import/user verbs share the
/// same function.
pub fn save(
    path: &Path,
    expected_revision: u64,
    cells: BTreeMap<String, Option<OverlayCell>>,
) -> Result<CatalogOverlay, OverlayError> {
    let current = load(path)?;
    if current.revision != expected_revision {
        return Err(OverlayError::RevisionConflict {
            expected: expected_revision,
            actual: current.revision,
        });
    }

    let next = CatalogOverlay {
        schema_version: CATALOG_OVERLAY_SCHEMA_VERSION,
        revision: expected_revision + 1,
        cells,
    };
    write_atomic(path, &next)?;
    Ok(next)
}

/// Read the current revision of an already-loaded overlay. A thin
/// accessor so callers (the invalidation-breadcrumb consumer chief among
/// them) never need to reach into [`CatalogOverlay`]'s field directly.
#[must_use]
pub const fn overlay_revision(overlay: &CatalogOverlay) -> u64 {
    overlay.revision
}

/// The single serialized write entry point for the overlay: acquire a
/// kernel advisory lock on a sibling `.lock` file, load the current
/// overlay under that lock, hand it to `f` for the caller to mutate (or
/// abort by returning `Err`), and -- only on success -- run the
/// revision-checked [`save`] under the SAME held lock before releasing it.
///
/// LOCK SCOPE: the lock covers exactly load -> `f` -> save, nothing more.
/// It must never be held across a network fetch or an interactive confirm
/// prompt -- callers that need either do that work BEFORE calling this
/// function and pass only the already-decided mutation into `f`. Two
/// truly concurrent callers on the same `path` serialize: the second
/// blocks on the lock until the first's save (or abort) releases it, then
/// loads whatever the first one left behind, so no update is lost.
///
/// `f` receives the overlay as loaded under the lock and returns the
/// mutated overlay to persist; `f`'s own `revision`/`schema_version`
/// fields on the returned value are ignored -- only `cells` is used, and
/// [`save`] is called with the revision this function observed at load
/// time, so the last-line-defense revision check in [`save`] can never
/// itself trip inside a single lock hold.
///
/// After a successful save, this function emits ONE `tracing::info!` line
/// carrying the new revision and the selector keys whose cell value
/// changed (added, removed, or edited) between the pre-write and
/// post-write cell maps.
pub fn with_overlay_write_lock<E, F>(path: &Path, f: F) -> Result<CatalogOverlay, E>
where
    E: From<OverlayError>,
    F: FnOnce(CatalogOverlay) -> Result<CatalogOverlay, E>,
{
    ensure_dir(path)?;
    let lock_path = lock_path_for(path);
    let lock_file = open_lock_file(&lock_path)?;
    let mut file_lock = fd_lock::RwLock::new(lock_file);
    let _guard = acquire_write_lock(&lock_path, &mut file_lock)?;

    let loaded = load(path)?;
    let expected_revision = loaded.revision;
    let cells_before = loaded.cells.clone();
    let mutated = f(loaded)?;
    let saved = save(path, expected_revision, mutated.cells)?;

    let changed = changed_selectors(&cells_before, &saved.cells);
    tracing::info!(
        revision = saved.revision,
        changed_selectors = %changed.join(","),
        "catalog overlay write committed",
    );

    Ok(saved)
}

/// Path to the sibling advisory-lock file for `path`, e.g.
/// `catalog_overlay.json` -> `catalog_overlay.json.lock`. The lock file's
/// own contents are never read or written -- it exists purely as a kernel
/// lock handle, so its lifetime and the overlay's are independent (a
/// stale empty lock file left behind by an old build is harmless).
fn lock_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Open (creating if absent) the sibling lock file for
/// [`fd_lock::RwLock`] to hold. Read+write access, never truncated -- the
/// file's contents are unused.
fn open_lock_file(lock_path: &Path) -> Result<std::fs::File, OverlayError> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| OverlayError::Io {
            path: lock_path.display().to_string(),
            reason: format!("open lock file: {e}"),
        })
}

/// Block until the advisory write lock on `lock_file` is held. RAII: the
/// returned guard releases the lock on drop.
fn acquire_write_lock<'a>(
    lock_path: &Path,
    lock_file: &'a mut fd_lock::RwLock<std::fs::File>,
) -> Result<fd_lock::RwLockWriteGuard<'a, std::fs::File>, OverlayError> {
    lock_file.write().map_err(|e| OverlayError::Io {
        path: lock_path.display().to_string(),
        reason: format!("acquire advisory write lock: {e}"),
    })
}

/// The selector keys whose value differs between `before` and `after` --
/// added, removed, or edited (including a flip between `Some(cell)` and
/// `None`/disabled). A cheap `BTreeMap` structural compare; overlays are
/// small (one row per configured selector), so this never needs to be
/// smarter than "diff both maps".
fn changed_selectors(
    before: &BTreeMap<String, Option<OverlayCell>>,
    after: &BTreeMap<String, Option<OverlayCell>>,
) -> Vec<String> {
    let mut changed: BTreeSet<&str> = BTreeSet::new();
    for (key, before_value) in before {
        if after.get(key) != Some(before_value) {
            changed.insert(key);
        }
    }
    for key in after.keys() {
        if !before.contains_key(key) {
            changed.insert(key);
        }
    }
    changed.into_iter().map(str::to_string).collect()
}

/// Ensure the parent directory of `path` exists with `0o700` before the
/// sibling lock file is created there. The atomic write itself re-asserts
/// this, but the lock file is opened before the write runs, so the
/// directory has to exist first. Delegates to the shared secret-dir helper.
fn ensure_dir(path: &Path) -> Result<(), OverlayError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    ensure_dir_0700(parent).map_err(|reason| OverlayError::Io {
        path: parent.display().to_string(),
        reason,
    })
}

/// Serialize `overlay` and persist it atomically as an owner-only
/// (`0o600`) file via the shared secret-file writer. Parent-directory
/// fsync stays BEST-EFFORT here: this overlay is a rebuildable operator
/// cache, not a secret whose loss on crash must fail the save, so a
/// parent-fsync error is swallowed rather than surfaced.
fn write_atomic(path: &Path, overlay: &CatalogOverlay) -> Result<(), OverlayError> {
    let display = path.display().to_string();
    let json = serde_json::to_vec_pretty(overlay).map_err(|e| OverlayError::Io {
        path: display.clone(),
        reason: format!("serialize: {e}"),
    })?;
    write_0600_atomic_with_policy(path, &json, FsyncPolicy::BestEffort).map_err(|reason| {
        OverlayError::Io {
            path: display,
            reason,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import_cell() -> OverlayCell {
        OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2026-01-01".to_string(),
            wm: Some(1.25),
            rm: Some(0.10),
            ttl_seconds: Some(300),
            min_prefix_tokens: None,
            max_context_tokens: None,
            capabilities: None,
        }
    }

    fn user_cell() -> OverlayCell {
        OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-07-01".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: Some(1024),
            max_context_tokens: Some(200_000),
            capabilities: Some(BTreeMap::from([("web_search".to_string(), true)])),
        }
    }

    // -----------------------------------------------------------------------
    // Round-trip: import cell + user cell + disabled cell + absent key.
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_preserves_import_user_and_disabled_cells() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert("openai-compat:grok-*".to_string(), Some(import_cell()));
        cells.insert(
            "anthropic-api:claude-opus-4-8*".to_string(),
            Some(user_cell()),
        );
        cells.insert("openai-compat:disabled-model".to_string(), None);

        // Act
        let saved = save(&path, 0, cells.clone()).expect("save");
        let loaded = load(&path).expect("load");

        // Assert: exact round-trip equality.
        assert_eq!(loaded, saved);
        assert_eq!(loaded.cells, cells);
        assert_eq!(loaded.revision, 1);

        // The disabled cell round-trips to `Some(None)` (present, null);
        // an absent key (never inserted) is simply not in the map.
        assert_eq!(
            loaded.cells.get("openai-compat:disabled-model"),
            Some(&None)
        );
        assert!(!loaded.cells.contains_key("never-inserted"));

        // On-disk JSON literally shows `null` for the disabled cell.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"openai-compat:disabled-model\": null"),
            "disabled cell must serialize as JSON null: {raw}"
        );
    }

    // -----------------------------------------------------------------------
    // Missing file -> empty overlay, not an error.
    // -----------------------------------------------------------------------

    #[test]
    fn load_missing_file_returns_empty_overlay() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");

        // Act
        let overlay = load(&path).expect("missing file must not be an error");

        // Assert
        assert_eq!(overlay.schema_version, CATALOG_OVERLAY_SCHEMA_VERSION);
        assert_eq!(overlay.revision, 0);
        assert!(overlay.cells.is_empty());
    }

    // -----------------------------------------------------------------------
    // Fail-closed: corrupt JSON and schema_version too new.
    // -----------------------------------------------------------------------

    #[test]
    fn load_corrupt_json_returns_err_with_path_and_reason() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        std::fs::write(&path, b"not json {{{").unwrap();

        // Act
        let err = load(&path).expect_err("corrupt JSON must fail closed");

        // Assert
        match err {
            OverlayError::Corrupt { path: p, reason } => {
                assert!(p.contains("catalog_overlay.json"), "path: {p}");
                assert!(!reason.is_empty());
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn load_schema_version_too_new_returns_err() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{},"revision":0,"cells":{{}}}}"#,
                CATALOG_OVERLAY_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();

        // Act
        let err = load(&path).expect_err("newer schema_version must fail closed");

        // Assert
        match err {
            OverlayError::VersionTooNew { found, current, .. } => {
                assert_eq!(found, CATALOG_OVERLAY_SCHEMA_VERSION + 1);
                assert_eq!(current, CATALOG_OVERLAY_SCHEMA_VERSION);
            }
            other => panic!("expected VersionTooNew, got {other:?}"),
        }
    }

    #[test]
    fn load_empty_object_is_rejected_as_corrupt_rather_than_an_empty_overlay() {
        // Arrange: `{}` must never parse as an empty overlay -- that would
        // silently discard every disable an operator wrote.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        std::fs::write(&path, b"{}").unwrap();

        // Act
        let err = load(&path).expect_err("an empty object must fail closed");

        // Assert
        match err {
            OverlayError::Corrupt { path: p, reason } => {
                assert!(p.contains("catalog_overlay.json"), "path: {p}");
                assert!(!reason.is_empty());
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn load_missing_cells_key_is_rejected_as_corrupt_naming_the_field() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        std::fs::write(&path, br#"{"schema_version":1,"revision":0}"#).unwrap();

        // Act
        let err = load(&path).expect_err("a file missing `cells` must fail closed");

        // Assert
        match err {
            OverlayError::Corrupt { reason, .. } => {
                assert!(reason.contains("cells"), "reason: {reason}");
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn load_tolerates_an_unknown_top_level_field_for_forward_compat() {
        // Arrange: every known field present, plus one this build has
        // never heard of -- must load, not reject (NOT deny_unknown_fields).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        std::fs::write(
            &path,
            br#"{"schema_version":1,"revision":0,"cells":{},"future_field":"x"}"#,
        )
        .unwrap();

        // Act
        let overlay = load(&path).expect("an unknown top-level field must not be rejected");

        // Assert
        assert_eq!(overlay.schema_version, 1);
        assert_eq!(overlay.revision, 0);
        assert!(overlay.cells.is_empty());
    }

    // -----------------------------------------------------------------------
    // Atomic save: 0600 mode, no leftover tempfiles, parent dir creation.
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn save_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        // Act
        save(&path, 0, BTreeMap::new()).expect("save");

        // Assert
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "catalog_overlay.json must be 0600, got {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn save_overwrites_atomically_and_leaves_no_tempfiles() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        save(&path, 0, BTreeMap::new()).expect("first save");

        let mut cells = BTreeMap::new();
        cells.insert("a:b".to_string(), Some(import_cell()));

        // Act
        save(&path, 1, cells.clone()).expect("second save");

        // Assert: latest content wins.
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.cells, cells);
        assert_eq!(loaded.revision, 2);

        // No `.tmp.` files left behind.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "atomic write left tempfiles: {leftover:?}"
        );
    }

    #[test]
    fn save_creates_parent_directory() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("catalog_overlay.json");

        // Act
        save(&nested, 0, BTreeMap::new()).expect("save");

        // Assert
        assert!(nested.exists());
    }

    // -----------------------------------------------------------------------
    // Revision-checked write: stale expected_revision is rejected, file
    // byte-unchanged; matching revision writes revision + 1.
    // -----------------------------------------------------------------------

    #[test]
    fn save_rejects_stale_expected_revision_and_leaves_file_unchanged() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert("a:b".to_string(), Some(import_cell()));
        save(&path, 0, cells).expect("first save at revision 0 -> 1");
        let before = std::fs::read(&path).unwrap();

        // Act: stale expected_revision (0, but on-disk is now 1).
        let mut conflicting = BTreeMap::new();
        conflicting.insert("c:d".to_string(), Some(user_cell()));
        let err = save(&path, 0, conflicting).expect_err("stale revision must conflict");

        // Assert
        match err {
            OverlayError::RevisionConflict { expected, actual } => {
                assert_eq!(expected, 0);
                assert_eq!(actual, 1);
            }
            other => panic!("expected RevisionConflict, got {other:?}"),
        }
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "file must be byte-unchanged on conflict");
    }

    #[test]
    fn save_with_matching_revision_writes_revision_plus_one() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let first = save(&path, 0, BTreeMap::new()).expect("first save");
        assert_eq!(first.revision, 1);

        // Act
        let second = save(&path, 1, BTreeMap::new()).expect("second save at matching revision");

        // Assert
        assert_eq!(second.revision, 2);
    }

    // -----------------------------------------------------------------------
    // default_path resolves under routectl_config_dir() with the expected
    // basename.
    // -----------------------------------------------------------------------

    #[test]
    fn default_path_uses_catalog_overlay_json_basename() {
        // Act
        let path = default_path();

        // Assert
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("catalog_overlay.json")
        );
    }

    // -----------------------------------------------------------------------
    // overlay_revision: thin accessor.
    // -----------------------------------------------------------------------

    #[test]
    fn overlay_revision_reads_the_current_revision() {
        // Arrange
        let overlay = CatalogOverlay {
            revision: 7,
            ..CatalogOverlay::default()
        };

        // Act / Assert
        assert_eq!(overlay_revision(&overlay), 7);
    }

    // -----------------------------------------------------------------------
    // with_overlay_write_lock: load -> mutate -> save under one lock hold.
    // -----------------------------------------------------------------------

    #[test]
    fn with_overlay_write_lock_persists_the_closures_mutation() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        // Act
        let saved = with_overlay_write_lock::<OverlayError, _>(&path, |overlay| {
            let mut next = overlay;
            next.cells.insert("a:b".to_string(), Some(import_cell()));
            Ok(next)
        })
        .expect("write");

        // Assert: persisted at revision 1, and the lock is released
        // (a second write against the same path succeeds too).
        assert_eq!(saved.revision, 1);
        let loaded = load(&path).expect("load");
        assert_eq!(loaded.cells.get("a:b"), Some(&Some(import_cell())));

        let saved_again = with_overlay_write_lock::<OverlayError, _>(&path, |overlay| {
            let mut next = overlay;
            next.cells.insert("c:d".to_string(), Some(user_cell()));
            Ok(next)
        })
        .expect("second write after the first released its lock");
        assert_eq!(saved_again.revision, 2);
    }

    #[test]
    fn with_overlay_write_lock_propagates_an_abort_without_writing() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        save(&path, 0, BTreeMap::new()).expect("seed");
        let before = std::fs::read(&path).unwrap();

        // Act: the closure aborts instead of returning a mutated overlay.
        let events = routectl_testkit::capture_events(|| {
            let err = with_overlay_write_lock::<OverlayError, _>(&path, |_overlay| {
                Err(OverlayError::Corrupt {
                    path: "n/a".to_string(),
                    reason: "caller-chosen abort".to_string(),
                })
            })
            .expect_err("closure abort must propagate");
            assert!(matches!(err, OverlayError::Corrupt { .. }));
        });

        // Assert: no save happened and no breadcrumb was emitted.
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "an aborted closure must not write");
        assert!(
            events.is_empty(),
            "an aborted write must not emit a breadcrumb: {events:?}"
        );
    }

    // -----------------------------------------------------------------------
    // changed_selectors: added / removed / edited / disabled-flip.
    // -----------------------------------------------------------------------

    #[test]
    fn changed_selectors_detects_added_removed_edited_and_disabled_flip() {
        // Arrange
        let mut before = BTreeMap::new();
        before.insert("keep-same:1".to_string(), Some(import_cell()));
        before.insert("removed:1".to_string(), Some(import_cell()));
        before.insert("edited:1".to_string(), Some(import_cell()));
        before.insert("flip-to-disabled:1".to_string(), Some(import_cell()));

        let mut after = BTreeMap::new();
        after.insert("keep-same:1".to_string(), Some(import_cell()));
        after.insert("edited:1".to_string(), Some(user_cell()));
        after.insert("flip-to-disabled:1".to_string(), None);
        after.insert("added:1".to_string(), Some(user_cell()));

        // Act
        let changed = changed_selectors(&before, &after);

        // Assert: every non-identical key, sorted; "keep-same" excluded.
        assert_eq!(
            changed,
            vec![
                "added:1".to_string(),
                "edited:1".to_string(),
                "flip-to-disabled:1".to_string(),
                "removed:1".to_string(),
            ]
        );
    }

    #[test]
    fn with_overlay_write_lock_emits_one_breadcrumb_with_revision_and_changed_selectors() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        save(&path, 0, BTreeMap::new()).expect("seed at revision 1");

        // Act
        let events = routectl_testkit::capture_events(|| {
            with_overlay_write_lock::<OverlayError, _>(&path, |overlay| {
                let mut next = overlay;
                next.cells.insert("a:b".to_string(), Some(import_cell()));
                Ok(next)
            })
            .expect("write");
        });

        // Assert: exactly one structured line, carrying the new revision
        // and the changed selector.
        assert_eq!(events.len(), 1, "events: {events:?}");
        let event = &events[0];
        assert_eq!(event.field("revision"), Some("2"));
        assert_eq!(event.field("changed_selectors"), Some("a:b"));
    }

    #[test]
    #[serial_test::serial]
    fn concurrent_writers_against_the_same_path_serialize_with_no_lost_update() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        let path_a = path.clone();
        let path_b = path.clone();

        // Act: two threads race to write different selectors through the
        // same lock.
        let writer_a = std::thread::spawn(move || {
            with_overlay_write_lock::<OverlayError, _>(&path_a, |overlay| {
                let mut next = overlay;
                next.cells.insert("a:b".to_string(), Some(import_cell()));
                Ok(next)
            })
        });
        let writer_b = std::thread::spawn(move || {
            with_overlay_write_lock::<OverlayError, _>(&path_b, |overlay| {
                let mut next = overlay;
                next.cells.insert("c:d".to_string(), Some(user_cell()));
                Ok(next)
            })
        });

        let result_a = writer_a.join().expect("writer_a thread");
        let result_b = writer_b.join().expect("writer_b thread");

        // Assert: both writers succeed (serialized, never a
        // RevisionConflict), and the final overlay carries both writes.
        result_a.expect("writer_a must not see a revision conflict");
        result_b.expect("writer_b must not see a revision conflict");

        let loaded = load(&path).expect("load");
        assert_eq!(loaded.revision, 2, "two serialized writes -> revision 2");
        assert_eq!(loaded.cells.get("a:b"), Some(&Some(import_cell())));
        assert_eq!(loaded.cells.get("c:d"), Some(&Some(user_cell())));
    }

    #[test]
    #[serial_test::serial]
    fn concurrent_writer_blocks_on_the_lock_held_by_another_thread() {
        // Arrange: the holder signals once it is inside the locked closure,
        // then sleeps for `HOLD` before returning -- proving a real overlap
        // (not just lucky thread scheduling) requires the waiter's total
        // wait time to be at least `HOLD`.
        const HOLD: std::time::Duration = std::time::Duration::from_millis(200);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let (holder_ready_tx, holder_ready_rx) = std::sync::mpsc::channel::<()>();

        let path_holder = path.clone();
        let holder = std::thread::spawn(move || {
            with_overlay_write_lock::<OverlayError, _>(&path_holder, move |overlay| {
                holder_ready_tx
                    .send(())
                    .expect("signal holder is inside the lock");
                std::thread::sleep(HOLD);
                let mut next = overlay;
                next.cells.insert("a:b".to_string(), Some(import_cell()));
                Ok(next)
            })
        });

        holder_ready_rx
            .recv()
            .expect("holder must signal before the waiter starts");

        // Act: the waiter can only proceed once the holder's sleep +
        // save + lock release completes.
        let waiter_start = std::time::Instant::now();
        let path_waiter = path.clone();
        let waiter_result = with_overlay_write_lock::<OverlayError, _>(&path_waiter, |overlay| {
            let mut next = overlay;
            next.cells.insert("c:d".to_string(), Some(user_cell()));
            Ok(next)
        });
        let elapsed = waiter_start.elapsed();

        // Assert
        waiter_result.expect("waiter must succeed after the holder releases");
        holder
            .join()
            .expect("holder thread")
            .expect("holder must succeed");
        assert!(
            elapsed >= HOLD,
            "waiter must block until the holder releases the lock, elapsed={elapsed:?}"
        );
    }
}
