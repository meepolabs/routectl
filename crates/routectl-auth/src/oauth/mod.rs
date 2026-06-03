//! OAuth-managed credentials for routectl.
//!
//! This module implements the routectl-side of an OAuth 2.0 PKCE flow
//! (RFC 7636) for upstream LLM providers that gate API access behind a
//! subscription-OAuth surface (Anthropic claude.ai today; OpenAI Codex
//! lands in PR3).
//!
//! Operators run `routectl login <provider>` once; routectl spawns a
//! local callback server, drives the browser flow, exchanges the
//! authorization code for tokens, and stores them in
//! `~/.config/routectl/credentials.json`. PR1 reads tokens via
//! `OAuthStore` and surfaces clear "near-expiry" guidance; refresh +
//! 401 retry land in PR2.
//!
//! Module layout:
//! - `types.rs`: `TokenRecord`, `CredentialsFile`, `AccountInfo`,
//!   `SecretToken`, `SCHEMA_VERSION`, `unix_now`.
//! - `file_io.rs`: atomic load/save of credentials.json (tempfile +
//!   rename), `chmod 0600` enforcement on Unix.
//! - `pkce.rs`: code verifier, code challenge (SHA-256, base64url-no-pad),
//!   CSRF state token. `OsRng`-sourced; never logged.
//! - `store.rs`: `OAuthStore` -- the `SecretStore` impl that owns the
//!   in-memory cache + (in PR2) the refresh single-flight gate.
//! - `login.rs`: callback `axum` sub-app + `webbrowser` launch + flow
//!   driver.
//! - `providers/`: per-upstream `OAuthFlow` impls (claude.ai today;
//!   chatgpt.com in PR3).

pub(crate) mod file_io;
pub mod installation_id;
pub(crate) mod login;
pub(crate) mod pkce;
pub(crate) mod providers;
pub(crate) mod rate_limit;
pub(crate) mod store;
pub mod types;

pub use login::{run as run_login, LoginOptions};
pub use providers::known_provider_ids;
pub use store::OAuthStore;
pub use types::{unix_now, AccountInfo, CredentialsFile, SecretToken, TokenRecord, SCHEMA_VERSION};

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
    #[error("unknown oauth provider `{0}` (known: anthropic, codex)")]
    UnknownProvider(String),

    #[error("no credentials for `{0}`; run `routectl login {0}` first")]
    NotLoggedIn(String),

    #[error(
        "credentials store schema is v{found}; this binary expects v{expected}; \
             upgrade routectl or delete {path} and re-run `routectl login`"
    )]
    SchemaMismatch {
        found: u32,
        expected: u32,
        path: String,
    },

    #[error("oauth credentials file at {path} is corrupted: {detail}")]
    CorruptedFile { path: String, detail: String },

    #[error("oauth flow timed out; re-run `routectl login {0}`")]
    LoginTimeout(String),

    #[error("oauth state mismatch (CSRF token did not match); re-run `routectl login {0}`")]
    StateMismatch(String),

    #[error("token endpoint returned an error: {0}")]
    TokenEndpoint(String),

    #[error("refresh token expired or revoked; re-run `routectl login {0}`")]
    RefreshExpired(String),

    #[error("network error during oauth flow: {0}")]
    Network(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("internal: {0}")]
    Internal(String),
}

impl From<OAuthError> for routectl_core::Error {
    fn from(e: OAuthError) -> Self {
        routectl_core::Error::Auth(e.to_string())
    }
}

impl From<std::io::Error> for OAuthError {
    fn from(e: std::io::Error) -> Self {
        OAuthError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for OAuthError {
    fn from(e: serde_json::Error) -> Self {
        OAuthError::CorruptedFile {
            path: "<unknown>".into(),
            detail: e.to_string(),
        }
    }
}

/// `OAuthError`-flavored Result alias for use within this module.
pub type OAuthResult<T> = std::result::Result<T, OAuthError>;
