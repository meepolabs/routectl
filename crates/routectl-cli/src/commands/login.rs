//! `routectl login <provider>` -- run the OAuth 2.0 PKCE flow and
//! persist tokens to `~/.config/routectl/credentials.json`.
//!
//! Providers: `anthropic` (claude.ai) and `codex` (OpenAI
//! ChatGPT/Codex). The accepted set is validated at the clap layer
//! against the auth registry; this body is provider-id-generic except
//! for the `--print-url` guard below.

use routectl_auth::{LoginOptions, OAuthStore};
use routectl_core::{Error, Result};

use crate::commands::seat::validate_label;

pub async fn run(
    provider: &str,
    print_url: bool,
    callback_port: Option<u16>,
    label: Option<&str>,
) -> Result<()> {
    if print_url && !provider_supports_print_url(provider) {
        return Err(Error::Auth(format!(
            "{provider} login does not support --print-url; run the default \
             browser flow (SSH users can port-forward 1455)"
        )));
    }
    let label = validate_label(label)?;
    let store = OAuthStore::open_default()
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    let opts = LoginOptions::new()
        .with_print_url(print_url)
        .with_callback_port(callback_port)
        .with_label(label.map(str::to_string));
    routectl_auth::oauth::run_login(provider, &store, opts)
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    Ok(())
}

/// Whether `provider` has a real headless `--print-url` landing page.
///
/// The `--print-url` flow prints the auth URL and reads a `code#state`
/// pair pasted back from the provider's success page. That only works
/// when the upstream serves such a page: claude.ai does, codex does not
/// (its `manual_redirect_url` is just the authorize URL placeholder, so
/// the flow always runs through the local callback server). Codex
/// operators on SSH should port-forward the fixed callback port (1455)
/// and use the default browser flow instead.
///
/// Kept as a CLI-local predicate rather than an `OAuthFlow` trait method:
/// it is a UX concern of this one command, and the registry stays the
/// single source of truth for *which* providers exist (clap validates
/// against `known_provider_ids()`); this only governs *how* a known
/// provider may be driven.
fn provider_supports_print_url(provider: &str) -> bool {
    !matches!(provider, "codex")
}

#[cfg(test)]
mod tests {
    use super::provider_supports_print_url;

    #[test]
    fn anthropic_supports_print_url() {
        assert!(provider_supports_print_url("anthropic"));
    }

    #[test]
    fn codex_does_not_support_print_url() {
        assert!(!provider_supports_print_url("codex"));
    }
}
