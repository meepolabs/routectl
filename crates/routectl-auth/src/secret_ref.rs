use std::fmt;
use std::path::PathBuf;

use routectl_core::{Error, Result};

/// A reference to a secret, resolved at use-time. Four sources, all
/// explicit-by-config -- the user picks per provider in TOML by writing
/// the appropriate URI scheme. Routectl never auto-discovers credentials.
#[derive(Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
    /// `oauth://<provider>` -> token resolved from the routectl-managed
    /// OAuth credentials store. Provider must have been logged in via
    /// `routectl login <provider>` first. Resolution reads the token
    /// from `~/.config/routectl/credentials.json`; refresh and 401-retry
    /// are handled transparently by `OAuthStore`.
    OAuth { provider: String },
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
        if let Some(prov) = uri.strip_prefix("oauth://") {
            if prov.is_empty() {
                return Err(Error::Auth("oauth:// URI missing provider name".into()));
            }
            // Provider name validation is deferred to the OAuth store at
            // use-time -- mirroring env:// where existence of the var is
            // also a use-time check, not a parse-time one.
            return Ok(Self::OAuth {
                provider: prov.to_string(),
            });
        }
        // The fallthrough must never echo the raw `uri`: a bare,
        // unprefixed value (e.g. an API key pasted without a scheme) IS
        // secret material, and this error can reach operator-facing
        // stdout via `config check` -> shell history / CI logs. Report
        // only the scheme-shaped prefix (validated as an RFC 3986
        // scheme), or a generic message when none exists -- never the
        // value itself. The expected-scheme list stays for the operator.
        match scheme_token(uri) {
            Some(scheme) => Err(Error::Auth(format!(
                "unrecognized secret URI scheme `{scheme}` (expected env://, file://, literal:, or oauth://)"
            ))),
            None => Err(Error::Auth(
                "unrecognized secret URI scheme: no recognized scheme prefix \
                 (expected env://, file://, literal:, or oauth://)"
                    .into(),
            )),
        }
    }
}

/// Extract the scheme-shaped prefix of `uri` -- the text before the
/// first `:` -- but only when it is a syntactically valid URI scheme per
/// RFC 3986: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`. Returns `None`
/// for a bare value with no `:` delimiter, or one whose prefix is not
/// scheme-shaped. This guarantees secret material (which is not
/// scheme-shaped) is never echoed back to the caller in an error.
fn scheme_token(uri: &str) -> Option<&str> {
    let (scheme, _) = uri.split_once(':')?;
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        Some(scheme)
    } else {
        None
    }
}

impl fmt::Debug for SecretRef {
    /// Hand-rolled to redact the `Literal(_)` arm. The other arms are
    /// pointers (the secret is what they reference, not the URI), so
    /// they keep the derived-style `Variant(field)` shape. The
    /// `Literal` arm delegates to `Display`, which already redacts
    /// to `literal:[REDACTED]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Env(v) => f.debug_tuple("Env").field(v).finish(),
            SecretRef::File(p) => f.debug_tuple("File").field(p).finish(),
            SecretRef::OAuth { provider } => {
                f.debug_struct("OAuth").field("provider", provider).finish()
            }
            SecretRef::Literal(_) => write!(f, "{self}"),
        }
    }
}

impl fmt::Display for SecretRef {
    /// Renders a SecretRef without revealing the underlying secret
    /// value. The `env://`, `file://`, and `oauth://` arms are pointers
    /// (the referenced material is the secret, not the URI), so they
    /// round-trip safely. The `literal:` arm IS the secret material
    /// in-line, so we redact it here -- any caller that `format!`s
    /// or logs a SecretRef would otherwise leak the inline value.
    /// Resolution still happens normally via `SecretStore::get`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Env(var) => write!(f, "env://{var}"),
            SecretRef::File(path) => write!(f, "file://{}", path.display()),
            SecretRef::Literal(val) if val.is_empty() => write!(f, "literal:"),
            SecretRef::Literal(_) => write!(f, "literal:[REDACTED]"),
            SecretRef::OAuth { provider } => write!(f, "oauth://{provider}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oauth_provider() {
        let sr = SecretRef::parse("oauth://anthropic").unwrap();
        assert_eq!(
            sr,
            SecretRef::OAuth {
                provider: "anthropic".into()
            }
        );
    }

    #[test]
    fn parses_oauth_codex() {
        let sr = SecretRef::parse("oauth://codex").unwrap();
        assert_eq!(
            sr,
            SecretRef::OAuth {
                provider: "codex".into()
            }
        );
    }

    #[test]
    fn rejects_oauth_empty_provider() {
        let err = SecretRef::parse("oauth://").unwrap_err();
        assert!(err.to_string().contains("missing provider"));
    }

    #[test]
    fn parses_oauth_unknown_provider_at_parse_time() {
        // Parse accepts unknown provider names; OAuthStore rejects them
        // at use-time with a more helpful "known providers: ..." error.
        let sr = SecretRef::parse("oauth://made-up").unwrap();
        assert_eq!(
            sr,
            SecretRef::OAuth {
                provider: "made-up".into()
            }
        );
    }

    #[test]
    fn display_oauth_round_trips() {
        let sr = SecretRef::OAuth {
            provider: "anthropic".into(),
        };
        assert_eq!(format!("{sr}"), "oauth://anthropic");
        let parsed = SecretRef::parse(&format!("{sr}")).unwrap();
        assert_eq!(parsed, sr);
    }

    #[test]
    fn literal_debug_does_not_contain_secret() {
        let s = SecretRef::Literal("hunter2".into());
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains("hunter2"),
            "Debug must not leak secret: {dbg}"
        );
        assert!(
            dbg.contains("[REDACTED]"),
            "Debug must show redacted marker: {dbg}"
        );
    }

    #[test]
    fn unknown_scheme_error_lists_oauth() {
        let err = SecretRef::parse("vault://secret").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("oauth://"),
            "error message must mention oauth: {msg}"
        );
    }

    #[test]
    fn bare_value_error_does_not_leak_secret() {
        // A bare, unprefixed value is itself secret material. The
        // unrecognized-scheme error must not echo it -- it reaches
        // operator-facing stdout via `config check`. Uses an obvious
        // fake value, never a real key.
        let fake = "sk-ant-supersecret-value";
        let err = SecretRef::parse(fake).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("supersecret"),
            "error must not leak secret material: {msg}"
        );
        assert!(
            !msg.contains(fake),
            "error must not echo the raw value: {msg}"
        );
        assert!(
            msg.contains("oauth://"),
            "error should still list expected schemes: {msg}"
        );
    }
}
