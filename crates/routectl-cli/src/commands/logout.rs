//! `routectl logout <provider>` -- remove a provider's tokens from
//! `~/.config/routectl/credentials.json`. First-time logout (no record
//! present) is not an error: the operator may have run `logout` before
//! `login`, or be cleaning up a half-completed flow.

use routectl_auth::OAuthStore;
use routectl_core::{Error, Result};

pub async fn run(provider: &str) -> Result<()> {
    let store = OAuthStore::open_default()
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    let removed = store
        .logout(provider)
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    if removed {
        println!(
            "Logged out of {provider}. Credentials removed from {}.",
            store.path().display()
        );
    } else {
        println!("No credentials found for {provider}; nothing to remove.");
    }
    Ok(())
}
