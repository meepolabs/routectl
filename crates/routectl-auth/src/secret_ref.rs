use std::fmt;
use std::path::PathBuf;

use routectl_core::{Error, Result};

/// A reference to a secret, resolved at use-time. Three sources, all
/// explicit-by-config -- the user picks per provider in TOML by writing
/// the appropriate URI scheme. Routectl never auto-discovers credentials.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretRef {
    /// `env://VAR_NAME` -> process env var. Silent on every platform;
    /// the value is visible to anything that can read this process's
    /// environment (e.g. `/proc/<pid>/environ`).
    Env(String),
    /// `file:///absolute/path/to/key` -> file contents (trimmed). The
    /// file's permissions should restrict reads to the current user
    /// (mode 600 / 400). Compatible with sops, age, doppler-cli,
    /// vault-agent, or any tool that drops a token into a file.
    File(PathBuf),
    /// `literal:hunter2` -> inline plaintext. Useful in tests and
    /// for placeholders like `literal:not-needed` (llama.cpp). Avoid
    /// for real secrets in version-controlled config.
    Literal(String),
}

impl SecretRef {
    pub fn parse(uri: &str) -> Result<Self> {
        if let Some(var) = uri.strip_prefix("env://") {
            if var.is_empty() {
                return Err(Error::Auth("env:// URI missing variable name".into()));
            }
            return Ok(Self::Env(var.to_string()));
        }
        if let Some(rest) = uri.strip_prefix("file://") {
            if rest.is_empty() {
                return Err(Error::Auth("file:// URI missing path".into()));
            }
            let path = PathBuf::from(rest);
            if !path.is_absolute() {
                return Err(Error::Auth(format!(
                    "file:// URI must be an absolute path (got `{rest}`); use `file:///abs/path/to/key`"
                )));
            }
            return Ok(Self::File(path));
        }
        if let Some(lit) = uri.strip_prefix("literal:") {
            return Ok(Self::Literal(lit.to_string()));
        }
        Err(Error::Auth(format!(
            "unrecognized secret URI scheme: {uri} (expected env://, file://, or literal:)"
        )))
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Env(var) => write!(f, "env://{var}"),
            SecretRef::File(path) => write!(f, "file://{}", path.display()),
            SecretRef::Literal(val) => write!(f, "literal:{val}"),
        }
    }
}
