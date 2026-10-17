//! The error mapper parses an upstream error body at most once, within the
//! shared error-body cap, and carries that parse on the error it builds.

use super::map_gemini_upstream_error;
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

fn map(body: &str) -> (Error, usize) {
    capped_parses_during(|| {
        map_gemini_upstream_error(
            "gemini:test",
            UpstreamErrorRead::from_parts(429, &HeaderMap::new(), body, false),
        )
    })
}

#[test]
fn in_cap_envelope_parses_once_and_carries_the_parse() {
    let body = r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;

    let (err, parses) = map(body);

    assert_eq!(parses, 1);
    let parsed = err.parsed_body().expect("envelope parse carried");
    assert_eq!(parsed.error_message(), Some("Quota exceeded"));
}

#[test]
fn over_cap_json_body_is_never_parsed() {
    let filler = "x".repeat(MAX_ERROR_BODY_BYTES);
    let body = format!(r#"{{"error":{{"code":429,"message":"{filler}"}}}}"#);

    let (err, parses) = map(&body);

    assert_eq!(parses, 0);
    assert!(err.parsed_body().is_none());
}
