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

use async_trait::async_trait;
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
    /// Wired up in a prior change (refresh + 401 retry); unused in a prior change.
    #[allow(dead_code)]
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
        // "codex" provider lands in a prior change (see ROADMAP v0.7).
        other => Err(OAuthError::UnknownProvider(other.to_string())),
    }
}

/// All known provider ids. Used by the in-crate consistency test below
/// (and, post-a prior change, by the codex login arg-validator). Kept as a single
/// source of truth alongside the `lookup` match above so the two stay
/// in sync. Marked `pub(crate)` because the surface is internal: the
/// CLI hardcodes `anthropic` in clap, and the operator-visible
/// "unknown oauth provider" text is built from `OAuthError::Display`.
#[allow(dead_code)] // wired in a prior change when codex registers
pub(crate) fn known_provider_ids() -> &'static [&'static str] {
    &["anthropic"]
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
}
