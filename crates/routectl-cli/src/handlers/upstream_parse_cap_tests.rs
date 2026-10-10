//! The client-facing upstream detail and `param` come only from the body
//! parse the error carries. A JSON error body over the core parse cap carries
//! no parse, so neither the non-streaming envelope nor the stream terminal
//! surfaces anything from it; the same envelope within the cap still does.
//! The failure classifier and the capability matcher read the same carried
//! parse, so the cap withholds the envelope from every consumer at once.
//! The cap counts raw bytes, so multi-byte content and a leading BOM count
//! toward it.

use super::*;
use axum::body::to_bytes;
use routectl_core::failure_class::{ClassifiedFailure, FailureClass, MatchedBy};
use routectl_core::{Error, MAX_ERROR_BODY_BYTES, ReplayAttempt};
use routectl_router::resolve_requested_capability;

const MESSAGE: &str = "max_tokens is too large";
const PARAM: &str = "max_tokens";
const BOM: char = '\u{feff}';

fn envelope_body(padding: &str) -> String {
    json!({
        "error": {
            "type": "invalid_request_error",
            "message": MESSAGE,
            "param": PARAM,
        },
        "padding": padding,
    })
    .to_string()
}

fn in_cap_error() -> Error {
    let err = Error::upstream_full("p-internal", 400, envelope_body(""), None, None, None);
    assert!(
        err.parsed_body().is_some(),
        "in-cap fixture must carry a parse"
    );
    err
}

fn ascii_over_cap_body(body_with: fn(&str) -> String) -> String {
    body_with(&"x".repeat(MAX_ERROR_BODY_BYTES))
}

fn multibyte_over_cap_body(body_with: fn(&str) -> String) -> String {
    let body = body_with(&"\u{e9}".repeat(MAX_ERROR_BODY_BYTES / 2 + 1));
    assert!(
        body.chars().count() <= MAX_ERROR_BODY_BYTES,
        "multi-byte fixture must be within the cap in chars"
    );
    body
}

fn bom_over_cap_body(body_with: fn(&str) -> String) -> String {
    let shell_len = body_with("").len();
    let envelope = body_with(&"x".repeat(MAX_ERROR_BODY_BYTES - shell_len));
    assert_eq!(
        envelope.len(),
        MAX_ERROR_BODY_BYTES,
        "BOM fixture must sit exactly at the cap once the BOM is stripped"
    );
    format!("{BOM}{envelope}")
}

/// Each row is a valid JSON error body over the cap in raw bytes, built by
/// padding the envelope `body_with` renders.
fn over_cap_errors(body_with: fn(&str) -> String) -> [(&'static str, Error); 3] {
    [
        ("ascii padding", ascii_over_cap_body(body_with)),
        ("multi-byte padding", multibyte_over_cap_body(body_with)),
        ("leading BOM at the cap", bom_over_cap_body(body_with)),
    ]
    .map(|(row, body)| (row, over_cap_error(row, body)))
}

fn over_cap_error(row: &str, body: String) -> Error {
    assert!(
        body.len() > MAX_ERROR_BODY_BYTES,
        "{row}: fixture must be over the cap in bytes"
    );
    assert!(
        serde_json::from_str::<Value>(body.trim_start_matches(BOM)).is_ok(),
        "{row}: fixture must be valid JSON so only the cap withholds it"
    );
    let err = Error::upstream_full("p-internal", 400, body, None, None, None);
    assert!(
        err.parsed_body().is_none(),
        "{row}: over-cap body carries no parse"
    );
    err
}

async fn openai_error_object(err: Error) -> Value {
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["error"].clone()
}

#[tokio::test]
async fn map_error_surfaces_detail_and_param_from_an_in_cap_body() {
    let error = openai_error_object(in_cap_error()).await;

    assert_eq!(
        error["message"],
        format!("upstream error (HTTP 400): {MESSAGE}")
    );
    assert_eq!(error["param"], PARAM);
}

#[tokio::test]
async fn map_error_surfaces_no_detail_or_param_from_an_over_cap_body() {
    for (row, err) in over_cap_errors(envelope_body) {
        let error = openai_error_object(err).await;

        assert_eq!(error["message"], "upstream error (HTTP 400)", "{row}");
        assert!(
            error["param"].is_null(),
            "{row}: param must be null: {error}"
        );
    }
}

#[test]
fn stream_error_surfaces_detail_from_an_in_cap_body() {
    let safe = sanitize_stream_error_for_client(&in_cap_error());

    assert_eq!(safe, format!("upstream stream error (HTTP 400): {MESSAGE}"));
}

#[test]
fn stream_error_surfaces_no_detail_from_an_over_cap_body() {
    for (row, err) in over_cap_errors(envelope_body) {
        let safe = sanitize_stream_error_for_client(&err);

        assert_eq!(safe, "upstream stream error (HTTP 400)", "{row}");
    }
}

const REPLAY_KIND: &str = "openai-responses";

fn replay_rejection_body(padding: &str) -> String {
    json!({
        "error": {
            "type": "invalid_request_error",
            "code": "validation_error",
            "message": "encrypted content missing recognized prefix",
            "param": null,
        },
        "padding": padding,
    })
    .to_string()
}

fn replay_class_for(err: &Error) -> FailureClass {
    routectl_core::failure_class::classify_with_attempt(
        err,
        Some(REPLAY_KIND),
        ReplayAttempt::with_gray_artifacts(1),
    )
    .class
}

fn openai_compat_param_capability(err: &Error) -> Option<String> {
    let cf = ClassifiedFailure {
        class: FailureClass::FeatureUnsupported {
            capability: "unsupported_parameter".to_string(),
        },
        matched_by: MatchedBy::UpstreamType,
    };
    resolve_requested_capability("openai-compat", err, &cf).map(|(key, _, _)| key)
}

fn matcher_body(padding: &str) -> String {
    json!({
        "error": {
            "type": "invalid_request_error",
            "code": "unsupported_parameter",
            "param": "response_format",
        },
        "padding": padding,
    })
    .to_string()
}

#[test]
fn classifier_reads_the_envelope_from_an_in_cap_body() {
    let err = Error::upstream_full(
        "p-internal",
        400,
        replay_rejection_body(""),
        None,
        None,
        None,
    );

    let class = replay_class_for(&err);

    assert!(
        matches!(class, FailureClass::FeatureUnsupported { .. }),
        "{class:?}"
    );
}

#[test]
fn classifier_sees_no_envelope_in_an_over_cap_body() {
    for (row, err) in over_cap_errors(replay_rejection_body) {
        assert_eq!(replay_class_for(&err), FailureClass::BadRequest, "{row}");
    }
}

#[test]
fn matcher_reads_the_param_from_an_in_cap_body() {
    let err = Error::upstream_full("p-internal", 400, matcher_body(""), None, None, None);

    let capability = openai_compat_param_capability(&err);

    assert_eq!(capability.as_deref(), Some("structured_output"));
}

#[test]
fn matcher_sees_no_param_in_an_over_cap_body() {
    for (row, err) in over_cap_errors(matcher_body) {
        assert_eq!(openai_compat_param_capability(&err), None, "{row}");
    }
}
