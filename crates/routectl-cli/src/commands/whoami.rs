//! `routectl whoami` -- print the OAuth provider state from the
//! routectl-managed credentials store. Exits 0 if at least one
//! provider is logged in; exits 2 otherwise (so shell scripts can
//! `if routectl whoami; then ...`).

use routectl_auth::oauth::types::unix_now;
use routectl_auth::OAuthStore;
use routectl_core::{Error, Result};

pub async fn run() -> Result<i32> {
    let store = OAuthStore::open_default()
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    let entries = store.list().await;

    if entries.is_empty() {
        println!("No oauth providers logged in.");
        println!("Run `routectl login <provider>` to authenticate.");
        return Ok(2);
    }

    let now = unix_now();
    for (i, (provider, rec)) in entries.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("provider: {provider}");
        if let Some(email) = &rec.account.email {
            println!("  email:       {email}");
        }
        if let Some(account_id) = &rec.account.account_id {
            println!("  account_id:  {account_id}");
        }
        if !rec.scopes.is_empty() {
            println!("  scopes:      {}", rec.scopes.join(", "));
        }
        println!(
            "  expires_in:  {}",
            format_expires_in(rec.expires_at_unix, now, provider)
        );
        println!("  obtained:    {}", format_unix(rec.obtained_at_unix));
    }
    Ok(0)
}

fn format_expires_in(expires_at: u64, now: u64, provider: &str) -> String {
    if expires_at <= now {
        let ago = now - expires_at;
        return format!(
            "expired {} ago (run `routectl login {provider}` to refresh)",
            format_duration(ago)
        );
    }
    let remaining = expires_at - now;
    format_duration(remaining)
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

fn format_unix(secs: u64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(secs as i64, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("unix {secs}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_buckets() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(125), "2m 5s");
        assert_eq!(format_duration(3700), "1h 1m");
        assert_eq!(format_duration(90_000), "1d 1h");
    }

    #[test]
    fn format_expires_in_handles_past_and_future() {
        assert!(format_expires_in(2000, 1500, "anthropic").contains("8m"));
        let past = format_expires_in(1500, 2000, "anthropic");
        assert!(past.starts_with("expired"));
        assert!(past.contains("8m"));
        assert!(
            past.contains("routectl login anthropic"),
            "expected provider-aware refresh hint, got: {past}"
        );
    }

    #[test]
    fn format_unix_uses_iso_like_format() {
        // Epoch second 0 = 1970-01-01 00:00:00 UTC.
        assert_eq!(format_unix(0), "1970-01-01 00:00:00 UTC");
    }
}
