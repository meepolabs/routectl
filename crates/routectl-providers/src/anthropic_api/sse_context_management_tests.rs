//! Tests for the SSE state machine's thinking-accumulation and
//! pending-cache-write logic added for context_management emulation
//! Lives as a sibling file (declared on `sse` via
//! `#[cfg(test)] #[path = ...]`) so `sse.rs` stays under the
//! project's 800-LOC ceiling. Tests retain access to private items
//! via `use super::*`.

use super::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send(state: &mut SseState, payload: &str) {
    state
        .parse_event("test-provider", payload)
        .expect("parse_event must not fail");
}

fn thinking_block_start(index: u32) -> String {
    format!(
        r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"thinking","thinking":""}}}}"#
    )
}

fn thinking_delta(index: u32, text: &str) -> String {
    format!(
        r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"thinking_delta","thinking":"{text}"}}}}"#
    )
}

fn signature_delta(index: u32, sig: &str) -> String {
    format!(
        r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"signature_delta","signature":"{sig}"}}}}"#
    )
}

fn content_block_stop(index: u32) -> String {
    format!(r#"{{"type":"content_block_stop","index":{index}}}"#)
}

fn tool_use_block_start(index: u32, id: &str) -> String {
    format!(
        r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{id}","name":"some_tool"}}}}"#
    )
}

fn text_block_start(index: u32) -> String {
    format!(
        r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"text","text":""}}}}"#
    )
}

fn message_stop() -> &'static str {
    r#"{"type":"message_stop"}"#
}

// ---------------------------------------------------------------------------
// Test 1: thinking -> tool_use populates pending_cache_writes
// ---------------------------------------------------------------------------

/// A complete thinking block followed by a tool_use block must produce
/// exactly one pending_cache_write entry whose tool_use_id and thinking
/// vec match the accumulated block.
#[test]
fn thinking_then_tool_use_populates_pending_writes() {
    // Arrange
    let mut state = SseState::default();

    // Act: drive through one full thinking block then a tool_use start.
    send(&mut state, &thinking_block_start(0));
    send(&mut state, &thinking_delta(0, "I am thinking"));
    send(&mut state, &signature_delta(0, "sig-abc"));
    send(&mut state, &content_block_stop(0));
    send(&mut state, &tool_use_block_start(1, "toolu_01"));

    // Assert
    assert_eq!(
        state.pending_cache_writes.len(),
        1,
        "expected one pending write, got: {:?}",
        state.pending_cache_writes.len()
    );
    let (ref id, ref thinking) = state.pending_cache_writes[0];
    assert_eq!(id, "toolu_01", "tool_use_id mismatch");
    assert_eq!(thinking.len(), 1, "thinking vec must have one entry");
    let detail = &thinking[0];
    assert_eq!(
        detail.payload["text"], "I am thinking",
        "thinking text mismatch in cached detail"
    );
    assert_eq!(
        detail.payload["signature"], "sig-abc",
        "thinking signature mismatch in cached detail"
    );
}

// ---------------------------------------------------------------------------
// Test 2: two thinking blocks + two tool_uses -> non-cumulative semantics
// ---------------------------------------------------------------------------

/// Non-cumulative invariant: the first tool_use sees only the thinking
/// that preceded it; the second tool_use sees only the second thinking
/// block. completed_thinking is cleared after each tool_use.
#[test]
fn two_thinking_two_tool_uses_cumulative() {
    // Arrange
    let mut state = SseState::default();

    // Act: block1 -> tool1 -> block2 -> tool2
    send(&mut state, &thinking_block_start(0));
    send(&mut state, &thinking_delta(0, "block one"));
    send(&mut state, &signature_delta(0, "s1"));
    send(&mut state, &content_block_stop(0));
    send(&mut state, &tool_use_block_start(1, "t1"));

    send(&mut state, &thinking_block_start(2));
    send(&mut state, &thinking_delta(2, "block two"));
    send(&mut state, &signature_delta(2, "s2"));
    send(&mut state, &content_block_stop(2));
    send(&mut state, &tool_use_block_start(3, "t2"));

    // Assert: two entries
    assert_eq!(
        state.pending_cache_writes.len(),
        2,
        "expected two pending writes, got {}",
        state.pending_cache_writes.len()
    );

    // Find by tool_use_id
    let entry_t1 = state
        .pending_cache_writes
        .iter()
        .find(|(id, _)| id == "t1")
        .expect("pending write for t1 missing");
    let entry_t2 = state
        .pending_cache_writes
        .iter()
        .find(|(id, _)| id == "t2")
        .expect("pending write for t2 missing");

    // t1 sees only block1
    assert_eq!(
        entry_t1.1.len(),
        1,
        "t1 must see exactly one thinking block"
    );
    assert_eq!(
        entry_t1.1[0].payload["text"], "block one",
        "t1 must reference block-one thinking"
    );

    // t2 sees only block2 (non-cumulative; block1 was cleared after t1)
    assert_eq!(
        entry_t2.1.len(),
        1,
        "t2 must see only its own thinking block (non-cumulative)"
    );
    assert_eq!(
        entry_t2.1[0].payload["text"], "block two",
        "t2 must reference block-two thinking only"
    );
}

// ---------------------------------------------------------------------------
// Test 3: no tool_use -> no pending writes
// ---------------------------------------------------------------------------

/// A thinking block followed only by a text block and message_stop must
/// NOT produce any pending_cache_writes (no tool_use seen).
#[test]
fn no_tool_use_no_pending_writes() {
    // Arrange
    let mut state = SseState::default();

    // Act
    send(&mut state, &thinking_block_start(0));
    send(&mut state, &thinking_delta(0, "some thought"));
    send(&mut state, &signature_delta(0, "sig-x"));
    send(&mut state, &content_block_stop(0));
    send(&mut state, &text_block_start(1));
    send(&mut state, &content_block_stop(1));
    send(&mut state, message_stop());

    // Assert
    assert!(
        state.pending_cache_writes.is_empty(),
        "no tool_use means no pending writes; got {} entries",
        state.pending_cache_writes.len()
    );
}

// ---------------------------------------------------------------------------
// Test 4: empty thinking block is NOT captured in completed_thinking
// ---------------------------------------------------------------------------

/// An empty thinking block (content_block_start immediately followed by
/// content_block_stop with no delta and no signature) must NOT push an
/// entry into completed_thinking. The skip-emission guard in the stop
/// handler must also skip the accumulator push.
#[test]
fn empty_thinking_not_captured() {
    // Arrange
    let mut state = SseState::default();

    // Act: start + immediate stop, no delta, no signature.
    send(&mut state, &thinking_block_start(0));
    send(&mut state, &content_block_stop(0));

    // Assert: completed_thinking stays empty.
    assert!(
        state.completed_thinking.is_empty(),
        "empty thinking block must not populate completed_thinking; got {} entries",
        state.completed_thinking.len()
    );
}

// ---------------------------------------------------------------------------
// Test 5: completed_thinking cleared at message_stop
// ---------------------------------------------------------------------------

/// After message_stop the completed_thinking accumulator must be empty so
/// the next assistant turn starts fresh.
#[test]
fn completed_thinking_cleared_at_message_stop() {
    // Arrange
    let mut state = SseState::default();

    // Build up some completed_thinking.
    send(&mut state, &thinking_block_start(0));
    send(&mut state, &thinking_delta(0, "a thought"));
    send(&mut state, &signature_delta(0, "s"));
    send(&mut state, &content_block_stop(0));

    // Precondition: completed_thinking must be non-empty before the stop.
    assert!(
        !state.completed_thinking.is_empty(),
        "precondition: completed_thinking must have one entry before message_stop"
    );

    // Act
    send(&mut state, message_stop());

    // Assert
    assert!(
        state.completed_thinking.is_empty(),
        "completed_thinking must be cleared on message_stop"
    );
}

// ---------------------------------------------------------------------------
// Test 6: tool_use with empty id skips pending_cache_writes push
// ---------------------------------------------------------------------------

/// A tool_use block whose id is an empty string must not be pushed to
/// pending_cache_writes (guard: `if !id.is_empty()`).
#[test]
fn pending_cache_writes_skips_empty_tool_use_id() {
    // Arrange
    let mut state = SseState::default();

    // Act: thinking block + tool_use with empty id.
    send(&mut state, &thinking_block_start(0));
    send(&mut state, &thinking_delta(0, "thought"));
    send(&mut state, &signature_delta(0, "sig"));
    send(&mut state, &content_block_stop(0));
    // Empty id -- the guard must prevent the push.
    send(&mut state, &tool_use_block_start(1, ""));

    // Assert
    assert!(
        state.pending_cache_writes.is_empty(),
        "empty tool_use id must not produce a pending write; got {} entries",
        state.pending_cache_writes.len()
    );
}

// ---------------------------------------------------------------------------
// Test 7: redacted_thinking block accumulates into completed_thinking
// ---------------------------------------------------------------------------

/// A `redacted_thinking` content_block_start must push a `ReasoningDetail`
/// with `kind = Encrypted` into `completed_thinking` so that a subsequent
/// `tool_use` block's `pending_cache_writes` entry includes the redacted
/// block. This mirrors the non-streaming path in `extract_tool_thinking`.
#[test]
fn redacted_thinking_then_tool_use_captures_redacted() {
    // Arrange
    let mut state = SseState::default();

    // Act: redacted_thinking block_start (no deltas follow), then tool_use.
    let redacted_start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"ENCRYPTED_PAYLOAD"}}"#;
    send(&mut state, redacted_start);
    // redacted_thinking has no delta events; open_block stays None after emit.
    send(&mut state, &tool_use_block_start(1, "toolu_redacted"));

    // Assert: one pending write entry.
    assert_eq!(
        state.pending_cache_writes.len(),
        1,
        "expected one pending write after redacted_thinking + tool_use; got {}",
        state.pending_cache_writes.len()
    );
    let (ref id, ref thinking) = state.pending_cache_writes[0];
    assert_eq!(id, "toolu_redacted", "tool_use_id mismatch");
    assert_eq!(
        thinking.len(),
        1,
        "thinking vec must have one entry from the redacted block"
    );
    let detail = &thinking[0];
    assert!(
        matches!(detail.kind, routectl_core::ReasoningDetailKind::Encrypted),
        "expected Encrypted kind, got {:?}",
        detail.kind
    );
    assert_eq!(
        detail.payload["data"], "ENCRYPTED_PAYLOAD",
        "redacted data must match the content_block_start payload"
    );
}

// ---------------------------------------------------------------------------
// Test 8: streaming non-cumulative interleaved-thinking invariant
// ---------------------------------------------------------------------------

/// Interleaved sequence: [ThinkingA, ToolUse1, ThinkingB, ToolUse2].
/// After non-cumulative fix, t1's pending write must contain only ThinkingA
/// and t2's must contain only ThinkingB (no duplicate ThinkingA in t2).
#[test]
fn streaming_interleaved_thinking_non_cumulative() {
    // Arrange
    let mut state = SseState::default();

    // Act: ThinkingA -> ToolUse1 -> ThinkingB -> ToolUse2
    send(&mut state, &thinking_block_start(0));
    send(&mut state, &thinking_delta(0, "alpha"));
    send(&mut state, &signature_delta(0, "sa"));
    send(&mut state, &content_block_stop(0));
    send(&mut state, &tool_use_block_start(1, "tu1"));

    send(&mut state, &thinking_block_start(2));
    send(&mut state, &thinking_delta(2, "beta"));
    send(&mut state, &signature_delta(2, "sb"));
    send(&mut state, &content_block_stop(2));
    send(&mut state, &tool_use_block_start(3, "tu2"));

    // Assert
    assert_eq!(state.pending_cache_writes.len(), 2);

    let entry_tu1 = state
        .pending_cache_writes
        .iter()
        .find(|(id, _)| id == "tu1")
        .expect("pending write for tu1 missing");
    let entry_tu2 = state
        .pending_cache_writes
        .iter()
        .find(|(id, _)| id == "tu2")
        .expect("pending write for tu2 missing");

    assert_eq!(entry_tu1.1.len(), 1, "tu1 must see only one thinking block");
    assert_eq!(entry_tu1.1[0].payload["text"], "alpha");

    assert_eq!(
        entry_tu2.1.len(),
        1,
        "tu2 must see only its own thinking (no duplicate alpha); got {} entries",
        entry_tu2.1.len()
    );
    assert_eq!(
        entry_tu2.1[0].payload["text"], "beta",
        "tu2 must reference beta-thinking only"
    );
}
