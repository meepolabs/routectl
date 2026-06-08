//! `routectl logout <provider> [--label <name>]` -- remove a
//! provider's tokens from `~/.config/routectl/credentials.json`. Without
//! `--label`, removes the default (unlabeled) seat -- today's behavior.
//! With `--label`, removes only that one seat and leaves sibling seats
//! (including the default) intact, so an operator who added a pool is not
//! surprised by a bare `logout` wiping every seat. First-time logout (no
//! record present) is not an error: the operator may have run `logout`
//! before `login`, or be cleaning up a half-completed flow.

use routectl_auth::oauth::types::seat_key;
use routectl_auth::OAuthStore;
use routectl_core::{Error, Result};

use crate::commands::seat::validate_label;

pub async fn run(provider: &str, label: Option<&str>) -> Result<()> {
    let label = validate_label(label)?;
    let store = OAuthStore::open_default()
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    let seat = seat_key(provider, label);
    let removed = store
        .logout(&seat)
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    if removed {
        println!(
            "Logged out of {seat}. Credentials removed from {}.",
            store.path().display()
        );
    } else {
        println!("No credentials found for {seat}; nothing to remove.");
    }
    Ok(())
}
