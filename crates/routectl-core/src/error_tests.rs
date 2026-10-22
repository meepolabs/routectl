//! Tests for `Error`: the `Upstream` constructors and their single body
//! parse, the request-id attachment, and the bounded, redacted `Debug`
//! rendering.

use super::{Error, UpstreamBody};
use crate::RefusalReason;
use std::time::Duration;

#[test]
fn upstream_ctor_sets_retry_after_none() {
    // Arrange + Act
    let err = Error::upstream("test", 429, "rate limited");

    // Assert
    match err {
        Error::Upstream {
            provider,
            status,
            body,
            retry_after,
            ..
        } => {
            assert_eq!(provider, "test");
            assert_eq!(status, 429);
            assert_eq!(body, "rate limited");
            assert!(
                retry_after.is_none(),
                "plain ctor must set retry_after = None"
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

#[test]
fn upstream_with_retry_after_carries_value() {
    // Arrange
    let hint = Duration::from_secs(42);

    // Act
    let err = Error::upstream_with_retry_after("test", 429, "rate limited", Some(hint));

    // Assert
    match err {
        Error::Upstream { retry_after, .. } => {
            assert_eq!(retry_after, Some(hint), "ctor must carry the reset hint");
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

/// `Debug` for `Error::Upstream` renders the body as a bounded excerpt
/// plus a `body_len` marker, so a request-fault envelope carried up to
/// `MAX_ERROR_BODY_BYTES` cannot flood a `?e` WARN sink at full size.
#[test]
fn upstream_debug_bounds_oversized_body() {
    let body_len = crate::MAX_LOG_BODY_EXCERPT * 8;
    let body = "z".repeat(body_len);
    let err = Error::upstream("prov", 400, body);

    let rendered = format!("{err:?}");

    // The rendered Debug string is bounded: it never carries the full
    // oversized body (the excerpt + marker + field names are far under it).
    assert!(
        rendered.len() < body_len,
        "Debug output must be bounded, got {} for a {body_len}-byte body",
        rendered.len()
    );
    assert!(
        rendered.contains("body_excerpt"),
        "Debug must render a bounded body_excerpt field, got: {rendered}"
    );
    assert!(
        rendered.contains("... [truncated]"),
        "an oversized body must carry the truncation marker, got: {rendered}"
    );
    assert!(
        rendered.contains(&format!("body_len: {body_len}")),
        "Debug must carry the true total body length marker, got: {rendered}"
    );
}

/// A short body renders in full (no marker) and other fields keep their
/// derived Debug shape.
#[test]
fn upstream_debug_short_body_renders_in_full() {
    let err = Error::upstream_full(
        "prov",
        429,
        "rate limited",
        Some(Duration::from_secs(5)),
        Some("rate_limit_exceeded".to_string()),
        None,
    );

    let rendered = format!("{err:?}");

    assert!(rendered.contains("body_excerpt: \"rate limited\""));
    assert!(!rendered.contains("... [truncated]"));
    assert!(rendered.contains("body_len: 12"));
    assert!(rendered.contains("upstream_type: Some(\"rate_limit_exceeded\")"));
    assert!(rendered.contains("retry_after: Some("));
}

/// A JSON error envelope echoing a request credential renders without
/// it, while the upstream's message and type stay readable.
#[test]
fn upstream_debug_redacts_credential_echoed_in_json_body() {
    let body = r#"{"error":{"type":"invalid_request_error","message":"bad mcp server","authorization_token":"SENTINEL-ECHOED-TOKEN"}}"#;
    let err = Error::upstream("prov", 400, body);

    let rendered = format!("{err:?}");

    assert!(!rendered.contains("SENTINEL-ECHOED-TOKEN"), "{rendered}");
    assert!(rendered.contains("invalid_request_error"), "{rendered}");
    assert!(rendered.contains("bad mcp server"), "{rendered}");
    assert!(rendered.contains("<redacted len=21>"), "{rendered}");
}

/// An oversized JSON body is still capped after redaction, on a char
/// boundary.
#[test]
fn upstream_debug_caps_redacted_json_body() {
    let message = "\u{e9}".repeat(crate::MAX_LOG_BODY_EXCERPT * 4);
    let body = format!(r#"{{"error":{{"message":"{message}","api_key":"SENTINEL-KEY"}}}}"#);
    let err = Error::upstream("prov", 400, body);

    let rendered = format!("{err:?}");

    assert!(!rendered.contains("SENTINEL-KEY"), "{rendered}");
    assert!(rendered.contains("... [truncated]"), "{rendered}");
    assert!(rendered.chars().count() < crate::MAX_LOG_BODY_EXCERPT * 2);
}

/// A JSON body over the shared error-body ceiling is never parsed and
/// never excerpted raw: a fixed marker stands in.
#[test]
fn upstream_debug_oversized_json_body_renders_marker() {
    let filler = "x".repeat(crate::MAX_ERROR_BODY_BYTES);
    let body = format!(r#"{{"api_key":"SENTINEL-KEY","pad":"{filler}"}}"#);
    let err = Error::upstream("prov", 400, body.clone());

    let rendered = format!("{err:?}");

    let marker = format!("(json body, {} bytes, not excerpted)", body.len());
    assert!(rendered.contains(&marker), "{rendered}");
    assert!(!rendered.contains("SENTINEL-KEY"), "{rendered}");
    assert!(!rendered.contains("xxxx"), "{rendered}");
}

/// A JSON-looking body that no longer parses (a producer truncated it)
/// renders the marker rather than a raw prefix.
#[test]
fn upstream_debug_unparseable_json_body_renders_marker() {
    let body = r#"{"detail":[{"input":{"api_key":"SENTINEL-KEY"... [truncated]"#;
    let err = Error::upstream("prov", 422, body);

    let rendered = format!("{err:?}");

    assert!(rendered.contains("(json body, "), "{rendered}");
    assert!(!rendered.contains("SENTINEL-KEY"), "{rendered}");
}

/// With prompt redaction on, prompt text an error echoes is redacted in
/// the excerpt too; with it off the text stays readable.
#[test]
fn upstream_debug_excerpt_honors_prompt_redaction_flag() {
    let body = r#"{"error":{"message":"bad turn"},"echo":{"content":"SENTINEL-PROMPT"}}"#;

    let on = super::body_debug_excerpt_with_flag(body, true);
    let off = super::body_debug_excerpt_with_flag(body, false);

    assert!(!on.contains("SENTINEL-PROMPT"), "{on}");
    assert!(on.contains("bad turn"), "{on}");
    assert!(off.contains("SENTINEL-PROMPT"), "{off}");
}

/// A BOM-prefixed JSON body is still parsed and redacted.
#[test]
fn upstream_debug_redacts_bom_prefixed_json_body() {
    let body = "\u{feff}{\"error\":{\"message\":\"bad\"},\"api_key\":\"SENTINEL-BOM\"}";
    let err = Error::upstream("prov", 400, body);

    let rendered = format!("{err:?}");

    assert!(!rendered.contains("SENTINEL-BOM"), "{rendered}");
    assert!(rendered.contains("bad"), "{rendered}");
}

/// A control char in a local refusal's detail cannot reach a `?e` sink.
#[test]
fn local_refusal_debug_neutralizes_control_chars_in_detail() {
    let err = Error::LocalRefusal {
        provider: "prov".into(),
        detail: "line1\r\nforged: entry\u{1b}[31m".into(),
        reason: RefusalReason::CloakRelocation,
    };

    let rendered = format!("{err:?}");

    assert!(rendered.starts_with("LocalRefusal"), "{rendered}");
    assert!(rendered.contains("line1"), "{rendered}");
    for raw in ['\r', '\n', '\u{1b}'] {
        assert!(!rendered.contains(raw), "raw {raw:?} in: {rendered}");
    }
    for escaped in ["\\r", "\\n", "\\u{1b}"] {
        assert!(!rendered.contains(escaped), "{escaped} in: {rendered}");
    }
}

type UpstreamCtor = fn(&str) -> Error;

const UPSTREAM_CTORS: &[(&str, UpstreamCtor)] = &[
    ("upstream", |body| Error::upstream("prov", 400, body)),
    ("upstream_with_retry_after", |body| {
        Error::upstream_with_retry_after("prov", 400, body, None)
    }),
    ("upstream_full", |body| {
        Error::upstream_full("prov", 400, body, None, None, None)
    }),
    ("upstream_with_body", |body| {
        Error::upstream_with_body("prov", 400, UpstreamBody::new(body), None, None, None)
    }),
];

fn parses_during(build: impl FnOnce() -> Error) -> (Error, usize) {
    let before = crate::log_safe::capped_parse_count();
    let err = build();
    (err, crate::log_safe::capped_parse_count() - before)
}

#[test]
fn each_upstream_ctor_parses_an_in_cap_json_body_exactly_once() {
    let body = r#"{"error":{"type":"invalid_request_error","message":"bad"}}"#;
    for (name, ctor) in UPSTREAM_CTORS {
        let (err, parses) = parses_during(|| ctor(body));

        assert_eq!(parses, 1, "{name}");
        let parsed = err
            .parsed_body()
            .unwrap_or_else(|| panic!("{name}: no parse"));
        assert_eq!(parsed.error_type(), Some("invalid_request_error"), "{name}");
    }
}

#[test]
fn each_upstream_ctor_carries_no_parse_for_an_over_cap_body() {
    let filler = "x".repeat(crate::MAX_ERROR_BODY_BYTES);
    let body = format!(r#"{{"error":{{"message":"{filler}"}}}}"#);
    for (name, ctor) in UPSTREAM_CTORS {
        let (err, parses) = parses_during(|| ctor(&body));

        assert!(err.parsed_body().is_none(), "{name}");
        assert_eq!(parses, 0, "{name}");
    }
}

#[test]
fn upstream_with_body_attaches_the_bundled_parse_without_reparsing() {
    let body = UpstreamBody::new(r#"{"error":{"type":"overloaded_error"}}"#);

    let (err, parses) =
        parses_during(|| Error::upstream_with_body("prov", 529, body, None, None, None));

    assert_eq!(parses, 0);
    let carried = err.parsed_body().expect("parse attached");
    assert_eq!(carried.error_type(), Some("overloaded_error"));
}

#[test]
fn upstream_with_body_carries_the_parse_of_its_own_text() {
    let raw = UpstreamBody::new(r#"{"error":{"type":"raw","message":"secret"}}"#);
    let scrubbed = UpstreamBody::new(r#"{"error":{"type":"scrubbed"}}"#);
    assert_eq!(raw.parsed().and_then(|p| p.error_type()), Some("raw"));

    let err = Error::upstream_with_body("prov", 400, scrubbed, None, None, None);

    let Error::Upstream { body, .. } = &err else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(body, r#"{"error":{"type":"scrubbed"}}"#);
    let carried = err.parsed_body().expect("parse attached");
    assert_eq!(carried.error_type(), Some("scrubbed"));
    assert_eq!(carried.error_message(), None);
}

#[test]
fn with_upstream_request_id_keeps_the_parse() {
    let err = Error::upstream("prov", 400, r#"{"error":{"type":"t"}}"#)
        .with_upstream_request_id(Some("req-1".to_string()));

    assert_eq!(err.upstream_request_id(), Some("req-1"));
    let parsed = err.parsed_body().expect("parse survives");
    assert_eq!(parsed.error_type(), Some("t"));
}

#[test]
fn parsed_body_is_none_off_the_upstream_variant() {
    assert!(
        Error::Validation("bad".to_string(), RefusalReason::FieldType)
            .parsed_body()
            .is_none()
    );
}

/// The body itself renders (Display raw, Debug as a redacted excerpt),
/// so the planted key appears exactly once per rendering; a rendered
/// parse would add a second occurrence or a `ParsedErrorBody` marker.
#[test]
fn upstream_debug_and_display_never_render_the_parse() {
    let sentinel = "SENTINEL_PARSED_KEY";
    let body = format!(r#"{{"error":{{"type":"t","{sentinel}":"v"}}}}"#);
    let err =
        Error::upstream("prov", 400, body).with_upstream_request_id(Some("req-9".to_string()));
    assert!(err.parsed_body().is_some(), "fixture must carry a parse");

    for rendered in [format!("{err:?}"), err.to_string()] {
        assert_eq!(rendered.matches(sentinel).count(), 1, "{rendered}");
        assert!(!rendered.contains("ParsedErrorBody"), "{rendered}");
        assert!(!rendered.contains("UpstreamExtras"), "{rendered}");
    }
}

/// Non-`Upstream` variants keep the derived Debug shape (tuple form).
#[test]
fn non_upstream_variant_debug_unchanged() {
    let err = Error::Validation("bad field".to_string(), RefusalReason::FieldType);
    assert_eq!(
        format!("{err:?}"),
        "Validation(\"bad field\", \"field-type\")"
    );
}

#[test]
fn with_upstream_request_id_sets_field_on_upstream() {
    // Arrange
    let err = Error::upstream("test", 429, "rate limited");

    // Act
    let err = err.with_upstream_request_id(Some("req-123".to_string()));

    // Assert
    assert_eq!(err.upstream_request_id(), Some("req-123"));
}

#[test]
fn with_upstream_request_id_defaults_none_and_is_noop_off_variant() {
    // Arrange: the plain ctor leaves the id unset.
    assert!(
        Error::upstream("test", 500, "boom")
            .upstream_request_id()
            .is_none(),
        "ctor must default to None"
    );

    // Act + Assert: chaining on a non-Upstream variant is a no-op.
    let other = Error::Validation("bad".to_string(), RefusalReason::FieldType)
        .with_upstream_request_id(Some("x".into()));
    assert_eq!(format!("{other:?}"), "Validation(\"bad\", \"field-type\")");
}

#[test]
fn capability_unsupported_everywhere_displays_the_capability_and_no_lane() {
    let err =
        Error::capability_unsupported_everywhere("advisor", ["lane-alpha#up-1", "lane-beta#up-2"]);

    let Error::CapabilityUnsupportedEverywhere { capability, lanes } = &err else {
        panic!("expected CapabilityUnsupportedEverywhere, got {err:?}");
    };
    assert_eq!(&**capability, "advisor");
    let lanes: Vec<&str> = lanes.iter().map(|l| &**l).collect();
    assert_eq!(lanes, ["lane-alpha#up-1", "lane-beta#up-2"]);

    let display = err.to_string();
    assert!(display.contains("`advisor`"), "{display}");
    assert!(!display.contains("lane-alpha"), "{display}");
    assert!(!display.contains("lane-beta"), "{display}");
    assert!(format!("{err:?}").contains("lane-beta#up-2"));
    assert_eq!(err.refusal_reason(), None);
}
