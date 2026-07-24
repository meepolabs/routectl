//! Shared redaction for AWS/Bedrock upstream error envelopes.
//!
//! Both the native Bedrock lane (`bedrock/mod.rs`) and the anthropic-api
//! mantle lift (`anthropic_api/mod.rs`, which routes Messages-shaped
//! requests to AWS Bedrock) receive AWS error bodies. A 403 AccessDenied
//! body names the caller principal ARN, the account id, and the resource
//! ARN; none of that may reach a log line or a client-facing message. The
//! single classifier here is the one source both lanes derive from, so the
//! client path and the log path cannot drift on what a 403 exposes.
//!
//! Gated on `anthropic-api` OR `bedrock` (crate-level, NOT bedrock-only) so
//! the lean anthropic-api build -- which lifts AWS error bodies on the
//! mantle lane without linking the AWS SDK -- still sees it.

use routectl_core::sanitize_for_log;

/// Shared 403-vs-other classification for an AWS/Bedrock upstream error.
/// Both the client-facing message ([`classify_client_error_message`]) and
/// the structured log line derive from this single source so the two
/// cannot drift. For a 403 it pre-extracts the IAM action (sanitized) and
/// whether a principal field is present; the log path surfaces both, the
/// client path surfaces only the action.
pub enum BedrockErrorClass {
    AccessDenied {
        action: Option<String>,
        // Read only by the native Bedrock lane's WARN emitter; the
        // anthropic-api mantle lift consumes `action` alone.
        #[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
        principal_present: bool,
    },
    Other,
}

pub fn classify_bedrock_error(status: u16, body: &str) -> BedrockErrorClass {
    if status == 403 {
        // Sanitize the extracted action since it's a substring of an
        // upstream-controlled body. AWS error messages are machine-
        // generated today, but a compromised endpoint could embed
        // control chars; defense-in-depth.
        let action = extract_iam_action(body).map(|s| sanitize_for_log(&s));
        let principal_present = body.contains("User:") || body.contains("Principal:");
        BedrockErrorClass::AccessDenied {
            action,
            principal_present,
        }
    } else {
        BedrockErrorClass::Other
    }
}

/// Build the access-denied string shared by the client-facing message
/// ([`classify_client_error_message`]) and the DEBUG body
/// ([`sanitized_debug_body`]) so the two cannot drift on the 403 arm.
///
/// The action is only surfaced if it matches an IAM `service:Action`
/// shape (`^[A-Za-z0-9._-]+:[A-Za-z0-9*]+$`). A malformed upstream 403
/// could leave ARN/account text as the post-`perform:` token; an ARN
/// (`arn:aws:...` -- multiple colons, digit-only account segments,
/// slashes) fails the shape check and falls back to the generic string
/// so no principal/account/resource identifier leaks via the action.
pub fn access_denied_message(action: Option<String>) -> String {
    match action.filter(|a| is_iam_action_shape(a)) {
        Some(action) => format!("bedrock access denied: missing IAM action {action}"),
        None => "bedrock access denied".to_string(),
    }
}

/// True if `s` matches an IAM `service:Action` shape: a service segment
/// of `[A-Za-z0-9._-]+`, a single colon, then an action segment of
/// `[A-Za-z0-9*]+`. Pure char scan (no regex dependency, matching
/// `extract_iam_action`'s rationale). An ARN fails because it has more
/// than one colon and the resource segment carries `/`.
fn is_iam_action_shape(s: &str) -> bool {
    let Some((service, action)) = s.split_once(':') else {
        return false;
    };
    !service.is_empty()
        && !action.is_empty()
        && service
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && action
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '*')
}

/// Build the body string emitted to the DEBUG `upstream error body` line.
///
/// A 403 upstream body carries the caller's principal ARN, account id, and
/// the resource ARN. None of that may reach the DEBUG log. For a 403 we
/// return an action-only string built from the shared classifier (the IAM
/// action survives as the actionable bit; ARNs and account id are dropped).
/// Every other status returns the raw body unchanged -- the shared core
/// helper sanitizes and caps it before it is logged.
pub fn sanitized_debug_body(status: u16, body: &str) -> String {
    match classify_bedrock_error(status, body) {
        BedrockErrorClass::AccessDenied { action, .. } => access_denied_message(action),
        BedrockErrorClass::Other => body.to_string(),
    }
}

/// Build the CLIENT-facing error message from an AWS/Bedrock upstream body.
/// Shares the 403-vs-other split with the log path via
/// [`classify_bedrock_error`] so the client path and log path cannot drift
/// on classification.
///
/// - **403** -> generic "bedrock access denied", optionally suffixed
///   with the extracted IAM action. NEVER the principal ARN, account
///   id, or resource ARN.
/// - **other** -> sanitized body excerpt capped at `MAX_LOG_BODY_EXCERPT`.
pub fn classify_client_error_message(status: u16, body: &str) -> String {
    match classify_bedrock_error(status, body) {
        BedrockErrorClass::AccessDenied { action, .. } => access_denied_message(action),
        BedrockErrorClass::Other => routectl_core::sanitize_upstream_body_with_cap(
            body,
            routectl_core::MAX_LOG_BODY_EXCERPT,
        ),
    }
}

/// Extract the IAM action name from an AWS error message of the form
/// `User: arn:... is not authorized to perform: bedrock-runtime:InvokeModel
/// on resource: ...`. Returns the substring between "perform: " and the
/// next whitespace. Returns None if the body doesn't match this shape
/// or the action segment is empty.
///
/// **First-match semantics**: we extract the FIRST occurrence of
/// `perform: ` in the body. AWS's error template has historically been
/// stable (the `perform: <action>` seam has held for 10+ years across
/// IAM error messages and is treated as semi-public API for IAM
/// debugging tools). If the template ever changes to embed a second
/// `perform: ` substring (e.g. inside a resource ARN), this extractor
/// would return the FIRST occurrence -- which would still be the
/// correct action for current AWS bodies but could be wrong if the
/// embedded substring appears BEFORE the canonical one. This is a
/// best-effort log field, not a contract; if the template breaks the
/// extractor returns either a stale action or `None` and the
/// surrounding 256-char `body_excerpt` log field still surfaces the
/// raw error text.
///
/// Pure string search rather than `regex` so the bedrock feature does
/// not pull regex into the binary just for this one log call.
fn extract_iam_action(body: &str) -> Option<String> {
    const NEEDLE: &str = "perform: ";
    let start = body.find(NEEDLE)? + NEEDLE.len();
    let rest = &body[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_iam_action_pulls_action_from_aws_403_body() {
        // Real-world AWS error body shape. The `perform: ` substring
        // is the stable seam.
        let body = "User: arn:aws:iam::123456789012:user/foo is not authorized to \
                    perform: bedrock-runtime:InvokeModelWithResponseStream on resource: \
                    arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5";
        assert_eq!(
            extract_iam_action(body),
            Some("bedrock-runtime:InvokeModelWithResponseStream".to_string())
        );
    }

    #[test]
    fn extract_iam_action_returns_none_for_unrelated_body() {
        assert_eq!(extract_iam_action(""), None);
        assert_eq!(extract_iam_action("Some other validation error"), None);
        // Edge case: "perform: " followed by EOF or whitespace -> None.
        assert_eq!(extract_iam_action("perform: "), None);
    }

    #[test]
    fn extract_iam_action_first_match_when_pattern_appears_twice() {
        // First-match semantics: if the AWS template ever embeds a
        // second `perform: ` (e.g. in a resource ARN), we return the
        // FIRST occurrence. Pin this so the contract is explicit.
        let body = "perform: bedrock:InvokeModel on resource: \
                    arn:aws:fake:perform: bedrock:OtherAction";
        assert_eq!(
            extract_iam_action(body),
            Some("bedrock:InvokeModel".to_string())
        );
    }

    #[test]
    fn sanitized_debug_body_403_drops_arn_and_account_keeps_action() {
        // A real 403 body carries principal ARN + account id + resource
        // ARN. The DEBUG body must surface only the IAM action.
        let body = "User: arn:aws:iam::123456789012:user/foo is not authorized to \
                    perform: bedrock-runtime:InvokeModelWithResponseStream on resource: \
                    arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5";
        let got = sanitized_debug_body(403, body);
        assert!(
            !got.contains("arn:aws:"),
            "403 debug body must not contain any ARN, got: {got}"
        );
        assert!(
            !got.contains("123456789012"),
            "403 debug body must not contain the account id, got: {got}"
        );
        assert!(
            got.contains("bedrock-runtime:InvokeModelWithResponseStream"),
            "403 debug body must keep the IAM action, got: {got}"
        );
    }

    #[test]
    fn sanitized_debug_body_non_403_passes_body_through() {
        // Non-403 statuses keep the current behavior: the raw body is
        // returned (the shared core helper sanitizes + caps it on log).
        let body = "validation error: malformed request";
        assert_eq!(sanitized_debug_body(400, body), body);
        assert_eq!(sanitized_debug_body(500, body), body);
    }

    #[test]
    fn sanitized_debug_body_403_malformed_action_falls_back_to_generic() {
        // A malformed 403 body whose post-`perform:` token is an ARN /
        // account fragment (not a `service:Action` shape) must NOT leak
        // that token into the DEBUG body via the supposedly-safe action
        // string. The shape check rejects it and falls back to the
        // generic message.
        let body = "User: x is not authorized to perform: \
                    arn:aws:iam::123456789012:role/x on resource: y";
        let got = sanitized_debug_body(403, body);
        assert!(
            !got.contains("arn:aws:"),
            "malformed-action 403 debug body must not contain any ARN, got: {got}"
        );
        assert!(
            !got.contains("123456789012"),
            "malformed-action 403 debug body must not contain the account id, got: {got}"
        );
        assert_eq!(
            got, "bedrock access denied",
            "malformed action must fall back to the generic message, got: {got}"
        );
    }

    #[test]
    fn is_iam_action_shape_accepts_service_action_rejects_arn() {
        // Well-formed IAM actions pass; anything ARN-shaped (multiple
        // colons, slashes) or empty-segmented fails.
        assert!(is_iam_action_shape("bedrock:InvokeModel"));
        assert!(is_iam_action_shape(
            "bedrock-runtime:InvokeModelWithResponseStream"
        ));
        assert!(is_iam_action_shape("bedrock:*"));
        assert!(!is_iam_action_shape("arn:aws:iam::123456789012:role/x"));
        assert!(!is_iam_action_shape("noColon"));
        assert!(!is_iam_action_shape(":InvokeModel"));
        assert!(!is_iam_action_shape("bedrock:"));
        assert!(!is_iam_action_shape("svc:action/with/slash"));
    }

    #[test]
    fn client_403_message_carries_action_not_principal_arn() {
        // A real AWS 403 body names the principal ARN, account id, and
        // resource ARN. The client-facing message must surface ONLY the
        // IAM action -- never the principal/account/resource identifiers.
        let body = "User: arn:aws:iam::123456789012:role/AppRole is not \
                    authorized to perform: bedrock-runtime:InvokeModel on \
                    resource: arn:aws:bedrock:us-east-1::foundation-model/\
                    anthropic.claude-haiku-4-5";
        let msg = classify_client_error_message(403, body);
        assert!(
            msg.contains("bedrock-runtime:InvokeModel"),
            "client message should carry the IAM action: {msg}"
        );
        assert!(
            !msg.contains("arn:aws:iam"),
            "client message leaked the principal ARN: {msg}"
        );
        assert!(
            !msg.contains("123456789012"),
            "client message leaked the account id: {msg}"
        );
        assert!(
            !msg.contains("foundation-model"),
            "client message leaked the resource ARN: {msg}"
        );
    }

    #[test]
    fn client_403_without_action_is_generic_no_arn_leak() {
        // A 403 body that doesn't match the `perform: ` template yields
        // a generic message with NO body leak.
        let body = "User: arn:aws:iam::123456789012:role/AppRole denied for \
                    some other reason";
        let msg = classify_client_error_message(403, body);
        assert!(
            !msg.contains("arn:aws:iam"),
            "generic 403 message leaked the principal ARN: {msg}"
        );
        assert!(
            !msg.contains("123456789012"),
            "generic 403 message leaked the account id: {msg}"
        );
        assert_eq!(msg, "bedrock access denied");
    }

    #[test]
    fn client_non_403_message_capped_at_max_excerpt() {
        // A non-403 oversized body is sanitized and capped so an
        // unbounded raw body never reaches the client. The cap helper
        // keeps at most MAX_LOG_BODY_EXCERPT body chars plus a short
        // fixed truncation marker -- bounded regardless of input size.
        const MARKER_LEN: usize = "... [truncated]".len();
        let oversized = routectl_core::MAX_LOG_BODY_EXCERPT * 4;
        let body = "x".repeat(oversized);
        let msg = classify_client_error_message(400, &body);
        assert!(
            msg.len() <= routectl_core::MAX_LOG_BODY_EXCERPT + MARKER_LEN,
            "non-403 client message exceeded the bounded excerpt: {} > {}",
            msg.len(),
            routectl_core::MAX_LOG_BODY_EXCERPT + MARKER_LEN
        );
        assert!(
            msg.len() < oversized,
            "non-403 client message was not truncated: {} (input {})",
            msg.len(),
            oversized
        );
    }
}
