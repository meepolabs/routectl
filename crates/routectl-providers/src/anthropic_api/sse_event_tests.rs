//! In-stream error and ping contract tests for the Anthropic SSE
//! state machine. Relocated from `sse.rs`'s inline `mod tests` so
//! the parent file stays under the project's 800-LOC ceiling. Tests
//! retain access to private items via `use super::*`.

use super::*;

/// Anthropic spec allows a 200 response to carry an in-band
/// `error` event mid-stream. Without explicit handling, the
/// parser silently consumed it as housekeeping (Ok(None)) and
/// the SSE wrapper happily emitted clean EOS to the client,
/// hiding upstream failures + breaking router circuit-breaker
/// health accounting. Pin the contract so a future change can't
/// regress.
#[test]
fn in_stream_error_event_surfaces_as_streaming_error() {
    let mut state = SseState::default();
    let payload = r#"{"type":"error","error":{"type":"overloaded_error","message":"slow down"}}"#;
    let err = state
        .parse_event("test-anthropic", payload)
        .expect_err("error event must surface as Err");
    match err {
        Error::Streaming(msg) => {
            assert!(msg.contains("anthropic in-stream error"), "msg: {msg}");
            assert!(msg.contains("overloaded_error"), "msg: {msg}");
        }
        other => panic!("expected Error::Streaming, got: {other:?}"),
    }
}

/// Counterpart to the above: housekeeping events still produce
/// `Ok(None)`. Pinning this prevents a future change that
/// over-corrects the error-mapping fix into surfacing pings as
/// failures.
#[test]
fn ping_event_remains_ok_none() {
    let mut state = SseState::default();
    let got = state
        .parse_event("test-anthropic", r#"{"type":"ping"}"#)
        .unwrap();
    assert!(got.is_none(), "ping must be Ok(None), got: {got:?}");
}
