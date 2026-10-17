//! The typed rejection reason in a client error message: every ingress
//! dialect, before and after the response head, appends the same suffix.

use routectl_core::UpstreamReason;

/// `message` with ` (reason: <token>)` appended for `reason`, unless the
/// message already names the token (a provider or local message that renders
/// its own reason segment). The token is only ever [`UpstreamReason::as_str`];
/// upstream text decides nothing beyond whether the suffix would repeat it.
pub fn with_reason_suffix(message: String, reason: Option<UpstreamReason>) -> String {
    match reason.map(UpstreamReason::as_str) {
        Some(token) if !message.contains(token) => format!("{message} (reason: {token})"),
        _ => message,
    }
}
