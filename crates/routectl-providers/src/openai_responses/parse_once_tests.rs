//! The error mapper parses an upstream error body at most once, within the
//! shared error-body cap, and carries that parse on the error it builds.

use super::{AuthKind, map_responses_upstream_error};
use crate::upstream_error::UpstreamErrorRead;
use reqwest::header::HeaderMap;
use routectl_core::test_utils::capped_parse_count;
use routectl_core::{Error, MAX_ERROR_BODY_BYTES};

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
        map_responses_upstream_error(
            "p",
            &AuthKind::ChatgptOauth,
            UpstreamErrorRead::from_parts(status, &HeaderMap::new(), body, false),
        )
    })
}

#[test]
fn in_cap_json_bodies_parse_exactly_once() {
    let cases: [(&str, u16, &str, bool); 3] = [
        (
            "codex usage limit",
            429,
            r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":7200,"message":"capped"}}"#,
            true,
        ),
        (
            "flat mantle throttle",
            429,
            r#"{"__type":"com.amazonaws.bedrock#ThrottlingException","message":"slow"}"#,
            true,
        ),
        (
            "flat mantle access denied",
            403,
            r#"{"message":"User: arn:aws:iam::123456789012:role/App is not authorized to perform: bedrock-runtime:InvokeModel"}"#,
            false,
        ),
    ];
    for (name, status, body, carries_parse) in cases {
        let (err, parses) = map(status, body);

        assert_eq!(parses, 1, "{name}");
        assert_eq!(err.parsed_body().is_some(), carries_parse, "{name}");
    }
}

#[test]
fn codex_reset_hint_reads_the_single_parse() {
    let body =
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":7200,"message":"capped"}}"#;

    let (err, _) = map(429, body);

    let Error::Upstream { retry_after, .. } = err else {
        panic!("expected Error::Upstream");
    };
    assert_eq!(retry_after, Some(std::time::Duration::from_hours(2)));
}

#[test]
fn over_cap_json_body_is_never_parsed() {
    let filler = "x".repeat(MAX_ERROR_BODY_BYTES);
    let body = format!(r#"{{"error":{{"type":"usage_limit_reached","message":"{filler}"}}}}"#);

    let (err, parses) = map(429, &body);

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
        let key = key.replace("{lane}", "responses");
        let row = captured(MANTLE_CAPTURE, &key);

        let err = capped_parses_during(|| {
            map_responses_upstream_error("p", &AuthKind::ChatgptOauth, row.read())
        })
        .0;

        assert_eq!(err.upstream_reason(), want, "{key}");
    }
    let read = UpstreamErrorRead::from_parts(
        401,
        &HeaderMap::new(),
        r#"{"error":{"code":"invalid_api_key","message":"Incorrect API key provided"}}"#,
        false,
    )
    .aws_backed(true);
    let err =
        capped_parses_during(|| map_responses_upstream_error("p", &AuthKind::ChatgptOauth, read)).0;
    assert_eq!(err.upstream_reason(), None, "unrecorded message");
}

#[test]
fn a_403_carries_the_envelope_only_when_it_is_aws_shaped() {
    use crate::upstream_error::reason_tests::{CARRY_403_ROWS, assert_403_carry, carry_403_read};
    for row in &CARRY_403_ROWS {
        let read = carry_403_read(row.1, row.2);

        let err = capped_parses_during(|| {
            map_responses_upstream_error("p", &AuthKind::ChatgptOauth, read)
        })
        .0;

        assert_403_carry("openai-responses", row, &err);
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
            err = Some(map_responses_upstream_error("p", &AuthKind::ApiKey, read));
        });

        assert_principal_scrubbed("openai-responses", row, &err.expect("mapped"), &events);
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
