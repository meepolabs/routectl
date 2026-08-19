//! OAuth-managed credentials for routectl.
//!
//! This module implements the routectl-side of an OAuth 2.0 PKCE flow
//! (RFC 7636) for upstream LLM providers that gate API access behind a
//! subscription-OAuth surface (Anthropic claude.ai; OpenAI Codex
//! (chatgpt.com)).
//!
//! Operators run `routectl login <provider>` once; routectl spawns a
//! local callback server, drives the browser flow, exchanges the
//! authorization code for tokens, and stores them in
//! `~/.config/routectl/credentials.json`. routectl reads tokens via
//! `OAuthStore`, refreshes near expiry, and retries on 401.
//!
//! Module layout:
//! - `types.rs`: `TokenRecord`, `CredentialsFile`, `AccountInfo`,
//!   `SecretToken`, `SCHEMA_VERSION`, `unix_now`.
//! - `file_io.rs`: atomic load/save of credentials.json (tempfile +
//!   rename), `chmod 0600` enforcement on Unix.
//! - `pkce.rs`: code verifier, code challenge (SHA-256, base64url-no-pad),
//!   CSRF state token. `SysRng`-sourced; never logged.
//! - `store.rs`: `OAuthStore` -- the `SecretStore` impl that owns the
//!   in-memory cache and the refresh single-flight gate.
//! - `login.rs`: callback `axum` sub-app + `webbrowser` launch + flow
//!   driver.
//! - `providers/`: per-upstream `OAuthFlow` impls (claude.ai and
//!   chatgpt.com).

pub(crate) mod file_io;
pub(crate) mod login;
pub(crate) mod pkce;
pub(crate) mod project_cache;
pub(crate) mod providers;
pub(crate) mod rate_limit;
pub(crate) mod store;
pub mod types;

pub use login::{LoginOptions, run as run_login};
pub use project_cache::OAuthStoreProjectCache;
pub use providers::known_provider_ids;
pub use store::{LocalProbe, OAuthStore, OpenOutcome};

/// Where the credentials store lives by default
/// (`$XDG_CONFIG_HOME/routectl/credentials.json`, falling back to
/// `$HOME/.config`). Exposed so a caller that opens the store at an
/// explicit path -- and so stays hermetic in tests -- can still resolve
/// the production path, instead of reaching for `open_default` and losing
/// that seam.
///
/// # Errors
///
/// Fails when neither `XDG_CONFIG_HOME` nor `HOME` is set.
pub fn credentials_default_path() -> OAuthResult<std::path::PathBuf> {
    file_io::default_path()
}
pub use types::{
    AccountInfo, CredentialsFile, SCHEMA_VERSION, SecretToken, TokenRecord, seat_key, unix_now,
};

/// Test-only entry points into the OAuth provider surface. Not part of
/// the supported public API -- the symbols here exist solely so the
/// integration tests under `tests/` can drive private trace-emission
/// helpers without spawning a full callback server. Hidden from rustdoc
/// to keep this out of the published surface contract.
#[doc(hidden)]
pub mod testing {
    pub use super::providers::testing::*;
}

use thiserror::Error;

/// Errors specific to the OAuth subsystem. Bubbles up through
/// `routectl_core::Error::Auth` at API boundaries, but exposed here
/// for callers that need to dispatch on cause (e.g. login command
/// distinguishing "browser failed to open" from "user closed tab").
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OAuthError {
    /// The named provider is not in the registry.
    #[error("unknown oauth provider `{0}` (known: {known})", known = crate::oauth::known_provider_ids().join(", "))]
    UnknownProvider(String),

    /// No stored credentials exist for the named provider.
    #[error("no credentials for `{0}`; run `routectl login {0}` first")]
    NotLoggedIn(String),

    /// The on-disk credentials store uses a schema version this binary
    /// does not support.
    #[error(
        "credentials store schema is v{found}; this binary expects v{expected}; \
             upgrade routectl or delete {path} and re-run `routectl login`"
    )]
    SchemaMismatch {
        /// Schema version found on disk.
        found: u32,
        /// Schema version this binary expects.
        expected: u32,
        /// Path to the credentials file.
        path: String,
    },

    /// The credentials file could not be parsed.
    #[error("oauth credentials file at {path} is corrupted: {detail}")]
    CorruptedFile {
        /// Path to the credentials file.
        path: String,
        /// Sanitized cause of the parse failure.
        detail: String,
    },

    /// A `serve`-owned store opened over a credentials file that failed to
    /// load is kept PRESENT but degraded: every request and every write
    /// surfaces this pre-sanitized cause (path-free, value-free, built at
    /// open time by `store::sanitize_open_error`) instead of resolving or
    /// overwriting the unreadable file. `Display` is the cause verbatim so
    /// the operator sees the true failure class plus the
    /// reloads-without-restart hint. Cleared when the file is fixed and
    /// `reload_from_disk` succeeds. Start-and-degrade is a deliberate
    /// runtime contract -- see `oauth::store`.
    #[error("{0}")]
    Degraded(String),

    /// The OAuth flow did not complete before its deadline.
    #[error("oauth flow timed out; re-run `routectl login {0}`")]
    LoginTimeout(String),

    /// The callback CSRF state token did not match.
    #[error("oauth state mismatch (CSRF token did not match); re-run `routectl login {0}`")]
    StateMismatch(String),

    /// The token endpoint returned an error response.
    #[error("token endpoint returned an error: {0}")]
    TokenEndpoint(String),

    /// The refresh token is expired or revoked; re-login is required.
    #[error("refresh token expired or revoked; re-run `routectl login {0}`")]
    RefreshExpired(String),

    /// A network error occurred during the flow.
    #[error("network error during oauth flow: {0}")]
    Network(String),

    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(String),

    /// An unexpected internal error occurred.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<OAuthError> for routectl_core::Error {
    fn from(e: OAuthError) -> Self {
        Self::Auth(e.to_string())
    }
}

impl From<std::io::Error> for OAuthError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<serde_json::Error> for OAuthError {
    fn from(e: serde_json::Error) -> Self {
        Self::CorruptedFile {
            path: "<unknown>".into(),
            detail: e.to_string(),
        }
    }
}

/// `OAuthError`-flavored Result alias for use within this module.
pub type OAuthResult<T> = std::result::Result<T, OAuthError>;

#[cfg(test)]
mod tests {
    use super::{OAuthError, known_provider_ids};

    /// The `UnknownProvider` operator message must enumerate the live
    /// registry, not a stale hardcoded literal. Pins that the newly
    /// registered `antigravity` provider appears in the "known: ..." list
    /// so an operator correcting a typo sees every valid choice.
    #[test]
    fn unknown_provider_message_lists_all_known_ids() {
        let msg = OAuthError::UnknownProvider("made-up".into()).to_string();
        assert!(msg.contains("made-up"), "must echo the offending id: {msg}");
        assert!(
            msg.contains("antigravity"),
            "known list must include antigravity: {msg}"
        );
        for id in known_provider_ids() {
            assert!(msg.contains(id), "known list must include {id}: {msg}");
        }
    }
}
