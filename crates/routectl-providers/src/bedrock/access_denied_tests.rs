//! The native lane on a 403: the recorded credential rejections carry their
//! reason, and the client message, the DEBUG body line and the WARN excerpt
//! all render from the one access-denied function without a principal ARN,
//! account id or `User:` field.

use super::*;
use crate::upstream_error::reason_tests::{
    CAPTURE_ROWS, IAM_DENY_403, NATIVE_CAPTURE, captured, headers_with_type,
};
use reqwest::header::HeaderMap;
use routectl_core::ParsedErrorBody;
use routectl_testkit::{CapturedEvent, capture_events};

const DENY_HEADER: &str = "AccessDeniedException:urn:example:ignored-suffix";

fn map(read: UpstreamErrorRead) -> (Error, Vec<CapturedEvent>) {
    let mut err = None;
    let events = capture_events(|| {
        err = Some(map_bedrock_upstream_error("bedrock-invoke", "prov", read));
    });
    (err.expect("mapper ran"), events)
}

fn debug_body_line(events: &[CapturedEvent]) -> String {
    events
        .iter()
        .find(|e| e.message == "upstream error body")
        .and_then(|e| e.field("body"))
        .expect("DEBUG upstream error body line")
        .to_string()
}

fn carried_message(err: &Error) -> String {
    err.parsed_body()
        .and_then(ParsedErrorBody::error_message)
        .expect("the carried 403 body is an error envelope with a message")
        .to_string()
}

fn has_account_id(text: &str) -> bool {
    text.as_bytes()
        .windows(12)
        .any(|w| w.iter().all(u8::is_ascii_digit))
}

fn assert_no_principal(surface: &str, text: &str) {
    assert!(!text.contains("arn:"), "{surface} leaked an ARN: {text}");
    assert!(
        !has_account_id(text),
        "{surface} leaked an account id: {text}"
    );
    assert!(
        !text.contains("User:"),
        "{surface} leaked the principal: {text}"
    );
}

#[test]
fn a_recorded_credential_rejection_carries_its_reason_and_detail() {
    let native = CAPTURE_ROWS
        .iter()
        .filter(|(fixture, ..)| *fixture == NATIVE_CAPTURE);
    for (_, key, want) in native {
        let (err, events) = map(captured(NATIVE_CAPTURE, key).read());

        let rendered = format!(
            "bedrock access denied (AccessDeniedException): {}",
            want.as_str()
        );
        assert_eq!(err.upstream_reason(), Some(*want), "{key}");
        assert_eq!(carried_message(&err), rendered, "{key}");
        assert_eq!(debug_body_line(&events), rendered, "{key}");
    }
}

#[test]
fn the_carried_403_envelope_parses_as_the_message_it_names() {
    let (err, _) = map(captured(NATIVE_CAPTURE, "expired-converse").read());

    let Error::Upstream { body, .. } = &err else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    let reparsed: serde_json::Value = serde_json::from_str(body).expect("carried body is JSON");
    assert_eq!(
        reparsed,
        serde_json::json!({"error": {"message": carried_message(&err)}})
    );
}

#[test]
fn an_iam_deny_stays_reasonless_and_names_no_principal() {
    let cases: [(&str, HeaderMap); 2] = [
        ("body __type", HeaderMap::new()),
        ("body __type and header", headers_with_type(DENY_HEADER)),
    ];
    for (name, headers) in cases {
        let (err, events) = map(UpstreamErrorRead::from_parts(
            403,
            &headers,
            IAM_DENY_403,
            false,
        ));

        let Error::Upstream { body, .. } = &err else {
            panic!("expected Error::Upstream, got {err:?}");
        };
        let rendered =
            "bedrock access denied (AccessDeniedException): missing IAM action bedrock:InvokeModel";
        assert_eq!(err.upstream_reason(), None, "{name}");
        assert_eq!(carried_message(&err), rendered, "{name}");
        assert_eq!(debug_body_line(&events), rendered, "{name}");
        assert_no_principal("carried body", body);
    }
}

#[test]
fn the_403_warn_excerpt_names_no_principal() {
    let (_, events) = map(UpstreamErrorRead::from_parts(
        403,
        &HeaderMap::new(),
        IAM_DENY_403,
        false,
    ));

    let warn = events
        .iter()
        .find(|e| e.message == "bedrock IAM access denied")
        .expect("the 403 WARN fires");
    assert_eq!(warn.level, tracing::Level::WARN);
    assert!(
        warn.field("action")
            .is_some_and(|a| a.contains("bedrock:InvokeModel")),
        "the action field survives: {warn:?}"
    );
    assert_eq!(warn.field("principal_present"), Some("true"));
    let excerpt = warn.field("body_excerpt").expect("body_excerpt field");
    assert!(excerpt.contains("bedrock access denied"), "{excerpt}");
    assert_no_principal("WARN body_excerpt", excerpt);
}

#[test]
fn the_403_warn_action_field_is_none_for_an_arn_shaped_target() {
    let body = r#"{"message":"User: x is not authorized to perform: arn:aws:iam::123456789012:role/x on resource: y"}"#;

    let (_, events) = map(UpstreamErrorRead::from_parts(
        403,
        &HeaderMap::new(),
        body,
        false,
    ));

    let warn = events
        .iter()
        .find(|e| e.message == "bedrock IAM access denied")
        .expect("the 403 WARN fires");
    assert_eq!(warn.field("action"), Some("None"));
    for (_, value) in &warn.fields {
        assert_no_principal("WARN field", value);
    }
}
