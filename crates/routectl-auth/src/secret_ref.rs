use std::fmt;
use routectl_core::{Error, Result};

/// A reference to a secret, resolved at use-time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretRef {
    /// `keychain://<service>/<account>` -> OS keychain entry.
    Keychain { service: String, account: String },
    /// `env://VAR_NAME` -> process env var.
    Env(String),
    /// Literal value. Strongly discouraged outside ephemeral test configs.
    Literal(String),
}

impl SecretRef {
    pub fn parse(uri: &str) -> Result<Self> {
        if let Some(rest) = uri.strip_prefix("keychain://") {
            let (service, account) = rest
                .split_once('/')
                .ok_or_else(|| Error::Auth(format!("invalid keychain URI: {uri}")))?;
            return Ok(Self::Keychain {
                service: service.to_string(),
                account: account.to_string(),
            });
        }
        if let Some(var) = uri.strip_prefix("env://") {
            return Ok(Self::Env(var.to_string()));
        }
        if let Some(lit) = uri.strip_prefix("literal:") {
            return Ok(Self::Literal(lit.to_string()));
        }
        Err(Error::Auth(format!(
            "unrecognized secret URI scheme: {uri}"
        )))
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Keychain { service, account } => {
                write!(f, "keychain://{service}/{account}")
            }
            SecretRef::Env(var) => write!(f, "env://{var}"),
            SecretRef::Literal(val) => write!(f, "literal:{val}"),
        }
    }
}
