//! Streaming pin for the `thinking.display: "omitted"` wire shape: a
//! thinking block with NO `thinking_delta` but WITH a `signature_delta`.
//! Sibling of `sse.rs` (declared via `#[cfg(test)] #[path = ...]`) so the
//! parent stays under the 800-LOC ceiling.

use super::*;

fn send(state: &mut SseState, payload: &str) -> Option<routectl_core::ChatChunk> {
    state
        .parse_event("test-provider", payload)
        .expect("parse_event must not fail")
}

const THINKING_START: &str =
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#;
const BLOCK_STOP: &str = r#"{"type":"content_block_stop","index":0}"#;

fn signature_delta(sig: &str) -> String {
    format!(
        r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"signature_delta","signature":"{sig}"}}}}"#
    )
}

fn thinking_delta(text: &str) -> String {
    format!(
        r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"thinking_delta","thinking":"{text}"}}}}"#
    )
}

/// The drop guard at content_block_stop keys on empty-text AND absent
/// signature. Under `display: "omitted"` the signature IS present, so an
/// empty-text block must still emit its aggregated detail -- dropping it
/// would lose the signature the next replay turn needs.
#[test]
fn empty_thinking_with_signature_still_emits_a_detail() {
    // Arrange
    let mut state = SseState::default();

    // Act: no thinking_delta at all, only a signature.
    send(&mut state, THINKING_START);
    send(&mut state, &signature_delta("EqoCCkYIBxgCKkASDGZvbw=="));
    let chunk = send(&mut state, BLOCK_STOP);

    // Assert
    let chunk = chunk.expect("an empty-but-signed thinking block must emit a chunk");
    let details = &chunk.choices[0].delta.reasoning_details;
    assert_eq!(details.len(), 1, "exactly one aggregated detail");
    assert_eq!(
        details[0].payload["text"], "",
        "the omitted-display text is empty and stays empty"
    );
    assert_eq!(
        details[0].payload["signature"], "EqoCCkYIBxgCKkASDGZvbw==",
        "the signature is the load-bearing field and must survive"
    );
    assert_eq!(
        state.completed_thinking.len(),
        1,
        "the accumulator must also capture it for replay"
    );
}

/// Positive control for the test above: the same drive with a
/// thinking_delta present also emits, so the assertion is not passing on
/// a state machine that emits unconditionally regardless of shape.
#[test]
fn populated_thinking_with_signature_emits_a_detail() {
    let mut state = SseState::default();

    send(&mut state, THINKING_START);
    send(&mut state, &thinking_delta("some thought"));
    send(&mut state, &signature_delta("EqoCCkYIBxgCKkASDGZvbw=="));
    let chunk = send(&mut state, BLOCK_STOP).expect("a populated block emits");

    let details = &chunk.choices[0].delta.reasoning_details;
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].payload["text"], "some thought");
}

/// The negative control that keeps the guard meaningful: empty text AND
/// no signature (an upstream defect, not the omitted-display shape) is
/// still dropped.
#[test]
fn empty_thinking_without_signature_is_still_dropped() {
    let mut state = SseState::default();

    send(&mut state, THINKING_START);
    let chunk = send(&mut state, BLOCK_STOP);

    assert!(
        chunk.is_none(),
        "empty AND unsigned stays dropped; got {chunk:?}"
    );
    assert!(state.completed_thinking.is_empty());
}
