//! Atomic load and save of `~/.config/routectl/credentials.json`.
//!
//! Read flow: open + fstat the open fd (TOCTOU-safe) + permissions
//! check + JSON parse. Returns `OAuthError::CorruptedFile` on parse
//! failure so the operator can delete the file and re-login. Returns
//! `Ok(CredentialsFile::empty())` on `NotFound` so first-run servers
//! do not have to special-case the missing file.
//!
//! Write flow: serialize to a temp file in the SAME directory (so
//! `rename` is atomic within one filesystem), `fsync` the temp file,
//! `rename` over the target. The old file remains valid if the rename
//! fails. On Unix, the temp file is created with mode `0o600` so a
//! partial write never has wider permissions than the final.

use std::path::{Path, PathBuf};

use crate::oauth::types::{CredentialsFile, SCHEMA_VERSION};
use crate::oauth::{OAuthError, OAuthResult};

/// Default path: `$XDG_CONFIG_HOME/routectl/credentials.json`, falling
/// back to `~/.config/routectl/credentials.json`. Returns an error if
/// neither `XDG_CONFIG_HOME` nor `HOME` is set (rare; unset HOME on
/// Linux indicates a broken environment).
pub fn default_path() -> OAuthResult<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("routectl").join("credentials.json"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| OAuthError::Internal("neither XDG_CONFIG_HOME nor HOME is set".into()))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("routectl")
        .join("credentials.json"))
}

/// Load the credentials file. NotFound -> empty file (first-run).
/// Schema-version mismatch -> `OAuthError::SchemaMismatch` with
/// operator guidance (re-login is the only supported "migration").
///
/// Mirrors `MemoryStore::read_secret_file` defenses: open the file
/// once, `fstat` the open fd, refuse non-regular-file inodes, refuse
/// permissions wider than `0o600` on Unix. The file holds long-lived
/// refresh tokens; the same hygiene as `file://` secrets applies.
pub async fn load(path: &Path) -> OAuthResult<CredentialsFile> {
    let path_owned = path.to_path_buf();
    let display = path_owned.display().to_string();

    let bytes = match tokio::task::spawn_blocking(move || read_validated_blocking(&path_owned))
        .await
        .map_err(|e| OAuthError::Internal(format!("load task failed: {e}")))?
    {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(CredentialsFile::empty()),
        Err(e) => return Err(e),
    };

    let parsed: CredentialsFile =
        serde_json::from_slice(&bytes).map_err(|e| OAuthError::CorruptedFile {
            path: display.clone(),
            detail: e.to_string(),
        })?;

    if parsed.schema_version != SCHEMA_VERSION {
        return Err(OAuthError::SchemaMismatch {
            found: parsed.schema_version,
            expected: SCHEMA_VERSION,
            path: display,
        });
    }
    Ok(parsed)
}

/// Read the credentials file with TOCTOU-safe defenses.
/// Returns `Ok(None)` on `NotFound` so the caller can lift the
/// first-run case into a fresh empty file.
fn read_validated_blocking(path: &Path) -> OAuthResult<Option<Vec<u8>>> {
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
            "credentials file {display} is not a regular file (refusing symlink to special file or directory)",
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(OAuthError::Io(format!(
                "credentials file {} has permissions {:o}; \
                 use chmod 600 to restrict reads to the owner",
                display,
                mode & 0o7777
            )));
        }
    }

    // Cap the read at 1 MiB. Real credentials.json files are
    // bytes-to-kilobytes; a misconfigured or attacker-replaced inode
    // pointing at /dev/zero (via TOCTOU symlink swap that got past the
    // regular-file check) would otherwise drive the loader toward OOM.
    const MAX_CREDENTIALS_BYTES: u64 = 1 << 20;
    let len = meta.len();
    if len > MAX_CREDENTIALS_BYTES {
        return Err(OAuthError::Io(format!(
            "credentials file {display} is {len} bytes; refusing to load (cap is {MAX_CREDENTIALS_BYTES} bytes)",
        )));
    }

    let mut buf = Vec::with_capacity(len as usize);
    (&file)
        .take(MAX_CREDENTIALS_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| OAuthError::Io(format!("read {display}: {e}")))?;
    Ok(Some(buf))
}

/// Save the credentials file atomically. Creates parent directories
/// if missing (mode 0o700 on Unix). Returns the absolute path that was
/// written so callers can include it in success messages.
pub async fn save(path: &Path, file: &CredentialsFile) -> OAuthResult<()> {
    let path_owned = path.to_path_buf();
    let file = file.clone();

    tokio::task::spawn_blocking(move || -> OAuthResult<()> { save_blocking(&path_owned, &file) })
        .await
        .map_err(|e| OAuthError::Internal(format!("save task failed: {e}")))?
}

/// Top-level driver that delegates to the three discrete steps below.
/// Each helper is small enough to fit in a head with the surrounding
/// failure modes; the original single `save_blocking` had grown too
/// dense to read that way.
fn save_blocking(path: &Path, file: &CredentialsFile) -> OAuthResult<()> {
    ensure_dir(path)?;
    let tmp = write_to_temp(path, file)?;
    atomic_rename(tmp, path)
}

/// Step 1: ensure the parent directory exists with `0o700` perms.
/// Idempotent on every save -- cheap and self-healing if the operator
/// chmodded the dir wider by accident.
fn ensure_dir(path: &Path) -> OAuthResult<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| OAuthError::Io(format!("create_dir_all {}: {e}", parent.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort: the file itself still gets 0o600 inside
        // `write_to_temp`, so a stripped dir bit is not a security
        // failure (just a slightly looser umbrella).
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Step 2: serialize the credentials, write to a tempfile in the same
/// directory, and `fsync` it. Returns the `NamedTempFile` so the caller
/// can rename it onto the target path.
fn write_to_temp(path: &Path, file: &CredentialsFile) -> OAuthResult<tempfile::NamedTempFile> {
    use std::io::Write;

    let display = path.display().to_string();
    let parent = path
        .parent()
        .ok_or_else(|| OAuthError::Internal(format!("path has no parent: {display}")))?;

    // Pretty-print for human inspection (`routectl whoami` reads
    // structured; operators occasionally `cat` the file).
    let json = serde_json::to_vec_pretty(file)
        .map_err(|e| OAuthError::Internal(format!("serialize: {e}")))?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".credentials.tmp.")
        .suffix(".json")
        .tempfile_in(parent)
        .map_err(|e| OAuthError::Io(format!("tempfile in {}: {e}", parent.display())))?;

    // On Unix, force 0o600 BEFORE writing so a partial write never has
    // wider permissions than the final.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| OAuthError::Io(format!("chmod tempfile: {e}")))?;
    }

    tmp.write_all(&json)
        .map_err(|e| OAuthError::Io(format!("write tempfile: {e}")))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| OAuthError::Io(format!("fsync tempfile: {e}")))?;

    Ok(tmp)
}

/// Step 3: persist the tempfile onto the final path via `rename`
/// (atomic on POSIX within one filesystem), then re-set 0o600 as
/// defense-in-depth in case a paranoid umask stripped bits.
fn atomic_rename(tmp: tempfile::NamedTempFile, path: &Path) -> OAuthResult<()> {
    let display = path.display().to_string();
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
    use crate::oauth::types::{AccountInfo, SecretToken, TokenRecord};

    fn rec() -> TokenRecord {
        TokenRecord {
            access_token: SecretToken::new("tok"),
            refresh_token: SecretToken::new("rtok"),
            token_type: "Bearer".into(),
            expires_at_unix: 1_900_000_000,
            scopes: vec!["user:inference".into()],
            account: AccountInfo {
                email: Some("u@example.com".into()),
                account_id: None,
            },
            obtained_at_unix: 1_899_000_000,
            session_id: None,
        }
    }

    #[tokio::test]
    async fn load_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let cf = load(&path).await.unwrap();
        assert_eq!(cf.schema_version, SCHEMA_VERSION);
        assert!(cf.providers.is_empty());
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut cf = CredentialsFile::empty();
        cf.upsert("anthropic", rec());
        save(&path, &cf).await.unwrap();

        let loaded = load(&path).await.unwrap();
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(
            loaded.providers["anthropic"].access_token.expose(),
            cf.providers["anthropic"].access_token.expose()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn save_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut cf = CredentialsFile::empty();
        cf.upsert("anthropic", rec());
        save(&path, &cf).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "credentials.json must be 0600, got {:o}",
            mode & 0o777
        );
    }

    #[tokio::test]
    async fn save_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c.json");
        let cf = CredentialsFile::empty();
        save(&nested, &cf).await.unwrap();
        assert!(nested.exists());
    }

    #[tokio::test]
    async fn load_corrupted_returns_corrupted_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        tokio::fs::write(&path, b"not json").await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = load(&path).await.unwrap_err();
        match err {
            OAuthError::CorruptedFile { .. } => {}
            other => panic!("expected CorruptedFile, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_wrong_schema_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        tokio::fs::write(&path, br#"{"schema_version":99,"providers":{}}"#)
            .await
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = load(&path).await.unwrap_err();
        match err {
            OAuthError::SchemaMismatch {
                found, expected, ..
            } => {
                assert_eq!(found, 99);
                assert_eq!(expected, SCHEMA_VERSION);
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn save_overwrites_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        let mut cf = CredentialsFile::empty();
        cf.upsert("anthropic", rec());
        save(&path, &cf).await.unwrap();

        let mut cf2 = CredentialsFile::empty();
        let mut r2 = rec();
        r2.access_token = SecretToken::new("tok-NEW");
        cf2.upsert("anthropic", r2);
        save(&path, &cf2).await.unwrap();

        let loaded = load(&path).await.unwrap();
        assert_eq!(
            loaded.providers["anthropic"].access_token.expose(),
            "tok-NEW"
        );

        // No `.tmp.` files should be left behind.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "atomic write left tempfiles: {leftover:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn load_refuses_world_readable_credentials_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        tokio::fs::write(&path, br#"{"schema_version":1,"providers":{}}"#)
            .await
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = load(&path).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("permissions"), "got: {msg}");
        assert!(msg.contains("chmod 600"), "got: {msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn load_refuses_directory_at_credentials_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::create_dir(&path).unwrap();
        let err = load(&path).await.unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "got: {err}");
    }

    #[test]
    #[serial_test::serial]
    fn default_path_uses_xdg_config_home() {
        // Save & restore to be polite to other tests that rely on env.
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", "/x/y");
        let p = default_path().unwrap();
        assert_eq!(p, PathBuf::from("/x/y/routectl/credentials.json"));
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
