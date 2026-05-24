//! `routectl login <provider>` -- run the OAuth 2.0 PKCE flow and
//! persist tokens to `~/.config/routectl/credentials.json`.
//!
//! a prior change ships the Anthropic provider only. a prior change adds Codex; a prior change adds
//! refresh + 401 retry without changing this command.

use routectl_auth::{LoginOptions, OAuthStore};
use routectl_core::{Error, Result};

pub async fn run(provider: &str, print_url: bool, callback_port: Option<u16>) -> Result<()> {
    let store = OAuthStore::open_default()
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    let opts = LoginOptions::new()
        .with_print_url(print_url)
        .with_callback_port(callback_port);
    routectl_auth::oauth::run_login(provider, &store, opts)
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    Ok(())
}
