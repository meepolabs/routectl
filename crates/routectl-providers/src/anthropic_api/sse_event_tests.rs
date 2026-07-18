//! In-stream error and ping contract tests for the Anthropic SSE
//! state machine. Relocated from `sse.rs`'s inline `mod tests` so
//! the parent file stays under the project's 800-LOC ceiling. Tests
//! retain access to private items via `use super::*`.

use super::*;

/// Anthropic spec allows a 200 response to carry an in-band `error`
/// event mid-stream. It must surface the upstream `error.type` and the
/// synthetic status the sync path would carry (via
/// `anthropic_error_type_to_status`) rather than collapsing to a bare
/// `Error::Streaming` -- otherwise `failure_class::classify` maps it to
/// `NetworkError` by variant and the client sees `api_error` instead of
/// the real `overloaded_error`. Pin the contract so a future change
/// can't regress.
#[test]
fn in_stream_error_event_surfaces_upstream_type_and_status() {
    let mut state = SseState::default();
    let payload = r#"{"type":"error","error":{"type":"overloaded_error","message":"slow down"}}"#;
    let err = state
        .parse_event("test-anthropic", payload)
        .expect_err("error event must surface as Err");
    match err {
        Error::Upstream {
            status,
            upstream_type,
            body,
            ..
        } => {
            assert_eq!(status, 529, "overloaded_error maps to the sync-path 529");
            assert_eq!(
                upstream_type.as_deref(),
                Some("overloaded_error"),
                "the upstream error.type must be preserved for the classifier"
            );
            assert!(body.contains("slow down"), "message preserved: {body}");
        }
        other => panic!("expected Error::Upstream, got: {other:?}"),
    }
}

/// The in-stream error and the sync (non-stream) error path must
/// classify identically: a mid-stream `overloaded_error` resolves to
/// `Overloaded` (was `NetworkError` by variant when the path returned
/// `Error::Streaming`). This is what newly makes stream errors
/// retry/fallback-eligible on the same terms as the sync path.
#[test]
fn in_stream_error_classifies_same_as_sync_path() {
    use routectl_core::failure_class::{FailureClass, classify};

    let mut state = SseState::default();
    let payload = r#"{"type":"error","error":{"type":"overloaded_error","message":"slow"}}"#;
    let stream_err = state
        .parse_event("test-anthropic", payload)
        .expect_err("error event must surface as Err");

    // The sync path constructs an equivalent structured upstream error.
    let sync_err = Error::upstream_full(
        "test-anthropic",
        529,
        "overloaded_error: slow",
        None,
        Some("overloaded_error".to_string()),
        None,
    );

    let stream_class = classify(&stream_err, Some("anthropic"));
    let sync_class = classify(&sync_err, Some("anthropic"));
    assert_eq!(
        stream_class.class,
        FailureClass::Overloaded,
        "in-stream overloaded_error must classify as Overloaded, not NetworkError"
    );
    assert_eq!(
        stream_class.class, sync_class.class,
        "streaming and sync failure classification must converge"
    );
}

/// Counterpart: housekeeping events still produce `Ok(None)`. Pinning
/// this prevents a future change that over-corrects the error-mapping
/// fix into surfacing pings as failures.
#[test]
fn ping_event_remains_ok_none() {
    let mut state = SseState::default();
    let got = state
        .parse_event("test-anthropic", r#"{"type":"ping"}"#)
        .unwrap();
    assert!(got.is_none(), "ping must be Ok(None), got: {got:?}");
}
