//! `routectl refresh <provider> [--label <name>]` -- force a token
//! refresh through the per-seat single-flight gate, regardless of expiry.
//! Useful when the operator suspects a token has been revoked
//! server-side or wants to cycle a fresh access token before a
//! long-running session. Without `--label`, refreshes the default
//! (unlabeled) seat -- today's behavior. With `--label`, refreshes only
//! that one seat.

use routectl_auth::oauth::types::{seat_key, unix_now};
use routectl_auth::OAuthStore;
use routectl_core::{Error, Result};

use crate::commands::seat::validate_label;

pub async fn run(provider: &str, label: Option<&str>) -> Result<()> {
    let label = validate_label(label)?;
    let store = OAuthStore::open_default()
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    // `force_refresh` already wraps refresh failures with the actionable
    // "re-run `routectl login {provider}`" hint. Surface the message
    // verbatim by propagating the error.
    let new_rec = store.force_refresh(provider, label).await?;
    let now = unix_now();
    let remaining = new_rec.expires_at_unix.saturating_sub(now);
    let human = humanize_remaining(remaining);
    let seat = seat_key(provider, label);
    println!(
        "Refreshed {seat}. Access token expires in {human} (expires_at_unix={}).",
        new_rec.expires_at_unix
    );
    Ok(())
}

/// Render seconds-from-now as a short human-readable string. Operators
/// running `routectl refresh` to verify a token is alive should see
/// "expires in ~58m" not "expires in 3520s".
fn humanize_remaining(secs: u64) -> String {
    if secs == 0 {
        return "now".to_string();
    }
    let mins = secs / 60;
    if mins < 1 {
        return format!("{secs}s");
    }
    let hours = mins / 60;
    if hours < 1 {
        return format!("~{mins}m");
    }
    let days = hours / 24;
    if days < 1 {
        let extra_mins = mins % 60;
        if extra_mins == 0 {
            return format!("~{hours}h");
        }
        return format!("~{hours}h{extra_mins}m");
    }
    let extra_hours = hours % 24;
    if extra_hours == 0 {
        format!("~{days}d")
    } else {
        format!("~{days}d{extra_hours}h")
    }
}

#[cfg(test)]
mod tests {
    use super::humanize_remaining;

    #[test]
    fn humanize_zero() {
        assert_eq!(humanize_remaining(0), "now");
    }

    #[test]
    fn humanize_seconds() {
        assert_eq!(humanize_remaining(45), "45s");
    }

    #[test]
    fn humanize_minutes() {
        assert_eq!(humanize_remaining(180), "~3m");
    }

    #[test]
    fn humanize_hours() {
        assert_eq!(humanize_remaining(3600), "~1h");
        assert_eq!(humanize_remaining(3600 + 600), "~1h10m");
    }

    #[test]
    fn humanize_days() {
        assert_eq!(humanize_remaining(86_400), "~1d");
        assert_eq!(humanize_remaining(86_400 + 3600 * 5), "~1d5h");
    }
}
