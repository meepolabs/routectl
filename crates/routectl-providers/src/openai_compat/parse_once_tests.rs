//! The error mapper parses an upstream error body at most once, within the
//! shared error-body cap, and carries that parse on the error it builds.

use super::map_openai_compat_upstream_error;
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
        map_openai_compat_upstream_error("p", status, &HeaderMap::new(), body, false)
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
fn access_denied_carries_no_parse_of_the_raw_aws_body() {
    let body = format!(
        r#"{{"__type":"AccessDeniedException","message":"User: {ARN} is not authorized to perform: bedrock-runtime:InvokeModel"}}"#
    );

    let (err, _) = map(403, &body);

    assert!(err.parsed_body().is_none());
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
