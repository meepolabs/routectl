//! The error mapper parses an upstream error body at most once, within the
//! shared error-body cap, and carries that parse on the error it builds.

use super::map_openai_compat_upstream_error;
use crate::upstream_error::UpstreamErrorRead;
use reqwest::header::HeaderMap;
use routectl_core::test_utils::capped_parse_count;
use routectl_core::{Error, MAX_ERROR_BODY_BYTES};

const ARN: &str = "arn:aws:iam::123456789012:role/App";

/// Run `f` and count the capped error-body parses it ran. Logging is off for
/// the span: the DEBUG upstream-body line redacts its own copy of the body, a
/// log-sink cost rather than the reader's parse, and a global subscriber
/// installed by another test in this binary would otherwise enable it.
fn capped_parses_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
        let before = capped_parse_count();
        let out = f();
        (out, capped_parse_count() - before)
    })
}

fn map(status: u16, body: &str) -> (Error, usize) {
    capped_parses_during(|| {
        map_openai_compat_upstream_error(
            "p",
            UpstreamErrorRead::from_parts(status, &HeaderMap::new(), body, false),
        )
    })
}

#[test]
fn in_cap_json_bodies_parse_exactly_once() {
    let access_denied = format!(
        r#"{{"message":"User: {ARN} is not authorized to perform: bedrock-runtime:InvokeModel"}}"#
    );
    let cases: [(&str, u16, &str, bool); 3] = [
        (
            "native envelope",
            400,
            r#"{"error":{"type":"invalid_request_error","message":"bad"}}"#,
            true,
        ),
        (
            "flat mantle throttle",
            429,
            r#"{"__type":"com.amazonaws.bedrock#ThrottlingException","message":"slow"}"#,
            true,
        ),
        ("flat mantle access denied", 403, &access_denied, false),
    ];
    for (name, status, body, carries_parse) in cases {
        let (err, parses) = map(status, body);

        assert_eq!(parses, 1, "{name}");
        assert_eq!(err.parsed_body().is_some(), carries_parse, "{name}");
    }
}

#[test]
fn native_envelope_carries_its_own_parse() {
    let body = r#"{"error":{"type":"invalid_request_error","message":"bad","param":"top_k"}}"#;

    let (err, _) = map(400, body);

    let parsed = err.parsed_body().expect("envelope parse carried");
    assert_eq!(parsed.error_param(), Some("top_k"));
}

#[test]
fn access_denied_carries_the_parse_of_its_envelope_not_the_raw_aws_body() {
    let body = format!(
        r#"{{"__type":"AccessDeniedException","message":"User: {ARN} is not authorized to perform: bedrock-runtime:InvokeModel"}}"#
    );

    let (err, _) = map(403, &body);

    let carried = err
        .parsed_body()
        .expect("the built envelope carries its parse");
    assert!(!carried.value().to_string().contains("123456789012"));
    assert_eq!(
        carried.error_message(),
        Some("bedrock access denied (AccessDeniedException)")
    );
    let Error::Upstream { body, .. } = err else {
        panic!("expected Error::Upstream");
    };
    assert!(!body.contains("123456789012"), "{body}");
}

#[test]
fn over_cap_json_body_is_never_parsed() {
    let filler = "x".repeat(MAX_ERROR_BODY_BYTES);
    let body = format!(r#"{{"error":{{"type":"invalid_request_error","message":"{filler}"}}}}"#);

    let (err, parses) = map(400, &body);

    assert_eq!(parses, 0);
    assert!(err.parsed_body().is_none());
}

/// Each recorded Mantle 401 reaches this lane's error with its reason; an
/// unrecorded 401 stays reasonless.
#[test]
fn mantle_credential_rejections_carry_their_reason() {
    use crate::upstream_error::reason_tests::{MANTLE_CAPTURE, captured};
    use routectl_core::UpstreamReason::{CredentialExpired, CredentialInvalid};
    let cases = [
        ("mantle-{lane}-expired", Some(CredentialExpired)),
        (
            "mantle-{lane}-expired-security-token",
            Some(CredentialExpired),
        ),
        ("mantle-chat-invalid", Some(CredentialInvalid)),
    ];
    for (key, want) in cases {
        let key = key.replace("{lane}", "chat");
        let row = captured(MANTLE_CAPTURE, &key);

        let err = capped_parses_during(|| map_openai_compat_upstream_error("p", row.read())).0;

        assert_eq!(err.upstream_reason(), want, "{key}");
    }
    let read = UpstreamErrorRead::from_parts(
        401,
        &HeaderMap::new(),
        r#"{"error":{"code":"invalid_api_key","message":"Incorrect API key provided"}}"#,
        false,
    )
    .aws_backed(true);
    let err = capped_parses_during(|| map_openai_compat_upstream_error("p", read)).0;
    assert_eq!(err.upstream_reason(), None, "unrecorded message");
}

#[test]
fn a_403_carries_the_envelope_only_when_it_is_aws_shaped() {
    use crate::upstream_error::reason_tests::{CARRY_403_ROWS, assert_403_carry, carry_403_read};
    for row in &CARRY_403_ROWS {
        let read = carry_403_read(row.1, row.2);

        let err = capped_parses_during(|| map_openai_compat_upstream_error("p", read)).0;

        assert_403_carry("openai-compat", row, &err);
    }
}

#[test]
fn a_403_naming_a_principal_is_scrubbed_on_every_surface() {
    use crate::upstream_error::reason_tests::{
        FIRST_PARTY_403, PRINCIPAL_403_ROWS, assert_principal_scrubbed,
    };
    for row in &PRINCIPAL_403_ROWS {
        let read = UpstreamErrorRead::from_parts(403, &HeaderMap::new(), row.1, false);
        let mut err = None;

        let events = routectl_testkit::capture_events(|| {
            err = Some(map_openai_compat_upstream_error("p", read));
        });

        assert_principal_scrubbed("openai-compat", row, &err.expect("mapped"), &events);
    }
    let (err, _) = map(403, FIRST_PARTY_403);
    let Error::Upstream { body, .. } = &err else {
        panic!("expected Error::Upstream");
    };
    assert_eq!(
        body, FIRST_PARTY_403,
        "a first-party 403 is carried as read"
    );
}

#[test]
fn a_403_with_only_loose_markers_is_carried_as_read() {
    use crate::upstream_error::reason_tests::LOOSE_MARKER_403_ROWS;
    for (name, body) in LOOSE_MARKER_403_ROWS {
        let (err, _) = map(403, body);

        let Error::Upstream { body: carried, .. } = &err else {
            panic!("{name}: expected Error::Upstream");
        };
        assert_eq!(carried, body, "{name}");
    }
}
