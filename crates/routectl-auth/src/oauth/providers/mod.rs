//! Per-upstream OAuth flow definitions.
//!
//! The `OAuthFlow` trait abstracts the vendor-specific bits of an OAuth
//! 2.0 PKCE flow: client_id, scopes, endpoint URLs, callback path, and
//! the (sometimes idiosyncratic) token-endpoint POST body shape.
//!
//! Adding a new provider is one new file in this directory plus a
//! one-line registry entry below. The trait is `pub(crate)`, sealed by
//! visibility -- out-of-tree consumers cannot register providers
//! because the vendor surfaces are reverse-engineered, undocumented,
//! and need code-review discipline when they shift.

pub mod anthropic;
pub mod antigravity;
pub mod codex;
mod refresh_classify;
pub mod xai;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use url::Url;

use crate::oauth::types::TokenRecord;
use crate::oauth::{OAuthError, OAuthResult};

/// Parameters the auth-URL builder needs from the login driver.
/// Provider impls turn these into a vendor-specific URL.
pub struct AuthParams<'a> {
    pub challenge: &'a str,
    pub state: &'a str,
    pub redirect_uri: &'a str,
}

#[async_trait]
pub trait OAuthFlow: Send + Sync {
    /// Stable id used in `oauth://<provider>` URIs and `routectl
    /// login <provider>`.
    fn provider_id(&self) -> &'static str;

    /// Human-friendly name for `routectl whoami` headers.
    fn display_name(&self) -> &'static str;

    /// Build the browser-launch URL with PKCE challenge + CSRF state.
    fn auth_url(&self, params: &AuthParams<'_>) -> Url;

    /// Path the local callback server listens on (`/callback`).
    fn callback_path(&self) -> &'static str {
        "/callback"
    }

    /// Host the advertised redirect_uri is built against. Defaults to
    /// `localhost` (the Anthropic / codex registration), which the login
    /// driver pairs with a 127.0.0.1 bind because browsers resolve
    /// `localhost` back to the loopback. A provider whose public client
    /// registered its redirect against the literal `127.0.0.1` (xAI)
    /// overrides this so the authorize step does not reject the redirect.
    fn callback_host(&self) -> &'static str {
        "localhost"
    }

    /// Fixed local callback port this provider's redirect URIs are
    /// registered against, if any. `None` (the default) means the login
    /// driver binds a kernel-assigned ephemeral port -- the Anthropic
    /// behavior, where the public client allows any `localhost:<port>`
    /// redirect. `Some(port)` means the upstream registered a specific
    /// port (codex: 1455, with a fallback handled by the login driver),
    /// so an ephemeral port would be rejected at the authorize step.
    fn preferred_callback_port(&self) -> Option<u16> {
        None
    }

    /// Ordered list of candidate local ports the login driver tries when
    /// binding the callback listener (browser-launch path only). The
    /// first available port is bound; all candidates are exhausted before
    /// returning an error. An explicit `--callback-port` operator override
    /// bypasses this list entirely.
    ///
    /// The default covers two cases:
    /// - preferred port present (e.g. codex: 1455) -> [preferred, 1457],
    ///   so a single busy port does not abort the login.
    /// - no preferred port (e.g. Anthropic) -> [0] for a kernel-assigned
    ///   ephemeral port.
    ///
    /// A provider whose redirect URIs are registered against exactly one
    /// fixed port and where 1457 is NOT in its allow-list MUST override
    /// to return only [preferred_port], preventing the driver from falling
    /// back to a codex-specific port and advertising an unregistered
    /// redirect URI.
    fn callback_port_candidates(&self) -> Vec<u16> {
        match self.preferred_callback_port() {
            Some(p) => vec![p, 1457],
            None => vec![0],
        }
    }

    /// URL the upstream displays to operators using the headless
    /// `--print-url` flow. This is the value the operator's browser
    /// is redirected to after consent; routectl never serves it.
    /// Provider-specific because each upstream owns its own "manual
    /// paste" landing page.
    fn manual_redirect_url(&self) -> &'static str;

    /// POST to the token endpoint with `grant_type=authorization_code`.
    /// Provider-specific because some upstreams send JSON, others
    /// form-urlencoded; some require extra headers
    /// (e.g. `anthropic-beta: oauth-2025-04-20`); some surface
    /// `account` info inline, others via id_token claims. Some upstreams
    /// (Anthropic) also expect the CSRF `state` echoed in the body
    /// despite RFC 6749 not requiring it.
    async fn exchange_code(
        &self,
        http: &reqwest::Client,
        code: &str,
        verifier: &str,
        state: &str,
        redirect_uri: &str,
    ) -> OAuthResult<TokenRecord>;

    /// POST to the token endpoint with `grant_type=refresh_token`.
    /// Provider-specific for the same reasons as `exchange_code`.
    /// Called from `OAuthStore::refresh_under_lock` for both the
    /// near-expiry (`get`) and 401-recovery (`on_auth_failure`) paths.
    async fn refresh_token(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord>;
}

/// Look up a provider impl by id. Returns `OAuthError::UnknownProvider`
/// (whose Display drives the operator-visible "known: ..." message
/// from `known_provider_ids`) if the id is not registered.
pub fn lookup(provider_id: &str) -> OAuthResult<&'static dyn OAuthFlow> {
    match provider_id {
        "anthropic" => Ok(&anthropic::Anthropic),
        "codex" => Ok(&codex::Codex),
        "xai" => Ok(&xai::Xai),
        "antigravity" => Ok(&antigravity::Antigravity),
        other => Err(OAuthError::UnknownProvider(other.to_string())),
    }
}

/// All known provider ids. Used by the in-crate consistency test below
/// and by the login arg-validator. Kept as a single source of truth
/// alongside the `lookup` match above so the two stay in sync. Re-exported
/// at the `oauth` module root (`oauth::known_provider_ids`) so the CLI can
/// build its clap allowed-provider set from this list, keeping the accepted
/// set in lockstep with the registry. The operator-visible "unknown oauth
/// provider" text is built from `OAuthError::Display`.
pub const fn known_provider_ids() -> &'static [&'static str] {
    &["anthropic", "codex", "xai", "antigravity"]
}

/// Test-only re-exports for the integration tests under
/// `crates/routectl-auth/tests/`. Hidden from rustdoc and gated by
/// `#[doc(hidden)]` so this is not part of the supported public API.
#[doc(hidden)]
pub mod testing {
    use super::OAuthFlow;
    use super::codex::{Codex, decode_token_response_traced};
    use super::xai::{Xai, decode_token_response_traced as xai_decode_token_response_traced};
    use crate::oauth::OAuthResult;
    use crate::oauth::types::TokenRecord;

    /// Drive the codex provider's refresh-token leg. Wraps the
    /// `OAuthFlow::refresh_token` call so the integration test can
    /// invoke it without taking a dep on the crate-private trait.
    pub async fn codex_refresh(
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        Codex.refresh_token(http, refresh_token).await
    }

    /// Drive the response-side of a refresh-token call through the
    /// tracing-instrumented decoder. The integration test passes a
    /// synthetic `reqwest::Response`, asserts on the captured events,
    /// and uses the `Result` shape to confirm the error mapping is
    /// preserved alongside the trace emission.
    pub async fn decode_refresh_response_traced(
        resp: reqwest::Response,
        prior_refresh: Option<&str>,
        prior_refresh_sha8: &str,
    ) -> OAuthResult<TokenRecord> {
        decode_token_response_traced(resp, prior_refresh, prior_refresh_sha8).await
    }

    /// Drive the xai provider's refresh-token leg. Wraps the
    /// `OAuthFlow::refresh_token` call so the integration test can
    /// invoke it without taking a dep on the crate-private trait.
    pub async fn xai_refresh(
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        Xai.refresh_token(http, refresh_token).await
    }

    /// Drive the response-side of an xai refresh-token call through the
    /// tracing-instrumented decoder. The integration test passes a
    /// synthetic `reqwest::Response`, asserts on the captured events,
    /// and uses the `Result` shape to confirm the error mapping is
    /// preserved alongside the trace emission.
    pub async fn decode_xai_refresh_response_traced(
        resp: reqwest::Response,
        prior_refresh: Option<&str>,
        prior_refresh_sha8: &str,
    ) -> OAuthResult<TokenRecord> {
        xai_decode_token_response_traced(resp, prior_refresh, prior_refresh_sha8).await
    }
}

/// Build a body-free `TokenEndpoint` error from a serde_json parse
/// failure of a token-exchange response.
///
/// The raw response body carries freshly minted access/refresh tokens,
/// and serde's own `Display` echoes offending value fragments (`invalid
/// type: string "sk-...", ...`), so NEITHER the body nor the serde
/// message may reach an operator-visible error or log. Emit only the
/// error category plus its line/column position -- secret-free integers
/// that are enough to triage a shape mismatch. Shared by every provider's
/// `parse_token_response_json` so the four cannot drift.
pub(super) fn token_parse_error(e: &serde_json::Error) -> OAuthError {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    };
    OAuthError::TokenEndpoint(format!(
        "parse token response: {kind} error at line {} column {}",
        e.line(),
        e.column()
    ))
}

/// Longest OAuth error code we will echo. RFC 6749 section 5.2 codes
/// (`invalid_grant`, `invalid_client`, `unsupported_grant_type`) and the
/// provider refresh codes (`refresh_token_expired`) are short snake_case
/// tokens; a value longer than this is not a code and is dropped.
const MAX_TOKEN_ERROR_CODE_LEN: usize = 48;

/// Extract a machine-usable OAuth error code from a non-2xx token-endpoint
/// body, fail-safe by SHAPE. Returns `Some(code)` only when a recognized
/// error field (`error` as a string, `error.code`, or a top-level `code`)
/// holds a value matching the tightly-bounded OAuth-error-code shape:
/// 1..=`MAX_TOKEN_ERROR_CODE_LEN` bytes drawn only from `[a-z0-9_]`. Any
/// other value fails the check and is dropped -- an echoed authorization
/// code, PKCE verifier, or partially minted token carries uppercase, `-`,
/// `.`, or over-length runs, so it can never survive as a "code".
pub(super) fn safe_token_error_code(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let raw = token_error_code_field(&v)?;
    is_token_error_code_shape(&raw).then_some(raw)
}

/// Pull the candidate error-code string from the recognized JSON fields,
/// without shape-validating it (the caller does that).
fn token_error_code_field(v: &serde_json::Value) -> Option<String> {
    if let Some(err) = v.get("error") {
        if let Some(code) = err.as_str() {
            return Some(code.to_string());
        }
        if let Some(code) = err.get("code").and_then(|c| c.as_str()) {
            return Some(code.to_string());
        }
    }
    v.get("code").and_then(|c| c.as_str()).map(String::from)
}

/// Whether `s` matches the bounded OAuth-error-code shape (nonempty, at
/// most `MAX_TOKEN_ERROR_CODE_LEN` bytes, only `[a-z0-9_]`).
fn is_token_error_code_shape(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_TOKEN_ERROR_CODE_LEN
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Build the `TokenEndpoint` error for a non-2xx token-endpoint response
/// WITHOUT embedding the raw body. The exchange body carries the single-use
/// authorization code plus the PKCE verifier and the refresh body carries
/// the long-lived refresh token; an intercepting proxy can echo either back
/// in an error envelope, so the body is never surfaced. Only `status` +
/// `url` are kept, plus a machine-usable error code IF the body holds one in
/// the tightly-bounded shape (see [`safe_token_error_code`]). Shared by every
/// provider's `check_status_error` so the four cannot drift.
pub(super) fn token_status_error(status: reqwest::StatusCode, url: &str, body: &str) -> OAuthError {
    match safe_token_error_code(body) {
        Some(code) => OAuthError::TokenEndpoint(format!("{} {} ({code})", status.as_u16(), url)),
        None => OAuthError::TokenEndpoint(format!("{} {}", status.as_u16(), url)),
    }
}

/// First 4 bytes of `SHA-256(token)` rendered as 8 lowercase hex
/// chars. Stable, deterministic correlation id for refresh-flow trace
/// events without leaking token VALUES. 4 bytes (32 bits) gives a
/// collision probability that is fine for log correlation across a
/// single operator's refresh history -- this is NOT a cryptographic
/// commitment, just a logging tag.
///
/// Shared by the Anthropic and codex provider impls so both refresh
/// paths emit structurally identical trace events keyed off the same
/// sha8 computation.
pub(super) fn sha8(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(8);
    for b in &digest[..4] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_token_error_code_accepts_bounded_oauth_codes() {
        assert_eq!(
            safe_token_error_code(r#"{"error":"invalid_grant"}"#).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            safe_token_error_code(r#"{"error":{"code":"invalid_client"}}"#).as_deref(),
            Some("invalid_client")
        );
        assert_eq!(
            safe_token_error_code(r#"{"code":"unsupported_grant_type"}"#).as_deref(),
            Some("unsupported_grant_type")
        );
    }

    #[test]
    fn safe_token_error_code_rejects_credential_shaped_values() {
        // An echoed authorization code / minted token carries uppercase,
        // hyphens, dots, or runs longer than the bound -- none may survive.
        assert_eq!(
            safe_token_error_code(r#"{"error":"ac_MixedCase-AUTHCODE.deadbeef"}"#),
            None
        );
        assert_eq!(
            safe_token_error_code(r#"{"error":"sk-live-TOKEN99"}"#),
            None
        );
        let over_long = "a".repeat(MAX_TOKEN_ERROR_CODE_LEN + 1);
        assert_eq!(
            safe_token_error_code(&format!(r#"{{"error":"{over_long}"}}"#)),
            None
        );
        // Non-string / absent / non-JSON bodies yield no code.
        assert_eq!(safe_token_error_code(r#"{"error":123}"#), None);
        assert_eq!(safe_token_error_code("not json"), None);
        assert_eq!(safe_token_error_code(r#"{"error":""}"#), None);
    }

    #[test]
    fn token_status_error_never_embeds_body_but_keeps_status_and_url() {
        let err = token_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            "https://example.invalid/token",
            r#"{"error":"invalid_grant","secret_echo":"AUTHCODE-LEAK"}"#,
        );
        let msg = err.to_string();
        assert!(msg.contains("400"), "{msg}");
        assert!(msg.contains("https://example.invalid/token"), "{msg}");
        assert!(msg.contains("invalid_grant"), "{msg}");
        assert!(!msg.contains("AUTHCODE-LEAK"), "body leaked: {msg}");
        assert!(!msg.contains("secret_echo"), "body leaked: {msg}");
    }

    #[test]
    fn lookup_and_known_ids_stay_in_sync() {
        // Every id in `known_provider_ids()` must resolve via `lookup`.
        for id in known_provider_ids() {
            assert!(lookup(id).is_ok(), "lookup missing entry for {id}");
        }
    }

    #[test]
    fn callback_host_defaults_to_localhost_and_xai_overrides() {
        // The default host (anthropic / codex) is `localhost`; xAI
        // registered its loopback redirect against the literal IP, so it
        // must override to `127.0.0.1`.
        assert_eq!(lookup("anthropic").unwrap().callback_host(), "localhost");
        assert_eq!(lookup("codex").unwrap().callback_host(), "localhost");
        assert_eq!(lookup("xai").unwrap().callback_host(), "127.0.0.1");
    }

    #[test]
    fn xai_callback_port_candidates_is_single_registered_port_only() {
        // xAI registered its redirect against exactly one fixed port (56121).
        // Port 1457 (codex fallback) is NOT in xAI's allow-list, so the
        // candidate list must never include it.
        let candidates = lookup("xai").unwrap().callback_port_candidates();
        assert_eq!(candidates, vec![56121u16]);
        assert!(
            !candidates.contains(&1457),
            "xai must not fall back to codex port 1457; got {candidates:?}"
        );
    }

    #[test]
    fn codex_callback_port_candidates_includes_fallback() {
        // Codex registers both 1455 (preferred) and 1457 (fallback), so
        // the candidate list must be exactly [1455, 1457] in that order.
        assert_eq!(
            lookup("codex").unwrap().callback_port_candidates(),
            vec![1455u16, 1457u16]
        );
    }
}
