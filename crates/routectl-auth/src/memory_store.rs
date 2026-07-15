//! Default `SecretStore` impl. Resolves `env://` and `file://` secret
//! references at read-time. `literal:` refs are rejected here as well as
//! at parse -- defense in depth for a programmatically-constructed
//! `SecretRef::Literal`, which can never come from a URI. There is no
//! in-memory key/value back end any more -- the name is preserved for
//! backwards-compat with existing callers and tests.
//!
//! Routectl never auto-discovers credentials from third-party tools;
//! every secret source is explicit-by-config in the user's TOML.

use std::path::Path;

use async_trait::async_trait;
use routectl_core::{Error, Result};

use crate::{SecretRef, SecretStore};

#[derive(Clone, Default)]
pub struct MemoryStore;

impl MemoryStore {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SecretStore for MemoryStore {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String> {
        match secret_ref {
            SecretRef::Env(var) => std::env::var(var).map_err(|_| {
                tracing::warn!(
                    scheme = "env://",
                    var = %var,
                    reason = "not set",
                    "secret resolution failed",
                );
                Error::Auth(format!("env var {var} not set"))
            }),
            SecretRef::Literal(_) => Err(crate::secret_ref::literal_rejected()),
            SecretRef::File(path) => read_secret_file(path).await,
            SecretRef::OAuth { provider, .. } => {
                // MemoryStore does not own the OAuth store. The
                // composite store in routectl-cli (CompositeStore)
                // routes oauth:// refs to OAuthStore before they reach
                // here. If a caller wires only a MemoryStore (e.g.
                // tests, downstream embedders), oauth:// is a hard
                // error -- not a silent miss.
                tracing::warn!(
                    scheme = "oauth://",
                    provider = %provider,
                    reason = "MemoryStore does not handle oauth:// refs",
                    "secret resolution failed",
                );
                Err(Error::Auth(format!(
                    "oauth://{provider} cannot be resolved by MemoryStore; \
                     wire a CompositeStore that includes OAuthStore (or run \
                     `routectl login {provider}` from the CLI binary)",
                )))
            }
        }
    }

    async fn set(&self, _secret_ref: &SecretRef, _value: &str) -> Result<()> {
        // All sources are read-only via routectl. Users manage env vars,
        // files, and OAuth tokens through their own tooling -- routectl
        // just resolves them at request time (OAuth tokens flow through
        // `routectl login`, not `set`).
        Err(Error::Auth(
            "secrets are read-only via routectl; manage env vars / files outside the binary".into(),
        ))
    }

    async fn delete(&self, _secret_ref: &SecretRef) -> Result<()> {
        Err(Error::Auth(
            "secrets are read-only via routectl; manage env vars / files outside the binary".into(),
        ))
    }
}

/// Read a secret from a file. Trims trailing whitespace (handles the
/// common case where the file has a trailing newline).
///
/// TOCTOU-safe on Unix: opens the file ONCE, then `fstat`s the open
/// file descriptor (not the path) before reading bytes. This means
/// the permission check, the symlink/special-file check, and the read
/// all observe the same inode -- a `mv` or symlink swap between the
/// open and the stat cannot trick the check.
///
/// Refuses files that are not regular files (catches symlinks pointing
/// at devices/fifos/etc.), and refuses any file with `0o077`-readable
/// permissions (group or other has any bit set). `chmod 600` or `400`
/// is the recommended pattern.
async fn read_secret_file(path: &Path) -> Result<String> {
    let path_owned = path.to_path_buf();
    let display = path_owned.display().to_string();

    tokio::task::spawn_blocking(move || -> Result<String> {
        use std::io::Read;

        // Defense in depth: `SecretRef::parse()` rejects relative
        // `file://` URIs, but `SecretRef::File` is publicly
        // constructible -- programmatic callers (tests, downstream
        // crates) could pass a relative path that would otherwise be
        // resolved against the process CWD. Refuse here so the
        // "absolute path" invariant holds at every entry point.
        if !path_owned.is_absolute() {
            tracing::warn!(
                scheme = "file://",
                path = %path_owned.display(),
                reason = "relative path",
                "secret resolution failed",
            );
            return Err(Error::Auth(format!(
                "secret file `{}` must be an absolute path; relative paths are refused (CWD-relative resolution is unsafe)",
                path_owned.display()
            )));
        }

        let file = std::fs::File::open(&path_owned).map_err(|e| {
            tracing::warn!(
                scheme = "file://",
                path = %path_owned.display(),
                reason = "open failed",
                error = %e,
                "secret resolution failed",
            );
            Error::Auth(format!(
                "failed to open secret file `{}`: {e}",
                path_owned.display()
            ))
        })?;

        let meta = file.metadata().map_err(|e| {
            tracing::warn!(
                scheme = "file://",
                path = %path_owned.display(),
                reason = "stat failed",
                error = %e,
                "secret resolution failed",
            );
            Error::Auth(format!(
                "failed to stat secret file `{}`: {e}",
                path_owned.display()
            ))
        })?;

        if !meta.is_file() {
            tracing::warn!(
                scheme = "file://",
                path = %path_owned.display(),
                reason = "not a regular file",
                "secret resolution failed",
            );
            return Err(Error::Auth(format!(
                "secret file `{}` is not a regular file (refusing symlink to special file or directory)",
                path_owned.display()
            )));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    scheme = "file://",
                    path = %path_owned.display(),
                    mode = format!("{:o}", mode & 0o7777),
                    reason = "group/other readable; chmod 600 or 400",
                    "secret resolution failed",
                );
                return Err(Error::Auth(format!(
                    "secret file `{}` has permissions {:o}; use chmod 600 or 400 to restrict reads to the owner",
                    path_owned.display(),
                    mode & 0o7777
                )));
            }
        }

        // Cap the read at 1 MiB. Real secrets are bytes-to-kilobytes; a
        // misconfigured `file://` URI pointing at /var/log/syslog (or worse,
        // /dev/zero through a symlink that got past the regular-file check
        // via TOCTOU) would otherwise drive the process toward OOM.
        const MAX_SECRET_FILE_BYTES: u64 = 1 << 20;
        let len = meta.len();
        if len > MAX_SECRET_FILE_BYTES {
            return Err(Error::Auth(format!(
                "secret file `{}` is {} bytes; refusing to load (cap is {} bytes)",
                path_owned.display(),
                len,
                MAX_SECRET_FILE_BYTES
            )));
        }

        let mut buf = Vec::with_capacity(len as usize);
        file.take(MAX_SECRET_FILE_BYTES).read_to_end(&mut buf).map_err(|e| {
            Error::Auth(format!(
                "failed to read secret file `{}`: {e}",
                path_owned.display()
            ))
        })?;

        let s = String::from_utf8(buf).map_err(|e| {
            tracing::warn!(
                scheme = "file://",
                path = %path_owned.display(),
                reason = "not utf8",
                "secret resolution failed",
            );
            Error::Auth(format!(
                "secret file `{}` is not valid UTF-8: {e}",
                path_owned.display()
            ))
        })?;
        Ok(s.trim_end().to_string())
    })
    .await
    .map_err(|e| {
        let kind = if e.is_panic() { "panicked" } else { "was cancelled" };
        Error::Auth(format!("secret file read for `{display}` {kind}: {e}"))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_list_seats_returns_single_ref() {
        // MemoryStore inherits the trait's default `list_seats`: any ref
        // is a single credential, so enumeration echoes the input ref.
        let store = MemoryStore::new();
        for sr in [
            SecretRef::Env("FOO".into()),
            SecretRef::Literal("bar".into()),
            SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            },
        ] {
            let seats = store.list_seats(&sr).await.unwrap();
            assert_eq!(seats, vec![sr]);
        }
    }
}
