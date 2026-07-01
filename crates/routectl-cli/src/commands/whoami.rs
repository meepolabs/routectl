//! `routectl whoami` -- print the OAuth provider state from the
//! routectl-managed credentials store. Exits 0 if at least one
//! seat is logged in; exits 2 otherwise (so shell scripts can
//! `if routectl whoami; then ...`).
//!
//! Seats are grouped under their provider: the default (unlabeled) seat
//! renders as `<provider> (default)` and each labeled seat as
//! `<provider>#<label>`, each with its own expiry. A lone default seat
//! still reads cleanly as a single block.

use std::collections::BTreeMap;

use routectl_auth::OAuthStore;
use routectl_auth::oauth::types::{TokenRecord, unix_now};
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
    for line in render_whoami(&entries, now) {
        println!("{line}");
    }
    Ok(0)
}

/// One stored seat as seen by the renderer: the provider it belongs to,
/// an optional label (`None` for the default/unlabeled seat), and the
/// heading shown to the operator.
struct SeatView<'a> {
    provider: &'a str,
    heading: String,
    rec: &'a TokenRecord,
}

/// Split a credentials-map seat key back into `(provider, label)`. The
/// unlabeled/default seat is the bare provider key (`label: None`); a
/// labeled seat keys as `provider#label`. Inverse of `seat_key`.
fn split_seat_key(key: &str) -> (&str, Option<&str>) {
    match key.split_once('#') {
        Some((provider, label)) => (provider, Some(label)),
        None => (key, None),
    }
}

/// Build the operator-facing lines for `routectl whoami`, grouping
/// seats under their provider. Pure so the grouping and per-seat
/// rendering are unit-testable without opening a store. A blank line
/// separates seats; the default seat is labeled `<provider> (default)`
/// and labeled seats as `<provider>#<label>`.
fn render_whoami(entries: &[(String, TokenRecord)], now: u64) -> Vec<String> {
    // Group seats by provider so a pool reads as a block. The store
    // already returns keys in sorted order (BTreeMap), so collecting into
    // a BTreeMap of provider -> seats keeps both providers and their
    // seats sorted; the default seat sorts before any `provider#label`
    // because the bare key has no `#`.
    let mut by_provider: BTreeMap<&str, Vec<SeatView<'_>>> = BTreeMap::new();
    for (key, rec) in entries {
        let (provider, label) = split_seat_key(key);
        let heading = match label {
            None => format!("{provider} (default)"),
            Some(label) => format!("{provider}#{label}"),
        };
        by_provider.entry(provider).or_default().push(SeatView {
            provider,
            heading,
            rec,
        });
    }

    let mut lines = Vec::new();
    let mut first = true;
    for seats in by_provider.into_values() {
        for seat in seats {
            if !first {
                lines.push(String::new());
            }
            first = false;
            lines.extend(render_seat(&seat, now));
        }
    }
    lines
}

/// Render the block of lines for a single seat.
fn render_seat(seat: &SeatView<'_>, now: u64) -> Vec<String> {
    let rec = seat.rec;
    let mut lines = vec![format!("seat: {}", seat.heading)];
    if let Some(email) = &rec.account.email {
        lines.push(format!("  email:       {email}"));
    }
    if let Some(account_id) = &rec.account.account_id {
        lines.push(format!("  account_id:  {account_id}"));
    }
    if !rec.scopes.is_empty() {
        lines.push(format!("  scopes:      {}", rec.scopes.join(", ")));
    }
    lines.push(format!(
        "  expires_in:  {}",
        format_expires_in(rec.expires_at_unix, now, seat.provider)
    ));
    lines.push(format!(
        "  obtained:    {}",
        format_unix(rec.obtained_at_unix)
    ));
    lines
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
    Utc.timestamp_opt(secs as i64, 0).single().map_or_else(
        || format!("unix {secs}"),
        |dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `TokenRecord` for the renderer tests. `TokenRecord` is
    /// `#[non_exhaustive]`, so it cannot be struct-literal constructed
    /// from this crate; deserialize from JSON instead (the same on-disk
    /// shape `routectl login` persists).
    fn rec(email: &str, expires_at: u64) -> TokenRecord {
        let json = serde_json::json!({
            "access_token": "tok",
            "refresh_token": "rtok",
            "token_type": "Bearer",
            "expires_at_unix": expires_at,
            "scopes": ["user:inference"],
            "account": { "email": email, "account_id": "acct-x" },
            "obtained_at_unix": 0,
        });
        serde_json::from_value(json).expect("valid TokenRecord json")
    }

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

    #[test]
    fn split_seat_key_separates_default_and_labeled() {
        assert_eq!(split_seat_key("anthropic"), ("anthropic", None));
        assert_eq!(
            split_seat_key("anthropic#seat-b"),
            ("anthropic", Some("seat-b"))
        );
    }

    #[test]
    fn render_whoami_lists_seats_grouped_by_provider() {
        // Arrange: a provider with a default seat and a labeled seat.
        let entries = vec![
            ("anthropic".to_string(), rec("default@example.com", 4000)),
            (
                "anthropic#seat-b".to_string(),
                rec("seat-b@example.com", 4000),
            ),
        ];

        // Act
        let lines = render_whoami(&entries, 1000);

        // Assert: both seats render, the default labeled `(default)` and
        // the labeled seat as `anthropic#seat-b`, each with its own
        // expiry and email block.
        let joined = lines.join("\n");
        assert!(
            joined.contains("seat: anthropic (default)"),
            "default seat heading missing: {joined}"
        );
        assert!(
            joined.contains("seat: anthropic#seat-b"),
            "labeled seat heading missing: {joined}"
        );
        assert!(joined.contains("default@example.com"));
        assert!(joined.contains("seat-b@example.com"));
        // Two expiry lines -- one per seat.
        assert_eq!(
            joined.matches("expires_in:").count(),
            2,
            "each seat must show its own expiry: {joined}"
        );
    }

    #[test]
    fn render_whoami_single_default_seat_reads_cleanly() {
        // A lone default seat renders one block with no `(default)`
        // ambiguity and no leading blank line.
        let entries = vec![("anthropic".to_string(), rec("solo@example.com", 4000))];
        let lines = render_whoami(&entries, 1000);
        assert_eq!(lines[0], "seat: anthropic (default)");
        assert!(!lines.is_empty());
        assert!(lines.join("\n").contains("solo@example.com"));
    }
}
