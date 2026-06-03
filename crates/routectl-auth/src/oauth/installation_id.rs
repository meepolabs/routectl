//! Per-install `x-codex-installation-id` persistence.
//!
//! Mirrors codex CLI's `installation_id`: one UUIDv4 stamped per
//! routectl install, kept in a sibling file next to credentials.json
//! (`$XDG_CONFIG_HOME/routectl/installation_id`, fallback
//! `$HOME/.config/routectl/installation_id`). Unlike `session_id`
//! (per-credential, in credentials.json), the installation id is
//! global to the install and survives `routectl login` re-runs --
//! deleting the file is the only way to mint a new one.
//!
//! Persistence shape: a single line carrying the UUID string, no
//! schema versioning, mode `0o600`. Atomic write via a tempfile in
//! the same directory + `fsync` + `rename`, identical to credentials.
//!
//! The `OnceLock` cache means a single read-or-generate per process;
//! every subsequent provider construction (including hot-reload of
//! the router) reuses the same in-memory string without touching the
//! disk.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::oauth::{OAuthError, OAuthResult};

/// Resolves the installation_id file path, parallel to
/// `file_io::default_path()` for credentials.json.
///
/// `$XDG_CONFIG_HOME/routectl/installation_id`, falling back to
/// `$HOME/.config/routectl/installation_id`. Returns an error when
/// neither env var is set (broken environment).
pub fn default_path() -> OAuthResult<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("routectl").join("installation_id"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| OAuthError::Internal("neither XDG_CONFIG_HOME nor HOME is set".into()))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("routectl")
        .join("installation_id"))
}

/// Process-global cache for the default-path installation id.
/// Lazily populated on first read; subsequent reads are zero-cost.
static CACHED_DEFAULT: OnceLock<String> = OnceLock::new();

/// Read the installation id from the default path, generating and
/// persisting one when the file is missing. Cached after the first
/// successful read so repeat calls do not stat the disk.
pub fn read_or_generate_default() -> OAuthResult<String> {
    if let Some(cached) = CACHED_DEFAULT.get() {
        return Ok(cached.clone());
    }
    let path = default_path()?;
    let id = read_or_generate_at(&path)?;
    // OnceLock::set is a no-op if another caller raced and won; that
    // is fine because both winners read the same file.
    let _ = CACHED_DEFAULT.set(id.clone());
    Ok(id)
}

/// Read the installation id from `path`, generating and persisting
/// one when the file is missing. Used directly only in tests; the
/// production path is `read_or_generate_default`.
pub fn read_or_generate_at(path: &Path) -> OAuthResult<String> {
    match read_existing(path)? {
        Some(id) => Ok(id),
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            write_atomic(path, &id)?;
            Ok(id)
        }
    }
}

/// Best-effort sanity bound: a UUID string is 36 chars; a kilobyte
/// gives generous headroom for trailing whitespace or future schema
/// growth without letting a misconfigured inode (a TOCTOU symlink to
/// /dev/zero, say) drive the loader toward OOM.
const MAX_INSTALLATION_ID_BYTES: u64 = 1024;

fn read_existing(path: &Path) -> OAuthResult<Option<String>> {
    use std::io::Read;

    let display = path.display().to_string();
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(OAuthError::Io(format!("open {display}: {e}"))),
    };
    let meta = file
        .metadata()
        .map_err(|e| OAuthError::Io(format!("fstat {display}: {e}")))?;
    if !meta.is_file() {
        return Err(OAuthError::Io(format!(
            "installation id file {display} is not a regular file",
        )));
    }
    if meta.len() > MAX_INSTALLATION_ID_BYTES {
        return Err(OAuthError::Io(format!(
            "installation id file {display} is {} bytes; refusing to load (cap is {MAX_INSTALLATION_ID_BYTES} bytes)",
            meta.len()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(OAuthError::Io(format!(
                "installation id file {} has permissions {:o}; \
                 use chmod 600 to restrict reads to the owner",
                display,
                mode & 0o7777
            )));
        }
    }
    let mut buf = String::new();
    (&file)
        .take(MAX_INSTALLATION_ID_BYTES)
        .read_to_string(&mut buf)
        .map_err(|e| OAuthError::Io(format!("read {display}: {e}")))?;
    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        // An empty file is treated as "not present" so the caller
        // mints a fresh UUID rather than emitting an empty header.
        return Ok(None);
    }
    Ok(Some(trimmed))
}

fn write_atomic(path: &Path, id: &str) -> OAuthResult<()> {
    use std::io::Write;

    let display = path.display().to_string();
    let parent = path
        .parent()
        .ok_or_else(|| OAuthError::Internal(format!("path has no parent: {display}")))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| OAuthError::Io(format!("create_dir_all {}: {e}", parent.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".installation_id.tmp.")
        .tempfile_in(parent)
        .map_err(|e| OAuthError::Io(format!("tempfile in {}: {e}", parent.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| OAuthError::Io(format!("chmod tempfile: {e}")))?;
    }
    let mut bytes = id.as_bytes().to_vec();
    bytes.push(b'\n');
    tmp.write_all(&bytes)
        .map_err(|e| OAuthError::Io(format!("write tempfile: {e}")))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| OAuthError::Io(format!("fsync tempfile: {e}")))?;
    tmp.persist(path)
        .map_err(|e| OAuthError::Io(format!("rename to {display}: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_or_generate_creates_file_with_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installation_id");
        let id = read_or_generate_at(&path).unwrap();
        assert!(path.exists(), "file should be created on first read");
        // Round-trip: a second read returns the same id (NOT a fresh
        // UUID).
        let id2 = read_or_generate_at(&path).unwrap();
        assert_eq!(id, id2, "second read must return the persisted id");
        // The UUID string is 36 hex/dash chars.
        assert_eq!(id.len(), 36, "expected 36-char uuid, got {id:?}");
    }

    #[cfg(unix)]
    #[test]
    fn generated_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installation_id");
        let _ = read_or_generate_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "installation_id must be 0600, got {:o}",
            mode & 0o777,
        );
    }

    #[test]
    fn empty_file_treated_as_missing_and_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installation_id");
        std::fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let id = read_or_generate_at(&path).unwrap();
        assert_eq!(id.len(), 36, "expected 36-char uuid, got {id:?}");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_world_readable_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installation_id");
        std::fs::write(&path, b"some-id\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_or_generate_at(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("permissions"), "got: {msg}");
        assert!(msg.contains("chmod 600"), "got: {msg}");
    }
}
