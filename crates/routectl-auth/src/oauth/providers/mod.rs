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

pub(crate) mod anthropic;
pub(crate) mod antigravity;
pub(crate) mod codex;
pub(crate) mod xai;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use url::Url;

use crate::oauth::types::TokenRecord;
use crate::oauth::{OAuthError, OAuthResult};

/// Parameters the auth-URL builder needs from the login driver.
/// Provider impls turn these into a vendor-specific URL.
pub(crate) struct AuthParams<'a> {
    pub challenge: &'a str,
    pub state: &'a str,
    pub redirect_uri: &'a str,
}

#[async_trait]
pub(crate) trait OAuthFlow: Send + Sync {
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
pub(crate) fn lookup(provider_id: &str) -> OAuthResult<&'static dyn OAuthFlow> {
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
pub fn known_provider_ids() -> &'static [&'static str] {
    &["anthropic", "codex", "xai", "antigravity"]
}

/// Test-only re-exports for the integration tests under
/// `crates/routectl-auth/tests/`. Hidden from rustdoc and gated by
/// `#[doc(hidden)]` so this is not part of the supported public API.
#[doc(hidden)]
pub mod testing {
    use super::codex::{decode_token_response_traced, Codex};
    use super::xai::{decode_token_response_traced as xai_decode_token_response_traced, Xai};
    use super::OAuthFlow;
    use crate::oauth::types::TokenRecord;
    use crate::oauth::OAuthResult;

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

/// Truncate a string for inclusion in error/log messages without
/// dragging unbounded upstream payloads into routectl logs. Pure ASCII
/// (re-encodes through `&str` byte slicing); guarantees the result is
/// at most `max + suffix` bytes. Note: `max` is treated as a byte
/// count; multibyte UTF-8 input may be truncated on a non-char boundary
/// in edge cases but the function never panics because we slice through
/// `char_indices`.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Walk char boundaries to avoid `s[..max]` panicking inside a
    // multibyte sequence.
    let cut = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max)
        .last()
        .unwrap_or(0);
    format!("{}... ({} bytes total)", &s[..cut], s.len())
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
    fn truncate_handles_short_and_long() {
        assert_eq!(truncate("hi", 10), "hi");
        let long = "x".repeat(600);
        let trimmed = truncate(&long, 500);
        assert!(trimmed.starts_with(&"x".repeat(500)));
        assert!(trimmed.contains("600 bytes total"));
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
