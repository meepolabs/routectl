//! Crate-internal atomic writer for owner-only secret files.
//!
//! One shared implementation of the temp-file + fsync + rename +
//! force-`0o600` sequence used by every routectl-auth writer that puts
//! long-lived secrets on disk (the OAuth credentials store, the
//! secret-capture primitive). It force-sets `0o600`: unlike the
//! operator-owned `config.toml` writer in routectl-router (which
//! PRESERVES the existing mode), a secrets file has one correct mode and
//! a routectl writer always asserts it.
//!
//! Durability: the temp file is fsynced before the rename, and the
//! PARENT DIRECTORY is fsynced after it. `rename` is atomic for
//! concurrent readers, but the new directory entry is not durable across
//! a crash/power loss until the directory itself is flushed -- without
//! the parent fsync a crash right after this returns can roll the
//! directory entry back to the pre-rename file even though the temp
//! file's own contents were already on stable storage.

// This writer stays unconditional (not behind the `oauth` feature)
// because the crate-wide secret-capture primitive consumes it on every
// build, lean or not; the OAuth credentials store is an additional caller
// only when `oauth` is enabled.

use std::path::Path;

/// Atomically write `bytes` to `path` as an owner-only (`0o600`) file.
///
/// Steps:
///   1. Ensure the parent directory exists with `0o700` (idempotent and
///      self-healing on every call).
///   2. Create a temp file in the SAME directory, forcing `0o600` BEFORE
///      any bytes are written so a partial write never has wider perms.
///   3. Write the bytes and fsync the temp file.
///   4. Atomically rename the temp file over `path`.
///   5. Re-chmod `path` to `0o600` as defense-in-depth.
///   6. Fsync the parent directory so the rename is durable.
///
/// Returns a human-readable error string on failure; callers map it to
/// their own error type. On any error nothing is persisted at `path`
/// (the temp file is cleaned up by its `Drop`).
pub fn write_0600_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .ok_or_else(|| format!("path has no parent directory: {}", path.display()))?;

    ensure_dir_0700(parent)?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp.")
        .tempfile_in(parent)
        .map_err(|e| format!("tempfile in {}: {e}", parent.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod tempfile: {e}"))?;
    }

    tmp.write_all(bytes)
        .map_err(|e| format!("write tempfile: {e}"))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| format!("fsync tempfile: {e}"))?;

    tmp.persist(path)
        .map_err(|e| format!("rename to {}: {e}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::File::open(parent)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("fsync parent {}: {e}", parent.display()))?;

    Ok(())
}

/// Create `dir` (and ancestors) if absent and force `0o700`. Idempotent:
/// re-asserting the mode on every save self-heals a directory the
/// operator chmodded wider by accident.
fn ensure_dir_0700(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create_dir_all {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_residue(dir: &Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".tmp."))
            })
            .collect()
    }

    #[test]
    fn writes_bytes_to_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_0600_atomic(&path, b"payload").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("secret");
        write_0600_atomic(&path, b"x").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn overwrites_and_leaves_no_temp_residue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_0600_atomic(&path, b"first").unwrap();
        write_0600_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(
            tmp_residue(dir.path()).is_empty(),
            "atomic write left tempfiles: {:?}",
            tmp_residue(dir.path())
        );
    }

    #[test]
    fn error_on_missing_parent_leaves_no_residue() {
        let dir = tempfile::tempdir().unwrap();
        // A path whose parent is a FILE (not a directory) forces the
        // parent-dir creation to fail; nothing must be persisted.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let path = blocker.join("secret");
        let err = write_0600_atomic(&path, b"x").unwrap_err();
        assert!(err.contains("create_dir_all"), "got: {err}");
        assert!(tmp_residue(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn sets_0600_on_target_and_0700_on_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("nested");
        let path = parent.join("secret");
        write_0600_atomic(&path, b"x").unwrap();

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file must be 0600, got {file_mode:o}");

        let dir_mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "parent must be 0700, got {dir_mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn re_asserts_0700_on_a_widened_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("nested");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = parent.join("secret");
        write_0600_atomic(&path, b"x").unwrap();
        let dir_mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "parent must be re-narrowed to 0700");
    }
}
