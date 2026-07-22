//! Excerpt-sanitization tests.

use super::{AuthKind, build_error_excerpt, map_responses_upstream_error};
use reqwest::header::HeaderMap;
use routectl_core::{Error, sanitize_for_log};

#[test]
fn excerpt_sanitizes_crlf_and_ansi() {
    let body = "boom\r\n[fake INFO] injected\x1b[31mred";
    let msg = build_error_excerpt(body);
    let safe_excerpt = sanitize_for_log(&msg);
    assert!(
        !safe_excerpt.contains('\r'),
        "CR in excerpt: {safe_excerpt:?}"
    );
    assert!(
        !safe_excerpt.contains('\n'),
        "LF in excerpt: {safe_excerpt:?}"
    );
    assert!(
        !safe_excerpt.contains('\x1b'),
        "ESC in excerpt: {safe_excerpt:?}"
    );
}

/// The shared mapper drives both `complete()` and `stream()`. A plain
/// rate-limit body with a parseable `Retry-After` must surface that
/// reset on the canonical error from the single helper.
#[test]
fn map_upstream_error_preserves_retry_after_for_both_callers() {
    // Arrange: a 429 with a header reset hint, no codex body hint.
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", "30".parse().unwrap());
    let body = r#"{"error":{"type":"rate_limit_exceeded","message":"slow down"}}"#;

    // Act
    let err = map_responses_upstream_error("p", 429, &headers, &AuthKind::ApiKey, body, false);

    // Assert
    match err {
        Error::Upstream {
            status,
            retry_after,
            body,
            ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(30)));
            assert!(
                body.contains("slow down"),
                "message must reach body: {body}"
            );
        }
        other => panic!("expected Error::Upstream, got: {other:?}"),
    }
}

/// The Codex usage-limit body carries the 5-hour-cap reset and must
/// win over the header `Retry-After`. Proves the codex-hint resolution
/// stays INSIDE the extracted helper for both callers.
#[test]
fn map_upstream_error_codex_body_hint_wins_over_header() {
    // Arrange: a header hint of 30s AND a codex usage-limit body whose
    // resets_in_seconds is 7200 -- the body must win.
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", "30".parse().unwrap());
    let body =
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":7200,"message":"capped"}}"#;

    // Act
    let err =
        map_responses_upstream_error("p", 429, &headers, &AuthKind::ChatgptOauth, body, false);

    // Assert: the body's 7200s reset, not the header's 30s.
    match err {
        Error::Upstream { retry_after, .. } => {
            assert_eq!(
                retry_after,
                Some(std::time::Duration::from_hours(2)),
                "codex body hint must override the header Retry-After"
            );
        }
        other => panic!("expected Error::Upstream, got: {other:?}"),
    }
}

/// An over-cap error body (`hit_cap == true`) must never echo the
/// truncated prefix -- even one that still looks like a JSON error
/// envelope. The client sees only the fixed cap-exceeded message, the
/// original status is preserved, and the header `Retry-After` survives
/// (the body-derived codex hint is attempted on the prefix but an
/// incomplete envelope fails to parse, so it falls back to the header).
#[test]
fn map_upstream_error_over_cap_hides_body_and_preserves_status() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", "30".parse().unwrap());
    let prefix = r#"{"error":{"message":"secret upstream detail"#;

    let err =
        map_responses_upstream_error("p", 429, &headers, &AuthKind::ChatgptOauth, prefix, true);

    match err {
        Error::Upstream {
            status,
            retry_after,
            body,
            ..
        } => {
            assert_eq!(status, 429, "original status preserved on cap trip");
            assert_eq!(
                retry_after,
                Some(std::time::Duration::from_secs(30)),
                "header Retry-After preserved on cap trip"
            );
            assert!(
                !body.contains("secret upstream detail"),
                "truncated body must not be echoed: {body}"
            );
            assert!(body.contains("exceeded"), "cap message expected: {body}");
        }
        other => panic!("expected Error::Upstream, got: {other:?}"),
    }
}

/// A cap trip whose prefix is a COMPLETE codex usage-limit envelope: the
/// body-derived reset hint IS attempted on the prefix and wins over the
/// header hint, while the client body still collapses to the fixed cap
/// message (never echoing the envelope text).
#[test]
fn map_upstream_error_over_cap_lifts_codex_hint_when_prefix_parses() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", "30".parse().unwrap());
    let prefix =
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":7200,"message":"capped"}}"#;

    let err =
        map_responses_upstream_error("p", 429, &headers, &AuthKind::ChatgptOauth, prefix, true);

    match err {
        Error::Upstream {
            retry_after, body, ..
        } => {
            assert_eq!(
                retry_after,
                Some(std::time::Duration::from_hours(2)),
                "codex body hint must lift from a parseable prefix and win over the header"
            );
            assert!(
                !body.contains("capped"),
                "client body must be the fixed cap message, never the envelope: {body}"
            );
            assert!(body.contains("exceeded"), "cap message expected: {body}");
        }
        other => panic!("expected Error::Upstream, got: {other:?}"),
    }
}

/// The upstream-failure WARN fires on a cap trip, but its `body_excerpt`
/// is the fixed cap message -- the truncated prefix must never appear at
/// WARN level (prefix content is confined to the DEBUG-gated path).
#[test]
fn map_upstream_error_over_cap_warn_excerpt_is_fixed_message() {
    let prefix = r#"{"error":{"message":"secret upstream detail that must not be logged"#;

    let events = routectl_testkit::capture_events(|| {
        let _ = map_responses_upstream_error(
            "p",
            429,
            &HeaderMap::new(),
            &AuthKind::ChatgptOauth,
            prefix,
            true,
        );
    });

    let warn = events
        .iter()
        .find(|e| e.level == tracing::Level::WARN && e.field("context") == Some("openai-responses"))
        .expect("upstream-failure WARN must fire on a cap trip");
    assert_eq!(
        warn.field("body_excerpt"),
        Some(crate::http_client::body_cap_exceeded_message().as_str()),
        "WARN excerpt must be the fixed cap message on a cap trip"
    );
    assert!(
        events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN)
            .all(|e| e
                .fields
                .iter()
                .all(|(_, v)| !v.contains("secret upstream detail"))),
        "no WARN-level event may echo the truncated prefix"
    );
}
