//! Reuse-minimal "value/env -> ref" secret-capture primitive.
//!
//! Turns an operator-supplied secret VALUE into a durable, owner-only
//! `file://` reference, or an environment variable name into an
//! `env://` reference after verifying it currently resolves. The posture
//! is fail-closed: a capture either lands a correctly-permissioned file
//! whose bytes were read back and confirmed, or it errors -- it never
//! silently falls back to a weaker source and never widens directory or
//! file permissions.
//!
//! This module is UNCONDITIONAL (not behind the `oauth` feature): the
//! lean build captures `env://` / `file://` secrets too. Policy about
//! WHICH source to prefer (prompt vs. env auto-detect) lives in the CLI,
//! not here.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::secret_ref::SecretRef;

/// Errors from the secret-capture primitive. Mapped to
/// [`routectl_core::Error::Auth`] at the CLI boundary via the `From`
/// impl below; exposed here so a caller can dispatch on cause.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SecretCaptureError {
    #[error("could not resolve the default secret directory: {0}")]
    DirResolution(String),

    #[error("secret store path {path} exists but is not a directory")]
    NotADirectory { path: String },

    #[error(
        "secret store directory {path} is a symlink; a symlinked store can \
         redirect where secrets are written or read -- point the store at a \
         real directory"
    )]
    SymlinkedStore { path: String },

    #[error(
        "secret store directory {path} has permissions {mode:o}; it must be \
         owner-only (0700) so other users cannot read stored secrets"
    )]
    WrongPermissions { path: String, mode: u32 },

    #[error("secret store directory {path} is not usable: {detail}")]
    Unusable { path: String, detail: String },

    #[error("failed to write secret `{name}`: {detail}")]
    Write { name: String, detail: String },

    #[error("post-write verification failed for secret `{name}`; the file was removed")]
    VerificationFailed { name: String },

    #[error("environment variable `{var}` is not set")]
    EnvNotSet { var: String },

    #[error("environment variable `{var}` is set but empty")]
    EnvEmpty { var: String },

    #[error("environment variable `{var}` contains invalid unicode")]
    EnvNotUnicode { var: String },
}

impl From<SecretCaptureError> for routectl_core::Error {
    fn from(e: SecretCaptureError) -> Self {
        Self::Auth(e.to_string())
    }
}

/// `SecretCaptureError`-flavored result alias for the module's public API.
pub type SecretCaptureResult<T> = std::result::Result<T, SecretCaptureError>;

/// Default secret directory: `$XDG_CONFIG_HOME/routectl/secrets`, falling
/// back to `~/.config/routectl/secrets`. Sibling of `credentials.json`.
pub fn default_secret_dir() -> SecretCaptureResult<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("routectl").join("secrets"));
    }
    let home = std::env::var("HOME").map_err(|_| {
        SecretCaptureError::DirResolution("neither XDG_CONFIG_HOME nor HOME is set".into())
    })?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("routectl")
        .join("secrets"))
}

/// A routectl-managed directory of owner-only secret files.
///
/// Secret names are case-preserved, so two names differing only in case
/// map to distinct files on a case-sensitive filesystem but collide on a
/// case-insensitive one (e.g. default macOS/Windows volumes).
#[derive(Debug)]
pub struct ManagedSecretStore {
    dir: PathBuf,
}

impl ManagedSecretStore {
    /// Open the store at `dir`, creating it `0700` if absent. Hard-errors
    /// on a non-directory path, a directory readable by group/other, or a
    /// directory that cannot be created or written -- it never downgrades
    /// the security posture to accommodate a bad directory.
    ///
    /// `dir` is made absolute lexically, then -- once it exists -- the leaf
    /// is rejected if it is a symlink and the path is canonicalized so all
    /// symlinked ancestors are resolved. Canonicalizing ONCE here pins
    /// `ref_path`, `put`, and the post-write verification to the same real
    /// directory: a symlinked ancestor swapped between the pre-confirm ref
    /// computation and the post-confirm `put` cannot redirect where a
    /// managed secret is written or read.
    pub fn open(dir: PathBuf) -> SecretCaptureResult<Self> {
        let dir = to_absolute(dir)?;
        match std::fs::metadata(&dir) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return Err(SecretCaptureError::NotADirectory {
                        path: dir.display().to_string(),
                    });
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = meta.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        return Err(SecretCaptureError::WrongPermissions {
                            path: dir.display().to_string(),
                            mode,
                        });
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&dir).map_err(|e| SecretCaptureError::Unusable {
                    path: dir.display().to_string(),
                    detail: format!("create directory: {e}"),
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                        .map_err(|e| SecretCaptureError::Unusable {
                            path: dir.display().to_string(),
                            detail: format!("set directory permissions: {e}"),
                        })?;
                }
            }
            Err(e) => {
                return Err(SecretCaptureError::Unusable {
                    path: dir.display().to_string(),
                    detail: format!("inspect directory: {e}"),
                });
            }
        }
        let dir = canonicalize_store_dir(dir)?;
        probe_writable(&dir)?;
        Ok(Self { dir })
    }

    /// Deterministic, I/O-free path for the secret named `name`. The name
    /// is sanitized so the result is always a single component directly
    /// under the store directory; a traversal-shaped name cannot escape.
    pub fn ref_path(&self, name: &str) -> PathBuf {
        self.dir.join(sanitize_name(name))
    }

    /// Capture `value` as an owner-only (`0600`) file and return a
    /// `file://` reference to it. Writes the raw bytes (the resolver trims
    /// on read), then re-reads and byte-compares as a post-write check; on
    /// mismatch the file is removed and an error returned. Never falls
    /// back to another source.
    pub fn put(&self, name: &str, value: &str) -> SecretCaptureResult<SecretRef> {
        let path = self.ref_path(name);
        crate::atomic_write::write_0600_atomic(&path, value.as_bytes()).map_err(|detail| {
            SecretCaptureError::Write {
                name: name.to_string(),
                detail,
            }
        })?;
        let written = std::fs::read(&path).map_err(|e| SecretCaptureError::Write {
            name: name.to_string(),
            detail: format!("verification read: {e}"),
        })?;
        if written != value.as_bytes() {
            let _ = std::fs::remove_file(&path);
            return Err(SecretCaptureError::VerificationFailed {
                name: name.to_string(),
            });
        }
        Ok(SecretRef::File(path))
    }
}

/// Verify `var` resolves to a non-empty value NOW and return an `env://`
/// reference. Stores nothing on disk.
pub fn env_ref(var: &str) -> SecretCaptureResult<SecretRef> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(SecretRef::Env(var.to_string())),
        Ok(_) => Err(SecretCaptureError::EnvEmpty {
            var: var.to_string(),
        }),
        Err(std::env::VarError::NotUnicode(_)) => Err(SecretCaptureError::EnvNotUnicode {
            var: var.to_string(),
        }),
        Err(std::env::VarError::NotPresent) => Err(SecretCaptureError::EnvNotSet {
            var: var.to_string(),
        }),
    }
}

/// Sentinel filename for an empty `name`. A lone `%` can never be produced
/// by [`sanitize_name`] (which only ever emits `%` as the first byte of a
/// three-char `%XX` escape), so it collides with no non-empty name.
const EMPTY_NAME_PLACEHOLDER: &str = "%";

/// Percent-encode `name` into a single filesystem-safe filename component.
/// Every byte outside `[A-Za-z0-9_-]` (including `.`, `/`, and `%` itself)
/// becomes `%XX`, so the result never contains a path separator, is never
/// `.` or `..`, and is injective across distinct names.
fn sanitize_name(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(name.len());
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    if out.is_empty() {
        out.push_str(EMPTY_NAME_PLACEHOLDER);
    }
    out
}

/// Make `path` absolute without touching the filesystem or resolving
/// symlinks (pure lexical join with the current directory when relative).
fn to_absolute(path: PathBuf) -> SecretCaptureResult<PathBuf> {
    std::path::absolute(&path).map_err(|e| SecretCaptureError::Unusable {
        path: path.display().to_string(),
        detail: format!("resolve absolute path: {e}"),
    })
}

/// Reject a symlinked leaf directory and return the fully-resolved
/// (symlink-free) canonical path. Resolving all symlinks ONCE at open time
/// is what makes the store TOCTOU-safe: every later `ref_path` / `put` /
/// verification uses this real path, so neither the leaf nor a swapped
/// symlinked ancestor can redirect where a managed secret lands.
fn canonicalize_store_dir(dir: PathBuf) -> SecretCaptureResult<PathBuf> {
    let sym_meta = std::fs::symlink_metadata(&dir).map_err(|e| SecretCaptureError::Unusable {
        path: dir.display().to_string(),
        detail: format!("inspect directory: {e}"),
    })?;
    if sym_meta.file_type().is_symlink() {
        return Err(SecretCaptureError::SymlinkedStore {
            path: dir.display().to_string(),
        });
    }
    std::fs::canonicalize(&dir).map_err(|e| SecretCaptureError::Unusable {
        path: dir.display().to_string(),
        detail: format!("canonicalize directory: {e}"),
    })
}

/// Confirm the process can create a file in `dir` by creating and then
/// dropping a temporary probe file. Catches an existing-but-unwritable
/// directory (read-only mount, restrictive mode, ACL) that the mode check
/// alone would pass.
fn probe_writable(dir: &Path) -> SecretCaptureResult<()> {
    tempfile::Builder::new()
        .prefix(".probe.")
        .tempfile_in(dir)
        .map(|_| ())
        .map_err(|e| SecretCaptureError::Unusable {
            path: dir.display().to_string(),
            detail: format!("directory is not writable: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn has_temp_residue(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.contains(".tmp.") || n.contains(".probe.")
            })
    }

    fn set_env(key: &str, val: &str) {
        // SAFETY: env-touching tests are serialized via serial_test, so no
        // other thread reads or writes the process environment concurrently.
        unsafe { std::env::set_var(key, val) };
    }

    fn unset_env(key: &str) {
        // SAFETY: see set_env.
        unsafe { std::env::remove_var(key) };
    }

    fn restore_env(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => set_env(key, &v),
            None => unset_env(key),
        }
    }

    #[test]
    fn put_writes_file_with_exact_bytes_and_returns_file_ref() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let store = ManagedSecretStore::open(tmp.path().join("secrets")).unwrap();

        // Act
        let sref = store.put("api-key", "s3cr3t-value").unwrap();

        // Assert
        match &sref {
            SecretRef::File(path) => {
                assert!(path.is_absolute(), "ref path must be absolute: {path:?}");
                assert_eq!(std::fs::read(path).unwrap(), b"s3cr3t-value");
                assert_eq!(path, &store.ref_path("api-key"));
            }
            other => panic!("expected SecretRef::File, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn put_writes_0600_file() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let store = ManagedSecretStore::open(tmp.path().join("secrets")).unwrap();

        // Act
        let sref = store.put("k", "v").unwrap();

        // Assert
        let SecretRef::File(path) = sref else {
            panic!("expected File ref");
        };
        assert_eq!(mode_of(&path), 0o600, "captured secret must be 0600");
    }

    #[test]
    fn put_leaves_no_temp_residue() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let store = ManagedSecretStore::open(dir.clone()).unwrap();

        // Act
        store.put("k", "v").unwrap();

        // Assert
        assert!(!has_temp_residue(&dir), "capture left temp/probe residue");
    }

    #[test]
    fn put_overwrites_existing_secret() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let store = ManagedSecretStore::open(tmp.path().join("secrets")).unwrap();

        // Act
        store.put("k", "first").unwrap();
        let sref = store.put("k", "second").unwrap();

        // Assert
        let SecretRef::File(path) = sref else {
            panic!("expected File ref");
        };
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }

    #[test]
    fn ref_path_is_deterministic_and_writes_nothing() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let store = ManagedSecretStore::open(dir.clone()).unwrap();

        // Act
        let a = store.ref_path("anthropic");
        let b = store.ref_path("anthropic");

        // Assert
        assert_eq!(a, b, "ref_path must be deterministic");
        assert!(!a.exists(), "ref_path must perform no I/O");
    }

    #[test]
    fn ref_path_rejects_parent_traversal() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let store = ManagedSecretStore::open(dir.clone()).unwrap();
        let expected_parent = std::fs::canonicalize(&dir).unwrap();

        // Act
        let rp = store.ref_path("../../../etc/passwd");

        // Assert
        assert_eq!(
            rp.parent(),
            Some(expected_parent.as_path()),
            "traversal name must stay directly under the store dir: {rp:?}"
        );
        assert!(rp.starts_with(&expected_parent));
    }

    #[test]
    fn ref_path_flattens_nested_separators() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let store = ManagedSecretStore::open(dir.clone()).unwrap();
        let expected_parent = std::fs::canonicalize(&dir).unwrap();

        // Act
        let rp = store.ref_path("a/b");

        // Assert
        assert_eq!(rp.parent(), Some(expected_parent.as_path()));
    }

    #[test]
    fn ref_path_handles_empty_name_without_escaping() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let store = ManagedSecretStore::open(dir.clone()).unwrap();
        let expected_parent = std::fs::canonicalize(&dir).unwrap();

        // Act
        let rp = store.ref_path("");

        // Assert
        assert_eq!(rp.parent(), Some(expected_parent.as_path()));
        assert_ne!(
            rp, expected_parent,
            "empty name must not resolve to the dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_a_symlinked_store_directory() {
        // Arrange: a real owner-only target directory and a symlink pointing
        // at it. Opening the store through the symlink must be refused so a
        // swapped link cannot redirect where secrets are written or read.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real-secrets");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = tmp.path().join("linked-secrets");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Act
        let err = ManagedSecretStore::open(link).unwrap_err();

        // Assert
        assert!(
            matches!(err, SecretCaptureError::SymlinkedStore { .. }),
            "expected SymlinkedStore, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_creates_missing_directory_as_0700() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fresh");
        assert!(!dir.exists());

        // Act
        let _store = ManagedSecretStore::open(dir.clone()).unwrap();

        // Assert
        assert!(dir.is_dir());
        assert_eq!(mode_of(&dir), 0o700, "fresh store dir must be 0700");
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_group_or_world_accessible_directory() {
        // Arrange
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("wide");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Act
        let err = ManagedSecretStore::open(dir).unwrap_err();

        // Assert
        assert!(
            matches!(err, SecretCaptureError::WrongPermissions { .. }),
            "expected WrongPermissions, got {err:?}"
        );
    }

    #[test]
    fn open_hard_errors_when_directory_cannot_be_created() {
        // Arrange: the parent of the target is a regular FILE, so the
        // directory can neither be inspected nor created.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let dir = blocker.join("secrets");

        // Act
        let err = ManagedSecretStore::open(dir).unwrap_err();

        // Assert
        assert!(
            matches!(err, SecretCaptureError::Unusable { .. }),
            "expected Unusable, got {err:?}"
        );
    }

    #[test]
    fn open_rejects_non_directory_path() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("afile");
        std::fs::write(&path, b"x").unwrap();

        // Act
        let err = ManagedSecretStore::open(path).unwrap_err();

        // Assert
        assert!(
            matches!(err, SecretCaptureError::NotADirectory { .. }),
            "expected NotADirectory, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_hard_errors_on_existing_unwritable_directory() {
        // Arrange
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("locked");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        // If this environment can still write into a 0500 dir (e.g. running
        // as root), the writability probe cannot fire -- skip rather than
        // assert a guarantee the environment does not provide.
        if std::fs::File::create(dir.join(".can_write")).is_ok() {
            let _ = std::fs::remove_file(dir.join(".can_write"));
            return;
        }

        // Act
        let err = ManagedSecretStore::open(dir).unwrap_err();

        // Assert
        assert!(
            matches!(err, SecretCaptureError::Unusable { .. }),
            "expected Unusable, got {err:?}"
        );
    }

    #[test]
    fn open_accepts_existing_owner_only_directory() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ok");
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        // Act
        let store = ManagedSecretStore::open(dir).unwrap();

        // Assert
        assert!(store.put("k", "v").is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn env_ref_returns_env_for_non_empty_var() {
        // Arrange
        let key = "ROUTECTL_SECRET_CAPTURE_SET_VAR";
        let prev = std::env::var(key).ok();
        set_env(key, "present");

        // Act
        let sref = env_ref(key);

        // Assert
        assert_eq!(sref.unwrap(), SecretRef::Env(key.to_string()));
        restore_env(key, prev);
    }

    #[test]
    #[serial_test::serial]
    fn env_ref_errors_for_unset_var() {
        // Arrange
        let key = "ROUTECTL_SECRET_CAPTURE_UNSET_VAR";
        let prev = std::env::var(key).ok();
        unset_env(key);

        // Act
        let err = env_ref(key).unwrap_err();

        // Assert
        assert!(
            matches!(err, SecretCaptureError::EnvNotSet { .. }),
            "expected EnvNotSet, got {err:?}"
        );
        restore_env(key, prev);
    }

    #[test]
    #[serial_test::serial]
    fn env_ref_errors_for_empty_var() {
        // Arrange
        let key = "ROUTECTL_SECRET_CAPTURE_EMPTY_VAR";
        let prev = std::env::var(key).ok();
        set_env(key, "");

        // Act
        let err = env_ref(key).unwrap_err();

        // Assert
        assert!(
            matches!(err, SecretCaptureError::EnvEmpty { .. }),
            "expected EnvEmpty, got {err:?}"
        );
        restore_env(key, prev);
    }

    #[test]
    #[serial_test::serial]
    fn default_secret_dir_uses_xdg_config_home() {
        // Arrange
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        set_env("XDG_CONFIG_HOME", "/x/y");

        // Act
        let dir = default_secret_dir().unwrap();

        // Assert
        assert_eq!(dir, PathBuf::from("/x/y/routectl/secrets"));
        restore_env("XDG_CONFIG_HOME", prev);
    }

    #[test]
    #[serial_test::serial]
    fn default_secret_dir_falls_back_to_home_config() {
        // Arrange
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        unset_env("XDG_CONFIG_HOME");
        set_env("HOME", "/home/tester");

        // Act
        let dir = default_secret_dir().unwrap();

        // Assert
        assert_eq!(dir, PathBuf::from("/home/tester/.config/routectl/secrets"));
        restore_env("XDG_CONFIG_HOME", prev_xdg);
        restore_env("HOME", prev_home);
    }
}
