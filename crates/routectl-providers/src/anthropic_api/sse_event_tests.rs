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

const MESSAGE_START: &str = r#"{
    "type":"message_start",
    "message": {
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "content": [],
        "model": "claude-opus-4-7",
        "stop_reason": null,
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 0}
    }
}"#;

/// The Anthropic stream opens with a single `delta.role="assistant"`
/// chunk at `message_start`, before any content -- matching every peer
/// egress lane. The non-final chunk omits `usage` and `finish_reason`.
#[test]
fn anthropic_stream_opens_with_role_chunk() {
    let mut state = SseState::default();
    let chunk = state
        .parse_event("test-anthropic", MESSAGE_START)
        .unwrap()
        .expect("message_start must emit the opening role chunk");
    let delta = &chunk.choices[0].delta;
    assert!(matches!(delta.role, Some(routectl_core::Role::Assistant)));
    assert!(delta.content.is_none());
    assert!(chunk.usage.is_none());
    assert!(chunk.choices[0].finish_reason.is_none());

    // The serialized shape must omit usage/finish_reason so the
    // non-final opening chunk keeps the usage:null / finish_reason:null
    // omission the peer lanes rely on.
    let json = serde_json::to_value(&chunk).unwrap();
    assert!(json.get("usage").is_none(), "usage must be omitted: {json}");
    assert!(
        json["choices"][0].get("finish_reason").is_none(),
        "finish_reason must be omitted: {json}"
    );
}

/// A stream that errors before `message_start` yields NO role chunk:
/// the error surfaces and `role_emitted` was never set.
#[test]
fn anthropic_no_role_chunk_when_error_before_message_start() {
    let mut state = SseState::default();
    let payload = r#"{"type":"error","error":{"type":"overloaded_error","message":"slow"}}"#;
    let err = state.parse_event("test-anthropic", payload);
    assert!(err.is_err(), "error event must surface as Err");
    assert!(
        !state.role_emitted,
        "no role chunk may be emitted before message_start"
    );
}

/// A malformed upstream repeating `message_start` must not emit a second
/// role chunk -- the opening role chunk fires exactly once per stream.
#[test]
fn anthropic_role_chunk_emitted_once() {
    let mut state = SseState::default();
    let first = state.parse_event("test-anthropic", MESSAGE_START).unwrap();
    let second = state.parse_event("test-anthropic", MESSAGE_START).unwrap();
    assert!(first.is_some(), "first message_start emits the role chunk");
    assert!(
        second.is_none(),
        "a repeated message_start must not emit a second role chunk"
    );
}
