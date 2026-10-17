//! The anthropic-api error reader parses the upstream body once, within the
//! core parse cap, and the error carries the parse of the text it carries.
//!
//! The parse counter is thread-local, so each count is taken around the
//! synchronous half of the reader, with no subscriber on the thread: the
//! DEBUG body line formats its own redacted copy only when DEBUG is enabled.

use super::*;
use routectl_core::MAX_ERROR_BODY_BYTES;
use routectl_core::UpstreamBody;
use routectl_core::test_utils::capped_parse_count;

const ARN_SENTINEL: &str = "arn:aws:iam::999988887777:role/SentinelRole";

fn parses_during<T>(run: impl FnOnce() -> T) -> (T, usize) {
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
        let before = capped_parse_count();
        let out = run();
        (out, capped_parse_count() - before)
    })
}

fn read(status: u16, body: String) -> (String, Error) {
    let read = crate::upstream_error::UpstreamErrorRead::from_parts(
        status,
        &reqwest::header::HeaderMap::new(),
        body,
        false,
    );
    anthropic_error_from_read("anthropic-parse-once", read)
}

fn native_envelope(padding: usize) -> String {
    serde_json::json!({
        "type": "error",
        "error": { "type": "invalid_request_error", "message": "max_tokens too large" },
        "padding": "x".repeat(padding),
    })
    .to_string()
}

#[test]
fn in_cap_native_envelope_is_parsed_once_and_carried() {
    let body = native_envelope(0);

    let ((msg, err), parses) = parses_during(|| read(400, body.clone()));

    assert_eq!(parses, 1);
    assert_eq!(msg, "max_tokens too large");
    let carried = err
        .parsed_body()
        .expect("in-cap envelope carries its parse");
    assert_eq!(carried.error_type(), Some("invalid_request_error"));
    let Error::Upstream {
        body: carried_text,
        upstream_type,
        ..
    } = &err
    else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(carried_text, &body);
    assert_eq!(upstream_type.as_deref(), Some("invalid_request_error"));
}

#[test]
fn over_cap_native_envelope_is_never_parsed() {
    let body = native_envelope(MAX_ERROR_BODY_BYTES);
    assert!(body.len() > MAX_ERROR_BODY_BYTES);

    let ((_msg, err), parses) = parses_during(|| read(400, body));

    assert_eq!(parses, 0);
    assert!(err.parsed_body().is_none());
    let Error::Upstream {
        body: carried_text,
        upstream_type,
        ..
    } = &err
    else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(upstream_type, &None);
    assert!(
        !carried_text.contains("max_tokens too large"),
        "an over-cap body must not be carried raw: {} bytes",
        carried_text.len()
    );
}

#[test]
fn aws_lift_403_carries_its_envelope_not_the_raw_principal_body() {
    let raw = serde_json::json!({
        "__type": "com.amazonaws.bedrock#AccessDeniedException",
        "message": format!(
            "User: {ARN_SENTINEL} is not authorized to perform: bedrock:InvokeModel on resource: {ARN_SENTINEL}"
        ),
    })
    .to_string();
    let raw_parse = UpstreamBody::new(raw.clone());
    assert!(
        raw_parse
            .parsed()
            .is_some_and(|p| p.value().to_string().contains(ARN_SENTINEL)),
        "the raw body's parse must hold the sentinel for this check to bite"
    );

    let ((msg, err), parses) = parses_during(|| read(403, raw));

    assert_eq!(parses, 1);
    assert_eq!(
        msg,
        "bedrock access denied (AccessDeniedException): missing IAM action bedrock:InvokeModel"
    );
    let Error::Upstream {
        body: carried_text,
        upstream_type,
        ..
    } = &err
    else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(
        err.parsed_body().and_then(ParsedErrorBody::error_message),
        Some(msg.as_str())
    );
    assert!(!carried_text.contains(ARN_SENTINEL), "{carried_text}");
    assert_eq!(upstream_type.as_deref(), Some("AccessDeniedException"));
    let carried_parse = err.parsed_body().map(|p| p.value().to_string());
    assert!(
        !carried_parse
            .as_deref()
            .is_some_and(|v| v.contains(ARN_SENTINEL)),
        "the carried parse leaked the principal ARN: {carried_parse:?}"
    );
}

/// Each recorded Mantle Anthropic 401 reaches this lane's error with its
/// reason; an unrecorded 401 stays reasonless.
#[test]
fn mantle_credential_rejections_carry_their_reason() {
    use crate::upstream_error::reason_tests::{MANTLE_CAPTURE, captured};
    use routectl_core::UpstreamReason::{CredentialExpired, CredentialInvalid};
    let cases = [
        ("mantle-anthropic-expired", Some(CredentialExpired)),
        (
            "mantle-anthropic-expired-security-token",
            Some(CredentialExpired),
        ),
        ("mantle-anthropic-invalid", Some(CredentialInvalid)),
    ];
    for (key, want) in cases {
        let row = captured(MANTLE_CAPTURE, key);

        let (_, err) =
            parses_during(|| anthropic_error_from_read("anthropic-parse-once", row.read())).0;

        assert_eq!(err.upstream_reason(), want, "{key}");
    }
    let body =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let unrecorded = crate::upstream_error::UpstreamErrorRead::from_parts(
        401,
        &reqwest::header::HeaderMap::new(),
        body,
        false,
    )
    .aws_backed(true);
    let (_, err) =
        parses_during(|| anthropic_error_from_read("anthropic-parse-once", unrecorded)).0;
    assert_eq!(err.upstream_reason(), None, "unrecorded message");
}

#[test]
fn a_403_carries_the_envelope_only_when_it_is_aws_shaped() {
    use crate::upstream_error::reason_tests::{
        CARRY_403_ROWS, PLAIN_403_TEXT, assert_403_carry, carry_403_read,
    };
    for row in &CARRY_403_ROWS {
        let read = carry_403_read(row.1, row.2);

        let (msg, err) =
            parses_during(|| anthropic_error_from_read("anthropic-parse-once", read)).0;

        assert_403_carry("anthropic-api", row, &err);
        assert_eq!(
            msg,
            row.3.unwrap_or(PLAIN_403_TEXT),
            "anthropic-api {}",
            row.0
        );
    }
}
