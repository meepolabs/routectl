//! The over-cap excerpt of [`super::sanitize_upstream_body`] is scrubbed
//! before it is cut, always ends in the truncation marker, and an at-or-under
//! cap body passes through as it was.

use super::{
    sanitize_for_log, sanitize_upstream_body, sanitize_upstream_body_with_cap,
    upstream_excerpt_for_log,
};
use crate::{MAX_ERROR_BODY_BYTES, MAX_LOG_BODY_EXCERPT, UpstreamBody};

const SENTINEL: &str = "excerpt-sentinel-kept";
const MARKER: &str = "... [truncated]";
const KEY: &str = "sk-ant-api03-SECRETBODY0123456789abcdefghij";

/// A three-segment JWT whose length alone runs past the excerpt cut.
fn long_jwt() -> String {
    let segment = "abcDEF1234".repeat(30);
    format!("eyJ{segment}.{segment}.{}", &segment[..40])
}

/// `(name, body, planted)`: each credential sits where a cut at
/// [`MAX_LOG_BODY_EXCERPT`] characters splits it short of its scrub rule.
fn planted_over_cap() -> Vec<(&'static str, String, &'static str)> {
    let key_at = MAX_LOG_BODY_EXCERPT - 23;
    let head = format!("{SENTINEL} ");
    let pad = "x".repeat(key_at - head.len() - 1);
    let straddling = format!("{head}{pad} {KEY} tail");
    assert_eq!(straddling.find(KEY), Some(key_at), "key offset");
    let jwt = long_jwt();
    let with_jwt = format!("{SENTINEL} rejected token {jwt} tail");
    assert!(
        with_jwt.len() > MAX_LOG_BODY_EXCERPT,
        "jwt runs past the cut"
    );
    vec![
        ("key straddling the cut", straddling, "sk-ant-api03-"),
        ("jwt longer than the cut", with_jwt, "eyJ"),
    ]
}

#[test]
fn over_cap_excerpt_redacts_a_credential_the_cut_would_split() {
    for (row, body, planted) in planted_over_cap() {
        let excerpt = sanitize_upstream_body(&body);

        assert!(!excerpt.contains(planted), "{row}: planted in {excerpt:?}");
        assert!(excerpt.contains("[redacted]"), "{row}: {excerpt:?}");
        assert!(excerpt.contains(SENTINEL), "{row}: sentinel lost");
        assert!(excerpt.ends_with(MARKER), "{row}: marker missing");
    }
}

#[test]
fn over_cap_json_excerpt_never_parses() {
    let jwt = long_jwt();
    let rows = [
        (
            "longer than the cut after the scrub",
            format!(r#"{{"error":{{"message":"{}"}}}}"#, "m".repeat(600)),
        ),
        (
            "shrunk under the cut by the scrub",
            format!(r#"{{"error":{{"message":"{SENTINEL} {jwt}"}}}}"#),
        ),
    ];
    for (row, body) in rows {
        assert!(body.len() > MAX_LOG_BODY_EXCERPT, "{row}: over the cap");
        assert!(
            UpstreamBody::new(body.as_str()).parsed().is_some(),
            "{row}: the input itself parses"
        );

        let excerpt = sanitize_upstream_body(&body);

        assert!(excerpt.ends_with(MARKER), "{row}: {excerpt:?}");
        assert!(
            UpstreamBody::new(excerpt.as_str()).parsed().is_none(),
            "{row}: the excerpt parses: {excerpt:?}"
        );
    }
}

#[test]
fn at_or_under_cap_body_passes_through_unscrubbed() {
    let at_cap = format!("{KEY} {}", "y".repeat(MAX_LOG_BODY_EXCERPT - KEY.len() - 1));
    assert_eq!(at_cap.len(), MAX_LOG_BODY_EXCERPT);
    let rows = [
        ("short", format!("  {SENTINEL} {KEY}  ")),
        ("exactly at the cap", at_cap),
        ("json", format!(r#"{{"error":{{"message":"{KEY}"}}}}"#)),
    ];
    for (row, body) in rows {
        assert_eq!(sanitize_upstream_body(&body), body.trim(), "{row}");
    }
}

#[test]
fn excerpt_cut_counts_chars_of_the_scrubbed_text() {
    let body = format!("{KEY} {}", "z".repeat(40));

    let excerpt = sanitize_upstream_body_with_cap(&body, 20);

    assert_eq!(excerpt, format!("[redacted] {}{MARKER}", "z".repeat(9)));
}

/// `SENTINEL`, then an ARN padded so `tail` starts at byte `tail_at`, then
/// `tail`. The scrub collapses the ARN to `[redacted]`, which pulls whatever
/// follows it to the front of the excerpt.
fn arn_then(tail_at: usize, tail: &str) -> String {
    let head = format!("{SENTINEL} arn:aws:iam::123456789012:role/");
    let body = format!("{head}{} {tail}", "r".repeat(tail_at - head.len() - 1));
    assert_eq!(body.find(tail), Some(tail_at), "tail offset");
    body
}

/// A JWT whose third segment the error-body bound leaves 5 characters of,
/// short of the scrub's 10-character segment floor.
fn jwt_split_by_the_bound() -> (usize, String) {
    let segment = "abcDEF1234".repeat(3);
    let jwt = format!("eyJ{segment}.{segment}.{segment}");
    let kept = "eyJ".len() + 2 * (segment.len() + 1) + 5;
    (MAX_ERROR_BODY_BYTES - kept, jwt)
}

#[test]
fn excerpt_drops_a_credential_the_error_body_bound_splits() {
    let (jwt_at, jwt) = jwt_split_by_the_bound();
    const QUOTED: &str = r#""api_key":"planted one planted two""#;
    let quoted_at = MAX_ERROR_BODY_BYTES - (QUOTED.len() - 4);
    let rows = [
        (
            "jwt whose last segment the bound leaves short",
            arn_then(jwt_at, &jwt),
            "eyJ",
            format!("{SENTINEL} [redacted]{MARKER}"),
        ),
        (
            "quoted value the bound cuts after a space inside it",
            arn_then(quoted_at, QUOTED),
            "planted",
            format!(r#"{SENTINEL} [redacted] "api_key":"[redacted]{MARKER}"#),
        ),
    ];
    for (row, body, planted, want) in rows {
        assert!(body.len() > MAX_ERROR_BODY_BYTES, "{row}: over the bound");

        let excerpt = sanitize_upstream_body(&body);

        assert!(!excerpt.contains(planted), "{row}: fragment in {excerpt:?}");
        assert_eq!(excerpt, want, "{row}");
    }
}

#[test]
fn excerpt_of_one_token_filling_the_error_body_bound_is_the_marker_alone() {
    let segment = "abcDEF1234".repeat(3);
    let body = format!(
        "eyJ{}.{segment}.{segment}",
        "a".repeat(MAX_ERROR_BODY_BYTES)
    );

    let excerpt = sanitize_upstream_body(&body);

    assert_eq!(excerpt, MARKER);
}

#[test]
fn excerpt_keeps_the_last_token_of_a_body_the_error_body_bound_does_not_cut() {
    const TAIL: &str = "last-token-kept";
    let rows = [
        ("between the cap and the bound", arn_then(2000, TAIL)),
        (
            "exactly at the bound",
            arn_then(MAX_ERROR_BODY_BYTES - TAIL.len(), TAIL),
        ),
    ];
    for (row, body) in rows {
        assert!(
            body.len() <= MAX_ERROR_BODY_BYTES,
            "{row}: within the bound"
        );

        let excerpt = sanitize_upstream_body(&body);

        assert_eq!(
            excerpt,
            format!("{SENTINEL} [redacted] {TAIL}{MARKER}"),
            "{row}"
        );
    }
}

/// A JSON envelope `len` characters long whose message ends in a vendor key
/// the scrub collapses, so the scrubbed text is far shorter than `len`.
fn envelope_with_key(len: usize) -> String {
    let open = r#"{"error":{"message":"ref sk-ant-api03-"#;
    let close = r#""}}"#;
    let body = format!(
        "{open}{}{close}",
        "S".repeat(len - open.len() - close.len())
    );
    assert_eq!(body.chars().count(), len);
    body
}

#[test]
fn log_excerpt_at_or_under_its_cut_is_the_filtered_text() {
    let rows = [
        ("json at the cut", envelope_with_key(256)),
        (
            "multibyte at the cut",
            format!("caf\u{e9} {}", "\u{e9}".repeat(251)),
        ),
        ("control characters", "line\r\nnext\u{1b}[31m".to_string()),
    ];
    for (row, text) in rows {
        assert!(text.chars().count() <= 256, "{row}: at or under the cut");

        assert_eq!(
            upstream_excerpt_for_log(&text),
            sanitize_for_log(&text),
            "{row}"
        );
    }
}

#[test]
fn log_excerpt_past_its_cut_ends_in_the_marker_even_when_the_scrub_shrinks_it() {
    let text = envelope_with_key(257);

    let excerpt = upstream_excerpt_for_log(&text);

    assert_eq!(
        excerpt,
        format!(r#"{{"error":{{"message":"ref [redacted]"}}}}{MARKER}"#)
    );
    assert!(UpstreamBody::new(excerpt.as_str()).parsed().is_none());
}
