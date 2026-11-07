//! The typed rejection reason in a client error message: every ingress
//! dialect, before and after the response head, appends the same suffix.

use routectl_core::UpstreamReason;

/// `message` with ` (reason: <token>)` appended for `reason`, unless the
/// message already names the token in a form routectl itself renders: the
/// ` (reason: <token>)` segment (a local credential message), or a
/// `: <token>` segment ending the message or followed by `: ` (the Bedrock
/// access-denied detail). A message naming the bare token in other text still
/// gets the suffix. The token is only ever [`UpstreamReason::as_str`];
/// upstream text decides nothing beyond whether the suffix would repeat it.
pub fn with_reason_suffix(message: String, reason: Option<UpstreamReason>) -> String {
    match reason.map(UpstreamReason::as_str) {
        Some(token) if names_reason(&message, token) => message,
        Some(token) => format!("{message} (reason: {token})"),
        None => message,
    }
}

fn names_reason(message: &str, token: &str) -> bool {
    let colon_segment = format!(": {token}");
    message.contains(&format!(" (reason: {token})"))
        || message
            .match_indices(&colon_segment)
            .map(|(at, _)| &message[at + colon_segment.len()..])
            .any(|rest| rest.is_empty() || rest.starts_with(": "))
}
