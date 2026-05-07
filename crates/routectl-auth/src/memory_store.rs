//! Default `SecretStore` impl. Resolves `env://`, `file://`, and
//! `literal:` secret references at read-time. There is no in-memory
//! key/value back end any more -- the name is preserved for
//! backwards-compat with existing callers and tests.
//!
//! Routectl never auto-discovers credentials from third-party tools;
//! every secret source is explicit-by-config in the user's TOML.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use async_trait::async_trait;
use routectl_core::{Error, Result};

use crate::{SecretRef, SecretStore};

pub struct MemoryStore;

impl MemoryStore {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretStore for MemoryStore {
    async fn get(&self, secret_ref: &SecretRef) -> Result<String> {
        match secret_ref {
            SecretRef::Env(var) => std::env::var(var)
                .map_err(|_| Error::Auth(format!("env var {var} not set"))),
            SecretRef::Literal(s) => Ok(s.clone()),
            SecretRef::File(path) => read_secret_file(path).await,
        }
    }

    async fn set(&self, _secret_ref: &SecretRef, _value: &str) -> Result<()> {
        // All three sources are read-only via routectl. Users manage
        // env vars, files, and inline literals through their own
        // tooling -- routectl just resolves them at request time.
        Err(Error::Auth(
            "secrets are read-only via routectl; manage env vars / files outside the binary"
                .into(),
        ))
    }

    async fn delete(&self, _secret_ref: &SecretRef) -> Result<()> {
        Err(Error::Auth(
            "secrets are read-only via routectl; manage env vars / files outside the binary"
                .into(),
        ))
    }
}

/// Read a secret from a file. Trims trailing whitespace (handles the
/// common case where the file has a trailing newline). On Unix, refuses
/// world- or group-readable files to avoid silently using a leaked
/// secret -- mode 600 / 400 is the recommended pattern.
async fn read_secret_file(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path).await.map_err(|e| {
        Error::Auth(format!(
            "failed to read secret file `{}`: {e}",
            path.display()
        ))
    })?;

    #[cfg(unix)]
    {
        let meta = tokio::fs::metadata(path).await.map_err(|e| {
            Error::Auth(format!(
                "failed to stat secret file `{}`: {e}",
                path.display()
            ))
        })?;
        let mode = meta.permissions().mode();
        // Bits 0o077 cover group + other read/write/execute. If any
        // are set the file is too permissive for a secret.
        if mode & 0o077 != 0 {
            return Err(Error::Auth(format!(
                "secret file `{}` has permissions {:o}; use chmod 600 or 400 to restrict reads to the owner",
                path.display(),
                mode & 0o7777
            )));
        }
    }

    let s = String::from_utf8(bytes).map_err(|e| {
        Error::Auth(format!(
            "secret file `{}` is not valid UTF-8: {e}",
            path.display()
        ))
    })?;
    Ok(s.trim_end().to_string())
}
