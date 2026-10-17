//! The client-facing message for a local credential error
//! ([`routectl_core::Error::Credential`]). Fixed text plus the reason token
//! and, when every seat is parked, the remaining wait: never the provider id
//! or the OAuth detail, which name routing topology and operator guidance.

use routectl_core::UpstreamReason;

const LEAD: &str = "auth error: server-side credential resolution failed";

/// The message for a credential error carrying `reason`, parked until
/// `parked_until_ms` (epoch ms) when every reachable seat is parked.
pub fn credential_error_message(
    reason: UpstreamReason,
    parked_until_ms: Option<u64>,
    now_ms: u64,
) -> String {
    let token = reason.as_str();
    match parked_until_ms {
        Some(until) => {
            let remaining_secs = until.saturating_sub(now_ms).div_ceil(1000);
            format!("{LEAD} (reason: {token}); every seat is parked for another {remaining_secs}s")
        }
        None => format!("{LEAD} (reason: {token})"),
    }
}

/// [`credential_error_message`] against the wall clock.
pub fn credential_error_message_now(
    reason: UpstreamReason,
    parked_until_ms: Option<u64>,
) -> String {
    let now_ms = u64::try_from(super::usage_capture::epoch_ms_now()).unwrap_or(0);
    credential_error_message(reason, parked_until_ms, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_remaining_wait_rounds_up_and_never_goes_negative() {
        let rows = [
            ("a partial second rounds up", 10_001, 0, "2s"),
            ("an exact second", 10_000, 0, "1s"),
            ("a lapsed deadline", 0, 5_000, "0s"),
        ];
        for (name, until, now, expected) in rows {
            let message = credential_error_message(
                UpstreamReason::CredentialExpired,
                Some(until),
                now + 9_000,
            );

            assert!(
                message.ends_with(&format!("for another {expected}")),
                "{name}: {message}"
            );
        }
    }
}
