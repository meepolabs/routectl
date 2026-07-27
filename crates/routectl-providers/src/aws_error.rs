//! Shared redaction + token lift for AWS/Bedrock upstream error envelopes.
//!
//! The native Bedrock lane (`bedrock/mod.rs`), the anthropic-api mantle lift
//! (`anthropic_api/mod.rs`), and both OpenAI readers (`openai_compat` /
//! `openai_responses`, when a `bedrock_mantle` upstream returns a flat AWS
//! envelope instead of the native error shape) all receive AWS error bodies.
//! A 403 AccessDenied body names the caller principal ARN, the account id,
//! and the resource ARN; none of that may reach a log line or a
//! client-facing message. The single classifier here is the one source every
//! lane derives from, so the client path and the log path cannot drift on
//! what a 403 exposes.
//!
//! Gated on `anthropic-api` OR `bedrock` OR either OpenAI feature
//! (crate-level, NOT bedrock-only) so every lean build that can front a
//! mantle upstream lifts + redacts AWS error bodies without linking the AWS
//! SDK.

use routectl_core::sanitize_for_log;
use serde_json::Value;

/// Lift AWS/Bedrock error-envelope tokens from an already-parsed error body,
/// used when the native error shape (Anthropic `error.type`, OpenAI
/// `error.type` / `error.code`) is absent. The AWS envelope is flat
/// (`{"__type": "...", "code": "...", "message": "..."}`) rather than the
/// nested `{"error": {...}}` the first-party APIs use, so a mantle 403/429
/// would otherwise carry no classifier token.
///
/// `__type` is frequently namespaced
/// (`"com.amazonaws.bedrock#ThrottlingException"`); the classifier and logs
/// want the bare exception name, so everything up to and including the final
/// `#` is stripped. Returns `(upstream_type, upstream_code)`: `__type`
/// (namespace-stripped) becomes the type, a top-level `code` becomes the
/// code. Best-effort -- `(None, None)` when the body was not JSON or carried
/// neither token. These are top-level keys, so the lift is inert on a native
/// nested `{"error": {...}}` body and never overrides a first-party
/// classifier.
///
/// Both tokens are surfaced verbatim to a client-facing `error.type` /
/// `error.code` past the 403 body scrub, so each is bounded at the lift
/// boundary ([`is_bounded_aws_token`]): a token carrying an ARN, account id,
/// or free-form body text (spaces, slashes, oversize) lifts as `None` and
/// never smuggles principal material through a field the scrub does not
/// touch. Enforcing it here covers every lane (native, mantle, both OpenAI
/// readers) from one place.
pub fn lift_aws_error_tokens(parsed: Option<&Value>) -> (Option<String>, Option<String>) {
    let Some(v) = parsed else {
        return (None, None);
    };
    let upstream_type = v
        .get("__type")
        .and_then(Value::as_str)
        .filter(|raw| is_bounded_aws_token(raw))
        .map(strip_aws_namespace);
    let upstream_code = v
        .get("code")
        .and_then(Value::as_str)
        .filter(|raw| is_bounded_aws_token(raw))
        .map(str::to_string);
    (upstream_type, upstream_code)
}

/// Upper bound on a lifted AWS TYPE/CODE token surfaced verbatim to a
/// client-facing error field. Real AWS exception names and codes -- even
/// fully namespaced (`com.amazonaws.bedrock#ThrottlingException`) -- sit far
/// under this; the cap only bounds an adversarial or buggy upstream.
const MAX_AWS_TOKEN_LEN: usize = 128;

/// True when `token` is safe to lift verbatim into a client-facing
/// `error.type` / `error.code`: non-empty, within the length cap, and drawn
/// from the token-shaped charset real AWS exception names and codes use
/// (ASCII alphanumeric plus the namespace punctuation `.`, `#`, and the
/// name punctuation `_`, `-`). A token failing this bound carries body text,
/// an ARN (slashes, spaces), or an oversized blob smuggled through `__type`
/// / `code`, so it lifts as `None` and never reaches a client-facing field
/// past the 403 scrub. Applied to the RAW value (before namespace strip) so
/// the legitimate namespaced form still passes.
fn is_bounded_aws_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_AWS_TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'#' | b'_' | b'-'))
}

/// Strip an AWS namespace prefix from an exception token:
/// `"com.amazonaws.bedrock#ThrottlingException"` -> `"ThrottlingException"`.
/// A token with no `#` is returned unchanged.
fn strip_aws_namespace(raw: &str) -> String {
    raw.rsplit_once('#')
        .map_or(raw, |(_, bare)| bare)
        .to_string()
}

/// Whether a request-fault (400/422) body may be carried RAW (byte-capped)
/// into [`routectl_core::Error::Upstream`]'s `body` for the capability
/// matcher to re-parse, versus collapsed to the short sanitized excerpt.
///
/// True ONLY for the flat AWS envelope shape the matcher reads -- a
/// top-level `__type` (via [`lift_aws_error_tokens`]) and/or a top-level
/// string `message` -- AND when the whole body is within `max_bytes`. Any
/// other 400/422 shape returns false: a NESTED `{"error":{"message":...}}`
/// body (a reverse proxy or custom endpoint fronting Bedrock), an HTML
/// page, or plain text has no top-level `message` / `__type`, so it stays
/// on the short excerpt and the widened body cap can never reflect an
/// arbitrary large or nested-shaped body up to the caller via the ingress
/// `error.message` reader.
///
/// The `max_bytes` gate bounds BOTH the JSON parse this performs on the
/// routing path (the matcher's own cap prevents a large parse; this
/// mirrors it) AND the carried `message` -- which is a substring of the
/// body and so is itself bounded once the body is. Reuses
/// [`lift_aws_error_tokens`] for the `__type` detection rather than
/// hand-rolling a second envelope parser.
///
/// A genuine flat AWS error envelope carries only SCALAR top-level fields
/// (`__type` / `code` / `message` strings). Any container-valued top-level
/// field is rejected, so a wrapper such as
/// `{"message":"x","error":{"message":"<large>"}}` -- which would otherwise
/// pass the top-level-`message` check yet smuggle a large nested body into
/// the raw stored envelope -- stays on the short excerpt.
#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
pub fn is_carryable_flat_envelope(body: &str, max_bytes: usize) -> bool {
    if body.len() > max_bytes {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.values().any(|v| v.is_object() || v.is_array()) {
        return false;
    }
    let has_message = obj.get("message").and_then(Value::as_str).is_some();
    let (upstream_type, _) = lift_aws_error_tokens(Some(&value));
    upstream_type.is_some() || has_message
}

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

    /// `strip_aws_namespace` reduces a namespaced AWS exception token to its
    /// bare name and leaves an already-bare token untouched.
    #[test]
    fn strip_aws_namespace_reduces_to_bare_token() {
        assert_eq!(
            strip_aws_namespace("com.amazonaws.bedrock#ThrottlingException"),
            "ThrottlingException"
        );
        assert_eq!(
            strip_aws_namespace("SignatureDoesNotMatch"),
            "SignatureDoesNotMatch"
        );
        // Only the final `#` splits, and an empty bare token stays empty.
        assert_eq!(strip_aws_namespace("a#b#Trailing"), "Trailing");
        assert_eq!(strip_aws_namespace("prefix#"), "");
    }

    /// The AWS envelope lift reads `__type` (namespace-stripped) and a
    /// top-level `code`, and is inert on native-shaped or tokenless JSON.
    #[test]
    fn lift_aws_error_tokens_lifts_type_and_code() {
        let namespaced = serde_json::from_str::<Value>(
            r#"{"__type":"com.amazonaws.bedrock#ThrottlingException"}"#,
        )
        .ok();
        assert_eq!(
            lift_aws_error_tokens(namespaced.as_ref()),
            (Some("ThrottlingException".to_string()), None)
        );

        let with_code = serde_json::from_str::<Value>(r#"{"code":"SignatureDoesNotMatch"}"#).ok();
        assert_eq!(
            lift_aws_error_tokens(with_code.as_ref()),
            (None, Some("SignatureDoesNotMatch".to_string()))
        );

        // A JSON body carrying neither token yields no lift.
        let tokenless = serde_json::from_str::<Value>(r#"{"ok":true}"#).ok();
        assert_eq!(lift_aws_error_tokens(tokenless.as_ref()), (None, None));
        // A non-JSON body yields no lift.
        assert_eq!(lift_aws_error_tokens(None), (None, None));
    }

    /// The lift reads TOP-LEVEL keys, so a native nested `{"error": {...}}`
    /// envelope (Anthropic or OpenAI shape) never triggers it -- the
    /// first-party classifier always wins.
    #[test]
    fn lift_aws_error_tokens_is_inert_on_native_nested_shape() {
        let native = serde_json::from_str::<Value>(
            r#"{"error":{"type":"rate_limit_exceeded","code":"slow_down"}}"#,
        )
        .ok();
        assert_eq!(lift_aws_error_tokens(native.as_ref()), (None, None));
    }

    /// An ARN/principal-bearing `__type` (spaces, slashes, colons from a
    /// malicious or buggy upstream) fails the token bound and lifts as
    /// `None`, so it can never smuggle principal material into a
    /// client-facing `error.type` past the 403 scrub.
    #[test]
    fn lift_drops_arn_bearing_type_token() {
        let arn_type = serde_json::from_str::<Value>(
            r#"{"__type":"User: arn:aws:iam::123456789012:role/x is not authorized"}"#,
        )
        .ok();
        assert_eq!(lift_aws_error_tokens(arn_type.as_ref()), (None, None));
    }

    /// A code carrying ARN/account text (slashes) is dropped while a normal
    /// namespaced type on the same body still lifts stripped -- the bound is
    /// per-token, not all-or-nothing.
    #[test]
    fn lift_drops_arn_bearing_code_but_keeps_clean_type() {
        let mixed = serde_json::from_str::<Value>(
            r#"{"__type":"com.amazonaws.bedrock#ValidationException","code":"arn:aws:iam::123456789012:role/x"}"#,
        )
        .ok();
        assert_eq!(
            lift_aws_error_tokens(mixed.as_ref()),
            (Some("ValidationException".to_string()), None)
        );
    }

    /// A token past the length cap lifts as `None` -- an oversized blob never
    /// reaches a client-facing error field even if its charset is clean.
    #[test]
    fn lift_drops_oversized_token() {
        let oversized = "a".repeat(MAX_AWS_TOKEN_LEN + 1);
        let body = serde_json::from_str::<Value>(&format!(r#"{{"code":"{oversized}"}}"#)).ok();
        assert_eq!(lift_aws_error_tokens(body.as_ref()), (None, None));
    }

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

    /// A flat AWS envelope with a top-level `__type` and/or `message`, within
    /// the byte ceiling, is carryable -- the shape the capability matcher
    /// re-parses.
    #[test]
    fn carryable_accepts_flat_aws_envelope() {
        let with_both =
            r#"{"__type":"com.amazon.coral.validate#ValidationException","message":"bad field"}"#;
        assert!(is_carryable_flat_envelope(
            with_both,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
        // Top-level message alone (no __type) still counts as the flat shape.
        let message_only = r#"{"message":"bad field"}"#;
        assert!(is_carryable_flat_envelope(
            message_only,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
        // __type alone (a tokenless throttle-style body) still counts.
        let type_only = r#"{"__type":"com.amazonaws.bedrock#ThrottlingException"}"#;
        assert!(is_carryable_flat_envelope(
            type_only,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
    }

    /// A NESTED `{"error":{"message":...}}` envelope (proxy / custom endpoint),
    /// an HTML page, plain text, and a non-object body are all NOT carryable:
    /// they have no top-level `message` / `__type`, so they keep the short
    /// excerpt and cannot reflect a large body to the caller.
    #[test]
    fn carryable_rejects_non_flat_shapes() {
        let nested = r#"{"error":{"message":"leaked detail"}}"#;
        assert!(!is_carryable_flat_envelope(
            nested,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
        let html = "<!DOCTYPE html><html>error</html>";
        assert!(!is_carryable_flat_envelope(
            html,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
        let plain = "gateway timeout";
        assert!(!is_carryable_flat_envelope(
            plain,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
        let array = r#"["message","__type"]"#;
        assert!(!is_carryable_flat_envelope(
            array,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
    }

    /// A wrapper carrying a top-level string `message` AND a container-valued
    /// field (a nested `error` object smuggling a large body) is NOT
    /// carryable: only genuinely flat scalar-field envelopes pass, so the
    /// top-level-`message` check cannot be tricked into storing a large
    /// nested body raw.
    #[test]
    fn carryable_rejects_wrapper_with_container_field() {
        let wrapper = r#"{"message":"x","error":{"message":"large nested body"}}"#;
        assert!(!is_carryable_flat_envelope(
            wrapper,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
        // A flat envelope whose `message` is itself an OBJECT (not a string)
        // is likewise rejected.
        let object_message = r#"{"__type":"ValidationException","message":{"detail":"x"}}"#;
        assert!(!is_carryable_flat_envelope(
            object_message,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
    }

    /// A flat envelope whose whole body runs past the byte ceiling is NOT
    /// carryable: the ceiling bounds both the routing-path parse and the
    /// carried message (a substring of the body).
    #[test]
    fn carryable_rejects_over_ceiling_body() {
        let oversized = "x".repeat(routectl_core::MAX_ERROR_BODY_BYTES);
        let body = format!(r#"{{"__type":"ValidationException","message":"{oversized}"}}"#);
        assert!(body.len() > routectl_core::MAX_ERROR_BODY_BYTES);
        assert!(!is_carryable_flat_envelope(
            &body,
            routectl_core::MAX_ERROR_BODY_BYTES
        ));
    }
}
