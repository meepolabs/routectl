//! The single `config.toml` write path: an advisory-lock + base-bytes
//! revision-checked, format-preserving atomic edit primitive.
//!
//! Every cooperative routectl writer that edits an existing `config.toml`
//! (`config set`, `init`, the login config surface) goes through
//! [`edit_config_toml`]; laying down a fresh file goes through
//! [`write_config_new`], which never replaces one. [`edit_config_toml`] owns
//! three concerns and nothing else -- it is a write primitive, NOT a
//! mutation-DTO layer and NOT a validator:
//!
//!   1. Sibling advisory lock (`config.toml.lock`, mirroring
//!      [`crate::catalog_overlay::with_overlay_write_lock`]) so two
//!      cooperative routectl processes serialize rather than clobber.
//!   2. Base-bytes revision check: the caller snapshots the raw file bytes
//!      when it first reads the config; this primitive re-reads under the
//!      lock and refuses ([`ConfigWriteError::Conflict`], NO write, no retry
//!      loop) if they differ. This catches a hand-edit or a
//!      non-cooperative writer that the advisory lock alone cannot -- there
//!      is deliberately no `revision:` counter field in `config.toml`.
//!   3. Mode-preserving atomic write ([`write_config_atomic`]): temp file in
//!      the same directory, fsync, atomic rename, parent-directory fsync.
//!
//! Validation is the CALLER's job and lives in the `edit_fn` closure: the
//! caller mutates the [`toml_edit::DocumentMut`], serializes it, runs
//! whatever parse/validation gate it needs, and returns [`EditOutcome`]
//! (`Modified` triggers the write; `Unchanged` skips it). A validation
//! failure is surfaced as the closure's own error `E`, wrapped in
//! [`ConfigWriteError::Edit`] -- the primitive stays validation-agnostic.
//!
//! Lock scope is re-read + commit ONLY. It is NEVER held across an
//! interactive confirmation prompt or a network fetch: callers that need
//! either do that work BEFORE calling this function and pass only the
//! already-decided mutation into `edit_fn`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Whether the `edit_fn` closure actually changed the document. `Unchanged`
/// short-circuits the write entirely (no temp file, no rename, nothing the
/// daemon's file watcher can observe) -- callers should return it when the
/// requested edit is a no-op so an unchanged config never triggers a
/// spurious hot reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    /// The document was mutated; commit it atomically.
    Modified,
    /// The document was left as-is; write nothing.
    Unchanged,
}

/// Outcome of an [`edit_config_toml`] call.
#[derive(Debug, Clone)]
pub struct EditResult {
    /// What the closure decided.
    pub outcome: EditOutcome,
    /// Byte length of the document now on disk. Intentionally NOT the
    /// document text: config.toml can carry `literal:` secrets, and
    /// handing the raw bytes back would invite a caller to log them
    /// (the audit contract forbids logging values). A caller needing
    /// the new content captures it inside `edit_fn`.
    pub committed_len: usize,
}

/// Errors from [`edit_config_toml`]. Every variant fails closed: on any
/// error the file on disk is left BYTE-IDENTICAL to what it was when the
/// function was called (no temp file is ever persisted).
///
/// Generic over the closure's own error `E` so a caller's validation
/// failure ([`Edit`](ConfigWriteError::Edit)) keeps its concrete type and
/// full rendering, rather than being flattened to a string.
#[derive(Debug, thiserror::Error)]
pub enum ConfigWriteError<E> {
    /// The file changed on disk between the caller's snapshot and the
    /// re-read under the lock (a concurrent cooperative writer, or an
    /// out-of-band hand-edit). Nothing was written; the caller must
    /// re-read and decide again.
    #[error(
        "config `{path}` changed on disk since it was read; nothing was written -- \
         re-read the file and retry"
    )]
    Conflict {
        /// Path to the config file.
        path: String,
    },

    /// A filesystem failure (locking, reading, or the atomic write itself).
    #[error("config `{path}`: {reason}")]
    Io {
        /// Path to the config file.
        path: String,
        /// Underlying I/O error message.
        reason: String,
    },

    /// The re-read bytes did not parse as a TOML document.
    #[error("config `{path}`: parse: {reason}")]
    Parse {
        /// Path to the config file.
        path: String,
        /// Why the document failed to parse.
        reason: String,
    },

    /// The `edit_fn` closure rejected the edit (typically a validation
    /// failure on the candidate document); the caller's error is preserved
    /// verbatim.
    #[error(transparent)]
    Edit(E),
}

/// Edit `config.toml` at `path` under the advisory lock, refusing if the
/// file changed since `base_bytes_snapshot` was captured.
///
/// Sequence (the lock is held only for steps 2-6):
///   1. Acquire the sibling `config.toml.lock` advisory write lock (blocks
///      until any concurrent cooperative writer releases it).
///   2. Re-read the file's raw bytes.
///   3. Compare them to `base_bytes_snapshot`; a mismatch is a
///      [`ConfigWriteError::Conflict`] -- return WITHOUT writing.
///   4. Parse the re-read bytes into a [`toml_edit::DocumentMut`].
///   5. Run `edit_fn` against the document. It mutates in place and returns
///      [`EditOutcome`] (or its own error, surfaced as
///      [`ConfigWriteError::Edit`]).
///   6. On [`EditOutcome::Modified`], atomically write the serialized
///      document; on [`EditOutcome::Unchanged`], write nothing.
///
/// `base_bytes_snapshot` MUST be the exact bytes the caller read from this
/// same `path` earlier (a byte-for-byte comparison, not a parsed compare),
/// so any out-of-band edit -- even one that re-parses identically -- is
/// treated as a conflict.
///
/// The caller is responsible for all validation and for any interactive
/// confirmation, both of which happen inside `edit_fn` or before this call
/// respectively -- never while the lock is held.
pub fn edit_config_toml<E, F>(
    path: &Path,
    base_bytes_snapshot: &[u8],
    edit_fn: F,
) -> Result<EditResult, ConfigWriteError<E>>
where
    E: std::error::Error,
    F: FnOnce(&mut toml_edit::DocumentMut) -> Result<EditOutcome, E>,
{
    let display = path.display().to_string();

    let lock_path = lock_path_for(path);
    let lock_file = open_lock_file(&lock_path).map_err(|e| ConfigWriteError::Io {
        path: lock_path.display().to_string(),
        reason: format!("open lock file: {e}"),
    })?;
    let mut file_lock = fd_lock::RwLock::new(lock_file);
    let _guard = file_lock.write().map_err(|e| ConfigWriteError::Io {
        path: lock_path.display().to_string(),
        reason: format!("acquire advisory write lock: {e}"),
    })?;

    let current = std::fs::read(path).map_err(|e| ConfigWriteError::Io {
        path: display.clone(),
        reason: format!("re-read under lock: {e}"),
    })?;

    if current != base_bytes_snapshot {
        return Err(ConfigWriteError::Conflict { path: display });
    }

    let text = String::from_utf8(current).map_err(|e| ConfigWriteError::Parse {
        path: display.clone(),
        reason: format!("not valid UTF-8: {e}"),
    })?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigWriteError::Parse {
            path: display.clone(),
            reason: e.to_string(),
        })?;

    match edit_fn(&mut doc).map_err(ConfigWriteError::Edit)? {
        EditOutcome::Unchanged => Ok(EditResult {
            outcome: EditOutcome::Unchanged,
            committed_len: base_bytes_snapshot.len(),
        }),
        EditOutcome::Modified => {
            let bytes = doc.to_string().into_bytes();
            write_config_atomic(path, &bytes).map_err(|reason| ConfigWriteError::Io {
                path: display,
                reason,
            })?;
            Ok(EditResult {
                outcome: EditOutcome::Modified,
                committed_len: bytes.len(),
            })
        }
    }
}

/// Temp-file-then-rename write, mirroring `catalog_overlay`'s writer
/// discipline (fsync before rename), but preserving the ORIGINAL file's
/// permission bits instead of forcing `0o600` -- `config.toml` is
/// operator-owned and its existing mode is deliberate, not a default a
/// routectl writer should override.
///
/// Also fsyncs the PARENT DIRECTORY after the rename: `rename` is atomic
/// for concurrent readers, but the rename itself is not durable across a
/// crash/power loss until the directory entry pointing at the new inode is
/// flushed too -- without this, a crash right after this call returns can
/// roll the directory entry back to the pre-rename file even though the
/// temp file's own contents were already fsynced.
pub fn write_config_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?;

    #[cfg(unix)]
    let original_mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).ok().map(|m| m.permissions().mode())
    };

    let tmp = synced_temp_in(parent, bytes).map_err(|(stage, e)| match stage {
        TempStage::Create => format!("tempfile: {e}"),
        TempStage::Write => format!("write tempfile: {e}"),
        TempStage::Fsync => format!("fsync tempfile: {e}"),
    })?;
    tmp.persist(path).map_err(|e| format!("rename: {e}"))?;

    #[cfg(unix)]
    if let Some(mode) = original_mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    sync_parent_dir(parent);
    Ok(())
}

/// Why a [`write_config_new`] call failed, by stage. On every error nothing
/// is written at the target path and the temp file is removed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigCreateError {
    /// A file is already present at the target path; it was left untouched.
    #[error("config already exists")]
    AlreadyExists,
    /// The target path has no (or an empty) parent directory component.
    #[error("path has no parent directory")]
    NoParent,
    /// Creating the temp file in the parent directory failed.
    #[error("tempfile: {0}")]
    Tempfile(std::io::Error),
    /// Writing the bytes into the temp file failed.
    #[error("write: {0}")]
    Write(std::io::Error),
    /// Flushing the temp file to disk failed.
    #[error("fsync: {0}")]
    Fsync(std::io::Error),
    /// Linking the temp file into place failed for a reason other than an
    /// existing file.
    #[error("publish: {0}")]
    Publish(std::io::Error),
}

/// Create `path` holding `bytes`, never replacing an existing file: a fsynced
/// temp file in the parent directory is linked into place with a no-clobber
/// publish, then the parent directory is fsynced. A file already at `path`
/// (including one that appears between any caller check and the publish)
/// yields [`ConfigCreateError::AlreadyExists`].
///
/// The parent directory must already exist; it is not created.
pub fn write_config_new(path: &Path, bytes: &[u8]) -> Result<(), ConfigCreateError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or(ConfigCreateError::NoParent)?;

    let tmp = synced_temp_in(parent, bytes).map_err(|(stage, e)| match stage {
        TempStage::Create => ConfigCreateError::Tempfile(e),
        TempStage::Write => ConfigCreateError::Write(e),
        TempStage::Fsync => ConfigCreateError::Fsync(e),
    })?;
    tmp.persist_noclobber(path).map_err(|e| {
        if e.error.kind() == std::io::ErrorKind::AlreadyExists {
            ConfigCreateError::AlreadyExists
        } else {
            ConfigCreateError::Publish(e.error)
        }
    })?;

    sync_parent_dir(parent);
    Ok(())
}

/// Which step of [`synced_temp_in`] failed.
enum TempStage {
    Create,
    Write,
    Fsync,
}

/// A temp file in `parent` (so the publish is a same-filesystem rename)
/// holding `bytes`, already fsynced.
fn synced_temp_in(
    parent: &Path,
    bytes: &[u8],
) -> Result<tempfile::NamedTempFile, (TempStage, std::io::Error)> {
    let mut tmp = tempfile::Builder::new()
        .prefix(".config.tmp.")
        .suffix(".toml")
        .tempfile_in(parent)
        .map_err(|e| (TempStage::Create, e))?;
    tmp.write_all(bytes).map_err(|e| (TempStage::Write, e))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| (TempStage::Fsync, e))?;
    Ok(tmp)
}

/// Best-effort fsync of the parent directory so a just-published directory
/// entry survives a crash. A failure is ignored: the file is already
/// published and readable by the time this runs.
fn sync_parent_dir(parent: &Path) {
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
}

/// Path to the sibling advisory-lock file for `path`, e.g.
/// `config.toml` -> `config.toml.lock`. The lock file's own contents are
/// never read or written -- it exists purely as a kernel lock handle, so a
/// stale empty lock file left behind by an old build is harmless.
fn lock_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Open (creating if absent) the sibling lock file for [`fd_lock::RwLock`]
/// to hold. Read+write access, never truncated -- the file's contents are
/// unused.
fn open_lock_file(lock_path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal closure error, standing in for a caller's validation
    /// failure so the [`ConfigWriteError::Edit`] path is exercised.
    #[derive(Debug, thiserror::Error)]
    #[error("edit rejected in test: {0}")]
    struct TestEditError(&'static str);

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn temp_residue(dir: &Path) -> Vec<PathBuf> {
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

    // -----------------------------------------------------------------------
    // Happy path: a Modified edit rewrites the file, preserves comments, and
    // hands back the committed bytes.
    // -----------------------------------------------------------------------

    #[test]
    fn modified_edit_writes_and_preserves_comments() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let body = "# operator note\nversion = 2\n\n[server]\nport = 8787\n";
        let path = write_config(dir.path(), body);
        let snapshot = std::fs::read(&path).unwrap();

        // Act
        let result = edit_config_toml::<TestEditError, _>(&path, &snapshot, |doc| {
            doc["server"]["port"] = toml_edit::value(9999);
            Ok(EditOutcome::Modified)
        })
        .expect("edit");

        // Assert
        assert_eq!(result.outcome, EditOutcome::Modified);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.len(), result.committed_len);
        assert!(on_disk.contains("port = 9999"), "on_disk: {on_disk}");
        assert!(on_disk.contains("# operator note"), "on_disk: {on_disk}");
        assert!(temp_residue(dir.path()).is_empty());
    }

    // -----------------------------------------------------------------------
    // Unchanged: closure declines to write; the file is byte-identical and
    // no temp file is produced.
    // -----------------------------------------------------------------------

    #[test]
    fn unchanged_edit_writes_nothing() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let body = "version = 2\n[server]\nport = 8787\n";
        let path = write_config(dir.path(), body);
        let snapshot = std::fs::read(&path).unwrap();

        // Act
        let result = edit_config_toml::<TestEditError, _>(&path, &snapshot, |_doc| {
            Ok(EditOutcome::Unchanged)
        })
        .expect("edit");

        // Assert
        assert_eq!(result.outcome, EditOutcome::Unchanged);
        assert_eq!(std::fs::read(&path).unwrap(), snapshot);
        assert_eq!(result.committed_len, snapshot.len());
        assert!(temp_residue(dir.path()).is_empty());
    }

    // -----------------------------------------------------------------------
    // Abort: the closure erroring leaves the file byte-identical with no
    // temp residue, even though it claimed a mutation.
    // -----------------------------------------------------------------------

    #[test]
    fn edit_fn_error_leaves_file_byte_identical() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let body = "version = 2\n[server]\nport = 8787\n";
        let path = write_config(dir.path(), body);
        let snapshot = std::fs::read(&path).unwrap();

        // Act
        let err = edit_config_toml::<TestEditError, _>(&path, &snapshot, |doc| {
            doc["server"]["port"] = toml_edit::value(1); // mutate, then reject
            Err(TestEditError("candidate failed validation"))
        })
        .expect_err("closure error must propagate");

        // Assert
        assert!(matches!(err, ConfigWriteError::Edit(_)), "err: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), snapshot);
        assert!(temp_residue(dir.path()).is_empty());
    }

    // -----------------------------------------------------------------------
    // Conflict: a full external overwrite between snapshot and commit is a
    // stale-snapshot conflict; the closure never runs and nothing is written.
    // -----------------------------------------------------------------------

    #[test]
    fn stale_snapshot_conflicts_and_writes_nothing() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "version = 2\nport = 1\n");
        let stale_snapshot = std::fs::read(&path).unwrap();
        // Something else rewrote the file after we snapshotted it.
        std::fs::write(&path, "version = 2\nport = 2\n").unwrap();
        let current = std::fs::read(&path).unwrap();

        // Act
        let err = edit_config_toml::<TestEditError, _>(&path, &stale_snapshot, |doc| {
            doc["port"] = toml_edit::value(3);
            Ok(EditOutcome::Modified)
        })
        .expect_err("stale snapshot must conflict");

        // Assert: conflict, and the on-disk file is untouched.
        assert!(
            matches!(err, ConfigWriteError::Conflict { .. }),
            "err: {err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), current);
        assert!(temp_residue(dir.path()).is_empty());
    }

    // -----------------------------------------------------------------------
    // Conflict: an operator hand-edit (a single appended comment byte)
    // between snapshot and commit is likewise a conflict, no write.
    // -----------------------------------------------------------------------

    #[test]
    fn hand_edit_between_read_and_commit_conflicts() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let body = "version = 2\n[server]\nport = 8787\n";
        let path = write_config(dir.path(), body);
        let snapshot = std::fs::read(&path).unwrap();
        // Operator appends a comment out of band.
        let hand_edited = format!("{body}# added by hand\n");
        std::fs::write(&path, &hand_edited).unwrap();

        // Act
        let err = edit_config_toml::<TestEditError, _>(&path, &snapshot, |doc| {
            doc["server"]["port"] = toml_edit::value(1);
            Ok(EditOutcome::Modified)
        })
        .expect_err("hand-edit must conflict");

        // Assert
        assert!(
            matches!(err, ConfigWriteError::Conflict { .. }),
            "err: {err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), hand_edited);
        assert!(temp_residue(dir.path()).is_empty());
    }

    // -----------------------------------------------------------------------
    // Mode preservation: a non-default permission bit survives the write.
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "version = 2\nport = 1\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let snapshot = std::fs::read(&path).unwrap();

        // Act
        edit_config_toml::<TestEditError, _>(&path, &snapshot, |doc| {
            doc["port"] = toml_edit::value(2);
            Ok(EditOutcome::Modified)
        })
        .expect("edit");

        // Assert
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "mode must survive the atomic rewrite");
    }

    // -----------------------------------------------------------------------
    // Create-new: lays down a fresh file, never replaces an existing one.
    // -----------------------------------------------------------------------

    #[test]
    fn write_config_new_creates_the_file_with_the_bytes() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let bytes = b"version = 2\n[server]\nport = 8787\n";

        // Act
        write_config_new(&path, bytes).expect("fresh create");

        // Assert
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(temp_residue(dir.path()).is_empty());
    }

    #[test]
    fn write_config_new_refuses_an_existing_file_and_leaves_it_byte_identical() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let existing = "version = 2\n# operator file\n";
        let path = write_config(dir.path(), existing);

        // Act
        let err = write_config_new(&path, b"version = 2\nport = 1\n")
            .expect_err("existing file must refuse");

        // Assert
        assert!(
            matches!(err, ConfigCreateError::AlreadyExists),
            "err: {err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
        assert!(temp_residue(dir.path()).is_empty());
    }

    #[test]
    fn write_config_new_reports_a_missing_parent_as_the_tempfile_stage() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join("config.toml");

        // Act
        let err = write_config_new(&path, b"version = 2\n").expect_err("no parent dir");

        // Assert
        assert!(
            matches!(&err, ConfigCreateError::Tempfile(e) if e.kind() == std::io::ErrorKind::NotFound),
            "err: {err:?}"
        );
        assert!(err.to_string().starts_with("tempfile: "), "err: {err}");
        assert!(!path.exists());
    }

    #[test]
    fn write_config_new_reports_a_non_existence_publish_failure_as_the_publish_stage() {
        // Arrange: a file name past NAME_MAX fails only at the link into place.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c".repeat(300));

        // Act
        let err = write_config_new(&path, b"version = 2\n").expect_err("name too long");

        // Assert
        assert!(matches!(err, ConfigCreateError::Publish(_)), "err: {err:?}");
        assert!(err.to_string().starts_with("publish: "), "err: {err}");
        assert!(temp_residue(dir.path()).is_empty());
    }

    #[test]
    fn write_config_new_refuses_a_path_without_a_parent_component() {
        // Act
        let err = write_config_new(Path::new("config.toml"), b"version = 2\n")
            .expect_err("bare file name has an empty parent");

        // Assert
        assert!(matches!(err, ConfigCreateError::NoParent), "err: {err:?}");
    }

    // -----------------------------------------------------------------------
    // Concurrency: a second writer blocks on the advisory lock until the
    // first releases it. The holder returns Unchanged (no write) so the
    // waiter's snapshot stays valid and the test isolates lock behavior
    // from the conflict path. Mirrors catalog_overlay's lock-blocking test.
    // -----------------------------------------------------------------------

    #[test]
    #[serial_test::serial]
    fn concurrent_writer_blocks_on_the_lock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // The holder signals it is inside the lock, then stays in its
        // critical section for this window before flipping `holder_released`
        // just before returning. If the advisory lock were NOT honored the
        // waiter would enter its own critical section during this window and
        // observe `holder_released` still false; a working lock forces the
        // waiter to block until the holder's `edit_fn` has returned, so it
        // always observes `true`. The invariant is asserted on this observed
        // ordering, never on wall-clock elapsed time -- an earlier version
        // compared the waiter's elapsed time against this window and flaked
        // because the two clocks (holder-internal vs waiter-observed) are
        // offset by channel-delivery and scheduling latency.
        const HOLD: std::time::Duration = std::time::Duration::from_millis(200);

        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "version = 2\nport = 1\n");
        let snapshot = std::fs::read(&path).unwrap();
        let holder_released = Arc::new(AtomicBool::new(false));
        let (holder_ready_tx, holder_ready_rx) = std::sync::mpsc::channel::<()>();

        let path_holder = path.clone();
        let snapshot_holder = snapshot.clone();
        let holder_released_writer = Arc::clone(&holder_released);
        let holder = std::thread::spawn(move || {
            edit_config_toml::<TestEditError, _>(&path_holder, &snapshot_holder, move |_doc| {
                holder_ready_tx.send(()).expect("signal inside the lock");
                std::thread::sleep(HOLD);
                holder_released_writer.store(true, Ordering::SeqCst);
                Ok(EditOutcome::Unchanged)
            })
        });

        holder_ready_rx
            .recv()
            .expect("holder must signal before the waiter starts");

        // The holder now provably holds the lock. This second writer must
        // block until the holder's critical section completes; by the time
        // its own closure runs, the holder must already have released.
        let holder_released_reader = Arc::clone(&holder_released);
        let waiter = edit_config_toml::<TestEditError, _>(&path, &snapshot, move |_doc| {
            assert!(
                holder_released_reader.load(Ordering::SeqCst),
                "waiter entered the lock before the holder released it"
            );
            Ok(EditOutcome::Unchanged)
        });

        waiter.expect("waiter must succeed after the holder releases");
        holder
            .join()
            .expect("holder thread")
            .expect("holder must succeed");
    }
}
