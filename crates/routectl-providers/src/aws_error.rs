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

use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::header::HeaderMap;
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
        .map(|raw| strip_aws_namespace(raw).to_string());
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
///
/// The canonical reduction every `__type` / `x-amzn-errortype` consumer runs
/// before comparing against a bare exception name. Crate-private: downstream
/// crates gate through [`aws_exception_type_is`] rather than reducing by hand.
fn strip_aws_namespace(raw: &str) -> &str {
    raw.rsplit_once('#').map_or(raw, |(_, bare)| bare)
}

/// True when `raw` and `expected` name the same AWS exception, whichever of
/// them arrives bare and whichever namespaced
/// (`com.amazon.coral.validate#ValidationException`).
///
/// The canonical comparison for any consumer gating on an AWS exception
/// discriminator: an exact `==` against a bare name silently misses the
/// namespaced wire form, and a `contains` match accepts an unrelated
/// exception whose name merely embeds the target. BOTH operands are
/// namespace-reduced, so a caller holding a namespaced constant gets the same
/// verdict as one holding the bare name.
pub fn aws_exception_type_is(raw: &str, expected: &str) -> bool {
    strip_aws_namespace(raw) == strip_aws_namespace(expected)
}

/// The bare AWS exception name a Bedrock request-validation 400 carries in
/// its discriminator (body `__type` or the `x-amzn-errortype` header). Single
/// source of truth for every consumer gating on a validation rejection, so
/// the token cannot drift between the provider lift, the capability matcher,
/// and the envelope-capture harness.
pub const VALIDATION_EXCEPTION_TYPE: &str = "ValidationException";

/// The response header AWS/Bedrock uses to carry the exception
/// discriminator when the error body is the flat minimal `{"message":...}`
/// shape that omits the `__type` key. Compared case-insensitively by
/// [`HeaderMap`], so the lowercase form matches the wire's mixed case.
const AWS_ERROR_TYPE_HEADER: &str = "x-amzn-errortype";

/// Outcome of lifting the AWS exception discriminator from response
/// headers. The unusable variants are reason-labeled so the native lane
/// can surface a bounded WARN when the header fallback yields no
/// discriminator -- a stripped, duplicated, or garbled value stays
/// visible instead of silently degrading to a non-attributed rejection.
#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
pub enum ErrorTypeHeaderLift {
    Lifted(String),
    Missing,
    Invalid,
    Ambiguous,
    Conflict,
}

impl ErrorTypeHeaderLift {
    /// The bounded reason label for an unusable lift, or `None` when a
    /// token was lifted. Only this fixed label ever reaches a log line --
    /// never the raw header value. Shares its label set with the bounded
    /// counter slots ([`ERROR_TYPE_HEADER_UNUSABLE_REASONS`]) so the WARN
    /// label and the counter key cannot drift.
    #[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
    pub const fn unusable_reason(&self) -> Option<&'static str> {
        match self {
            Self::Lifted(_) => None,
            Self::Missing => Some(ERROR_TYPE_HEADER_UNUSABLE_REASONS[0]),
            Self::Invalid => Some(ERROR_TYPE_HEADER_UNUSABLE_REASONS[1]),
            Self::Ambiguous => Some(ERROR_TYPE_HEADER_UNUSABLE_REASONS[2]),
            Self::Conflict => Some(ERROR_TYPE_HEADER_UNUSABLE_REASONS[3]),
        }
    }
}

/// The fixed reason labels for an unusable `x-amzn-errortype` lift, in the
/// same slot order as [`ERROR_TYPE_HEADER_UNUSABLE_COUNTS`]. Single source
/// of truth for both the WARN label and the counter key.
#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
const ERROR_TYPE_HEADER_UNUSABLE_REASONS: [&str; 4] =
    ["missing", "invalid", "ambiguous", "conflict"];

/// Bounded fixed-cardinality counters for the unusable-header path, one slot
/// per reason label above. The reason-labeled WARN alone leaves the RATE
/// invisible; a rising slot means a stripped, garbled, duplicated, or
/// proxy-merged discriminator is arriving on the native lane and the
/// capability matcher is losing its gate token -- visible drift instead of a
/// silent non-attributed rejection. Keyed ONLY by the fixed labels (no
/// unbounded dimension); mirrors the router's
/// `bedrock_validation_unmatched_total`.
#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
static ERROR_TYPE_HEADER_UNUSABLE_COUNTS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
fn error_type_header_unusable_index(reason: &str) -> Option<usize> {
    ERROR_TYPE_HEADER_UNUSABLE_REASONS
        .iter()
        .position(|label| *label == reason)
}

/// Bump the bounded counter slot for an unusable-header reason label,
/// incremented alongside the native lane's reason-labeled WARN. A no-op for
/// any label outside the fixed set (the lift never produces one).
#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
pub fn incr_error_type_header_unusable(reason: &str) {
    if let Some(index) = error_type_header_unusable_index(reason) {
        ERROR_TYPE_HEADER_UNUSABLE_COUNTS[index].fetch_add(1, Ordering::Relaxed);
    }
}

/// Read the cumulative unusable-header count for `reason`, or 0 for an
/// unknown label. Test-visible accessor mirroring the router metrics readers.
#[cfg(test)]
pub fn error_type_header_unusable_count(reason: &str) -> u64 {
    error_type_header_unusable_index(reason).map_or(0, |index| {
        ERROR_TYPE_HEADER_UNUSABLE_COUNTS[index].load(Ordering::Relaxed)
    })
}

/// Classify the `x-amzn-errortype` response header into a lifted
/// discriminator or a reason-labeled failure. A single unambiguous value
/// is REQUIRED: zero entries are `Missing`, repeated identical entries are
/// `Ambiguous`, and differing entries are `Conflict` -- all fail closed so
/// a tampered or proxy-merged header can never seed a false classifier.
///
/// The single value is split at the FIRST `:` (the delimiter before the
/// coral URL tail the wire appends, distinct from the body lift's `#`
/// namespace delimiter); the head is validated through the same bounded
/// token path as the body lift ([`is_bounded_aws_token`] +
/// [`strip_aws_namespace`]) so only a bounded exception name -- never the
/// URL tail, an oversized blob, or non-token bytes -- survives.
#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
pub fn classify_aws_error_type_header(headers: &HeaderMap) -> ErrorTypeHeaderLift {
    let mut values = headers.get_all(AWS_ERROR_TYPE_HEADER).iter();
    let Some(first) = values.next() else {
        return ErrorTypeHeaderLift::Missing;
    };
    let Ok(first_str) = first.to_str() else {
        return ErrorTypeHeaderLift::Invalid;
    };
    let mut duplicate = false;
    for other in values {
        match other.to_str() {
            Ok(s) if s == first_str => duplicate = true,
            // A differing (or non-UTF8, hence non-comparable) second value
            // makes the discriminator ambiguous in intent: fail closed.
            _ => return ErrorTypeHeaderLift::Conflict,
        }
    }
    if duplicate {
        return ErrorTypeHeaderLift::Ambiguous;
    }
    let head = first_str
        .split_once(':')
        .map_or(first_str, |(head, _)| head);
    if is_bounded_aws_token(head) {
        ErrorTypeHeaderLift::Lifted(strip_aws_namespace(head).to_string())
    } else {
        ErrorTypeHeaderLift::Invalid
    }
}

/// Lift the bare AWS exception name from the `x-amzn-errortype` response
/// header, or `None` when the header is absent, duplicated, conflicting,
/// or malformed. Thin wrapper over [`classify_aws_error_type_header`] that
/// discards the failure reason; the native lane uses the classifier
/// directly to label its WARN. Separate from [`lift_aws_error_tokens`]:
/// different input (headers, not a parsed body), different split char
/// (`:` before the URL tail, not `#`), different provenance.
#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
pub fn lift_aws_error_type_from_headers(headers: &HeaderMap) -> Option<String> {
    match classify_aws_error_type_header(headers) {
        ErrorTypeHeaderLift::Lifted(token) => Some(token),
        _ => None,
    }
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

    /// `aws_exception_type_is` matches the NAMESPACED and the bare form of a
    /// discriminator identically, and refuses an unrelated exception whose
    /// name merely embeds the target (what a `contains` match would accept).
    #[test]
    fn aws_exception_type_is_matches_namespaced_and_bare_identically() {
        for raw in [
            VALIDATION_EXCEPTION_TYPE,
            "com.amazon.coral.service#ValidationException",
            "com.amazon.coral.validate#ValidationException",
        ] {
            assert!(
                aws_exception_type_is(raw, VALIDATION_EXCEPTION_TYPE),
                "`{raw}` must match the bare token"
            );
        }
        for raw in [
            "ThrottlingException",
            "com.amazon.coral.service#ThrottlingException",
            "PreValidationExceptionWrapper",
            "com.amazon.coral.service#SubValidationException",
        ] {
            assert!(
                !aws_exception_type_is(raw, VALIDATION_EXCEPTION_TYPE),
                "`{raw}` must not match the bare token"
            );
        }
    }

    /// The comparison is symmetric: all four bare/namespaced combinations of
    /// the two operands agree, so a caller holding a NAMESPACED constant gets
    /// the same verdict as one holding the bare name. A name that merely
    /// embeds the token is still rejected in every combination.
    #[test]
    fn aws_exception_type_is_normalizes_both_operands() {
        const BARE: &str = VALIDATION_EXCEPTION_TYPE;
        const NAMESPACED: &str = "com.amazon.coral.validate#ValidationException";
        for (raw, expected) in [
            (BARE, BARE),
            (BARE, NAMESPACED),
            (NAMESPACED, BARE),
            (NAMESPACED, NAMESPACED),
        ] {
            assert!(
                aws_exception_type_is(raw, expected),
                "`{raw}` vs `{expected}` must match in every bare/namespaced combination"
            );
        }

        const EMBEDDING_BARE: &str = "PreValidationExceptionWrapper";
        const EMBEDDING_NAMESPACED: &str = "com.amazon.coral.service#SubValidationException";
        for (raw, expected) in [
            (EMBEDDING_BARE, BARE),
            (EMBEDDING_BARE, NAMESPACED),
            (EMBEDDING_NAMESPACED, BARE),
            (EMBEDDING_NAMESPACED, NAMESPACED),
            (BARE, EMBEDDING_BARE),
            (NAMESPACED, EMBEDDING_NAMESPACED),
        ] {
            assert!(
                !aws_exception_type_is(raw, expected),
                "`{raw}` vs `{expected}` merely embeds the token and must not match"
            );
        }
    }

    /// A namespaced `__type` lifts to exactly the token a bare `__type`
    /// lifts to, so the provider lift and any downstream discriminator gate
    /// agree on one form.
    #[test]
    fn lift_of_namespaced_type_equals_lift_of_bare_type() {
        let namespaced = serde_json::from_str::<Value>(
            r#"{"__type":"com.amazon.coral.service#ThrottlingException"}"#,
        )
        .ok();
        let bare = serde_json::from_str::<Value>(r#"{"__type":"ThrottlingException"}"#).ok();
        assert_eq!(
            lift_aws_error_tokens(namespaced.as_ref()),
            lift_aws_error_tokens(bare.as_ref())
        );
    }

    /// The header lift reduces a namespaced discriminator to the same token a
    /// bare one yields, across both the plain and the coral-URL-tailed form.
    #[test]
    fn header_lift_of_namespaced_type_equals_lift_of_bare_type() {
        let bare = lift_aws_error_type_from_headers(&headers_with_error_type(&[
            VALIDATION_EXCEPTION_TYPE,
        ]));
        assert_eq!(bare.as_deref(), Some(VALIDATION_EXCEPTION_TYPE));
        for raw in [
            "com.amazon.coral.service#ValidationException",
            "com.amazon.coral.service#ValidationException:http://internal.example/coral/",
        ] {
            assert_eq!(
                lift_aws_error_type_from_headers(&headers_with_error_type(&[raw])),
                bare,
                "header `{raw}` must lift to the bare token"
            );
        }
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

    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    const ERROR_TYPE_HEADER_NAME: &str = "x-amzn-errortype";

    /// The exact value bedrock-runtime serves on a 400: the bare exception
    /// name followed by `:` and the coral URL tail. The URL tail must never
    /// survive the lift.
    const REAL_ERROR_TYPE_VALUE: &str =
        "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/";

    fn headers_with_error_type(values: &[&str]) -> HeaderMap {
        let name = HeaderName::from_static(ERROR_TYPE_HEADER_NAME);
        let mut headers = HeaderMap::new();
        for v in values {
            headers.append(name.clone(), HeaderValue::from_str(v).unwrap());
        }
        headers
    }

    /// The real captured header value lifts to the bare, namespace-stripped
    /// exception name -- the coral URL tail past the first `:` is dropped.
    #[test]
    fn header_lift_reads_real_captured_value() {
        let headers = headers_with_error_type(&[REAL_ERROR_TYPE_VALUE]);
        assert_eq!(
            lift_aws_error_type_from_headers(&headers),
            Some("ValidationException".to_string())
        );
    }

    /// A bare exception name with no `:` tail lifts unchanged, and a value
    /// whose head is namespaced (`...#Name:url`) lifts the stripped name.
    #[test]
    fn header_lift_handles_bare_and_namespaced_heads() {
        let bare = headers_with_error_type(&["ThrottlingException"]);
        assert_eq!(
            lift_aws_error_type_from_headers(&bare),
            Some("ThrottlingException".to_string())
        );
        let namespaced = headers_with_error_type(&[
            "com.amazon.coral.validate#ValidationException:http://internal.example/coral/",
        ]);
        assert_eq!(
            lift_aws_error_type_from_headers(&namespaced),
            Some("ValidationException".to_string())
        );
    }

    /// Repeated identical entries are ambiguous, and differing entries
    /// conflict -- both fail closed with distinct reason labels so a
    /// duplicated or proxy-merged header can never seed a false classifier.
    #[test]
    fn header_lift_fails_closed_on_duplicate_and_conflicting_values() {
        let duplicate = headers_with_error_type(&[REAL_ERROR_TYPE_VALUE, REAL_ERROR_TYPE_VALUE]);
        assert_eq!(lift_aws_error_type_from_headers(&duplicate), None);
        assert_eq!(
            classify_aws_error_type_header(&duplicate).unusable_reason(),
            Some("ambiguous")
        );
        let conflicting =
            headers_with_error_type(&["ValidationException:x", "ThrottlingException:y"]);
        assert_eq!(lift_aws_error_type_from_headers(&conflicting), None);
        assert_eq!(
            classify_aws_error_type_header(&conflicting).unusable_reason(),
            Some("conflict")
        );
    }

    /// A non-UTF8 value, an empty value, and a URL-tail-only value (a leading
    /// `:` leaving an empty head) all lift as `None` with the `invalid`
    /// reason. An absent header is `missing`.
    #[test]
    fn header_lift_fails_closed_on_malformed_and_missing() {
        let name = HeaderName::from_static(ERROR_TYPE_HEADER_NAME);
        let mut non_utf8 = HeaderMap::new();
        non_utf8.append(
            name.clone(),
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert_eq!(lift_aws_error_type_from_headers(&non_utf8), None);
        assert_eq!(
            classify_aws_error_type_header(&non_utf8).unusable_reason(),
            Some("invalid")
        );

        let empty = headers_with_error_type(&[""]);
        assert_eq!(lift_aws_error_type_from_headers(&empty), None);
        assert_eq!(
            classify_aws_error_type_header(&empty).unusable_reason(),
            Some("invalid")
        );

        let url_tail_only =
            headers_with_error_type(&[":http://internal.amazon.com/coral/com.amazon.bedrock/"]);
        assert_eq!(lift_aws_error_type_from_headers(&url_tail_only), None);
        assert_eq!(
            classify_aws_error_type_header(&url_tail_only).unusable_reason(),
            Some("invalid")
        );

        let absent = HeaderMap::new();
        assert_eq!(lift_aws_error_type_from_headers(&absent), None);
        assert_eq!(
            classify_aws_error_type_header(&absent).unusable_reason(),
            Some("missing")
        );
    }

    /// Each fixed reason label owns a distinct bounded counter slot: bumping
    /// one advances only that slot. An unknown label is a no-op that reads
    /// back 0.
    #[test]
    fn unusable_header_counter_bumps_each_reason_slot() {
        for reason in ["missing", "invalid", "ambiguous", "conflict"] {
            let before = error_type_header_unusable_count(reason);
            incr_error_type_header_unusable(reason);
            assert_eq!(
                error_type_header_unusable_count(reason),
                before + 1,
                "reason {reason} slot must advance by exactly one",
            );
        }
        // An out-of-set label never allocates a slot and reads back 0.
        incr_error_type_header_unusable("bogus");
        assert_eq!(error_type_header_unusable_count("bogus"), 0);
    }
}
