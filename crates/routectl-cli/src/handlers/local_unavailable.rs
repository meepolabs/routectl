//! The client-facing parts of a local eligibility refusal
//! ([`routectl_core::Error::LocallyUnavailable`]): the bounded wait in whole
//! seconds and the fixed message naming the reason token and that wait.

use std::time::Duration;

use routectl_core::EligibilityReason;

/// Shortest wait a known recovery advertises: `Retry-After: 0` would tell a
/// client to retry at once, which contradicts the refusal.
const MIN_RETRY_AFTER_SECS: u64 = 1;

/// Longest wait advertised, so a far-off recovery never parks a client for
/// longer than an hour on one answer.
const MAX_RETRY_AFTER_SECS: u64 = 3600;

/// The advertised wait for a known recovery, in whole seconds: rounded up,
/// then clamped to 1..=3600.
pub const fn retry_after_secs(wait: Duration) -> u64 {
    let secs = wait
        .as_secs()
        .saturating_add(if wait.subsec_nanos() > 0 { 1 } else { 0 });
    if secs < MIN_RETRY_AFTER_SECS {
        MIN_RETRY_AFTER_SECS
    } else if secs > MAX_RETRY_AFTER_SECS {
        MAX_RETRY_AFTER_SECS
    } else {
        secs
    }
}

/// The fixed message for a refusal carrying `reason`, naming the wait when
/// `wait_secs` is known.
pub fn locally_unavailable_message(reason: EligibilityReason, wait_secs: Option<u64>) -> String {
    let token = reason.as_str();
    let lead = format!("no eligible target for this request (reason: {token})");
    match wait_secs {
        Some(secs) => format!("{lead}; retry after {secs}s"),
        None => lead,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_rounds_up_and_clamps_to_one_hour() {
        let rows = [
            ("zero", Duration::ZERO, 1),
            ("sub-second", Duration::from_millis(250), 1),
            ("partial second", Duration::from_millis(1500), 2),
            ("whole seconds", Duration::from_secs(30), 30),
            ("just under the cap", Duration::from_millis(3_599_500), 3600),
            ("at the cap", Duration::from_hours(1), 3600),
            ("past the cap", Duration::from_hours(2), 3600),
            ("far past the cap", Duration::MAX, 3600),
        ];
        for (name, input, expected) in rows {
            assert_eq!(retry_after_secs(input), expected, "{name}");
        }
    }

    #[test]
    fn the_message_names_the_token_and_the_wait_only_when_known() {
        let known = locally_unavailable_message(EligibilityReason::BreakerOpen, Some(30));
        let unknown = locally_unavailable_message(EligibilityReason::CredentialUnusable, None);

        assert_eq!(
            known,
            "no eligible target for this request (reason: breaker_open); retry after 30s"
        );
        assert_eq!(
            unknown,
            "no eligible target for this request (reason: credential_unusable)"
        );
    }
}
