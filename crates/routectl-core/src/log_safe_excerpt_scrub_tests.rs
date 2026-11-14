//! The over-cap excerpt of [`super::sanitize_upstream_body`] is scrubbed
//! before it is cut, always ends in the truncation marker, and an at-or-under
//! cap body passes through as it was.

use super::{sanitize_upstream_body, sanitize_upstream_body_with_cap};
use crate::{MAX_LOG_BODY_EXCERPT, UpstreamBody};

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
