//! Relocated test surface for `sse.rs`'s message-level usage
//! accounting. Lives as a sibling file (declared on `sse` via
//! `#[cfg(test)] #[path = ...] mod sse_usage_tests;`) so the parent
//! state machine stays under the project's 800-LOC ceiling without
//! touching the well-tested production usage path. The tests here
//! drive `SseState::parse_event` end-to-end and pin the same
//! invariants they did in-tree before being extracted.

use super::*;

/// `message_start.usage` carries the input side of token accounting.
/// Real Anthropic emits non-zero `input_tokens` plus cache numbers
/// here; some upstream variants (and routectl's own Anthropic
/// ingress today) emit zeros that get corrected later in
/// `message_delta`. The state must capture whatever's in
/// `message_start.usage` so the closing chunk can sum them into
/// `prompt_tokens`.
#[test]
fn message_start_captures_input_usage_for_summing() {
    let mut state = SseState::default();
    let payload = r#"{
        "type":"message_start",
        "message": {
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": "claude-opus-4-7",
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": 100,
                "output_tokens": 0,
                "cache_creation_input_tokens": 200,
                "cache_read_input_tokens": 300
            }
        }
    }"#;
    let _ = state.parse_event("test", payload).unwrap();
    let cap = state
        .captured_input_usage
        .as_ref()
        .expect("input usage captured from message_start");
    assert_eq!(cap.input_tokens, 100);
    assert_eq!(cap.cache_creation_input_tokens, Some(200));
    assert_eq!(cap.cache_read_input_tokens, Some(300));
    // sum surfaces as the prompt_tokens helper.
    assert_eq!(cap.prompt_tokens(), 600);
}

/// Closing `message_delta` chunk must carry the full
/// `prompt_tokens` (sum of input + cache_creation + cache_read)
/// so OpenAI clients see the cumulative context size at end-of-
/// stream, not zero (the prior bug) or just the new turn's
/// non-cached count.
#[test]
fn message_delta_emits_prompt_tokens_from_captured_input() {
    let mut state = SseState::default();
    // message_start with non-trivial input + cache numbers.
    let _ = state
        .parse_event(
            "test",
            r#"{
                "type":"message_start",
                "message": {
                    "id":"msg_01","type":"message","role":"assistant",
                    "content":[],"model":"claude-opus-4-7",
                    "stop_reason":null,"stop_sequence":null,
                    "usage": {
                        "input_tokens": 50,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 100,
                        "cache_read_input_tokens": 200
                    }
                }
            }"#,
        )
        .unwrap();
    // message_delta with output usage and stop_reason.
    let chunk = state
        .parse_event(
            "test",
            r#"{
                "type":"message_delta",
                "delta": {"stop_reason":"end_turn","stop_sequence":null},
                "usage": {"output_tokens": 25}
            }"#,
        )
        .unwrap()
        .expect("closing chunk emitted");
    let usage = chunk.usage.expect("usage on closing chunk");
    // 50 + 100 + 200 = 350
    assert_eq!(usage.prompt_tokens, Some(350));
    assert_eq!(usage.completion_tokens, Some(25));
    assert_eq!(usage.total_tokens, Some(375));
}

/// When `message_delta.usage.input_tokens` is present (e.g. from
/// routectl's own Anthropic ingress emitting the post-cache total),
/// prefer it over the captured `message_start` value. This is the
/// "chained routectl" scenario: an upstream routectl renders the
/// final input count to `message_delta.usage.input_tokens` because
/// `message_start.usage` was hardcoded to zero.
#[test]
fn message_delta_input_tokens_overrides_captured_zero() {
    let mut state = SseState::default();
    // message_start with zero input (the upstream-hardcoded case).
    let _ = state
        .parse_event(
            "test",
            r#"{
                "type":"message_start",
                "message": {
                    "id":"msg_01","type":"message","role":"assistant",
                    "content":[],"model":"claude-opus-4-7",
                    "stop_reason":null,"stop_sequence":null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }"#,
        )
        .unwrap();
    let chunk = state
        .parse_event(
            "test",
            r#"{
                "type":"message_delta",
                "delta": {"stop_reason":"end_turn","stop_sequence":null},
                "usage": {
                    "input_tokens": 12345,
                    "output_tokens": 50,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }"#,
        )
        .unwrap()
        .expect("closing chunk emitted");
    let usage = chunk.usage.expect("usage");
    assert_eq!(usage.prompt_tokens, Some(12345));
    assert_eq!(usage.completion_tokens, Some(50));
}

/// Pin the chained-routectl invariant: when both message_start AND
/// message_delta carry non-zero input_tokens, the delta value
/// (which an upstream routectl writes as the already-summed
/// prompt_tokens) wins. Use DISTINCT values so the test
/// distinguishes "delta wins" from "captured wins" -- a regression
/// flipping the match arm order would fail this test.
#[test]
fn message_delta_input_tokens_wins_over_captured_when_both_nonzero() {
    let mut state = SseState::default();
    let _ = state
        .parse_event(
            "test",
            r#"{
                "type":"message_start",
                "message": {
                    "id":"msg_01","type":"message","role":"assistant",
                    "content":[],"model":"claude-opus-4-7",
                    "stop_reason":null,"stop_sequence":null,
                    "usage": {"input_tokens": 50, "output_tokens": 0}
                }
            }"#,
        )
        .unwrap();
    // Delta sends a DIFFERENT non-zero input count (the chained-
    // upstream's pre-summed value, e.g. after a cache hit on the
    // upstream side that wasn't visible to our message_start).
    let chunk = state
        .parse_event(
            "test",
            r#"{
                "type":"message_delta",
                "delta": {"stop_reason":"end_turn","stop_sequence":null},
                "usage": {"input_tokens": 99, "output_tokens": 10}
            }"#,
        )
        .unwrap()
        .expect("closing chunk emitted");
    let usage = chunk.usage.expect("usage");
    // 99 (delta) wins, NOT 50 (captured) and NOT 149 (sum).
    assert_eq!(usage.prompt_tokens, Some(99));
    assert_eq!(usage.completion_tokens, Some(10));
    assert_eq!(usage.total_tokens, Some(109));
}

/// Per-spec semantics: when a delta carries RAW `input_tokens`
/// alongside non-zero `cache_creation_input_tokens` and
/// `cache_read_input_tokens`, the canonical prompt_tokens MUST be
/// the sum of all three. Pre-fix, routectl assumed the delta's
/// `input_tokens` was already-summed and silently undercounted
/// when a non-routectl Anthropic upstream sent raw values.
#[test]
fn message_delta_sums_raw_input_plus_cache_per_spec() {
    let mut state = SseState::default();
    let _ = state
        .parse_event(
            "test",
            r#"{
                "type":"message_start",
                "message": {
                    "id":"msg_01","type":"message","role":"assistant",
                    "content":[],"model":"claude-opus-4-7",
                    "stop_reason":null,"stop_sequence":null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }"#,
        )
        .unwrap();
    // Hypothetical non-routectl Anthropic upstream: sends RAW
    // input_tokens on message_delta, NOT pre-summed. cache fields
    // are separate (per Anthropic spec).
    let chunk = state
        .parse_event(
            "test",
            r#"{
                "type":"message_delta",
                "delta": {"stop_reason":"end_turn","stop_sequence":null},
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "cache_creation_input_tokens": 200,
                    "cache_read_input_tokens": 300
                }
            }"#,
        )
        .unwrap()
        .expect("closing chunk emitted");
    let usage = chunk.usage.expect("usage");
    // 100 raw + 200 cache_creation + 300 cache_read = 600
    assert_eq!(usage.prompt_tokens, Some(600));
    assert_eq!(usage.completion_tokens, Some(50));
    assert_eq!(usage.total_tokens, Some(650));
}

/// Pin the cache-merge zero-aware fallback: if the closing
/// `message_delta.usage` restates `cache_*` as explicit zero while
/// `message_start` had non-zero captured values, keep the captured
/// values. Otherwise a placeholder restatement would erase real
/// cache stats.
#[test]
fn message_delta_cache_zero_does_not_overwrite_captured_nonzero() {
    let mut state = SseState::default();
    let _ = state
        .parse_event(
            "test",
            r#"{
                "type":"message_start",
                "message": {
                    "id":"msg_01","type":"message","role":"assistant",
                    "content":[],"model":"claude-opus-4-7",
                    "stop_reason":null,"stop_sequence":null,
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 100,
                        "cache_read_input_tokens": 200
                    }
                }
            }"#,
        )
        .unwrap();
    let chunk = state
        .parse_event(
            "test",
            r#"{
                "type":"message_delta",
                "delta": {"stop_reason":"end_turn","stop_sequence":null},
                "usage": {
                    "output_tokens": 7,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }"#,
        )
        .unwrap()
        .expect("closing chunk");
    let usage = chunk.usage.expect("usage");
    assert_eq!(usage.cache_creation_input_tokens, Some(100));
    assert_eq!(usage.cache_read_input_tokens, Some(200));
}

/// Pin the per-TTL `cache_creation` field-level merge: a delta
/// carrying a partial or empty `cache_creation` object must NOT
/// wholesale-replace the captured object's per-TTL detail. Each
/// field falls back independently through the same zero-aware
/// pick.
#[test]
fn message_delta_partial_cache_creation_object_merges_per_ttl() {
    let mut state = SseState::default();
    // message_start with both TTL buckets set.
    let _ = state
        .parse_event(
            "test",
            r#"{
                "type":"message_start",
                "message": {
                    "id":"msg_01","type":"message","role":"assistant",
                    "content":[],"model":"claude-opus-4-7",
                    "stop_reason":null,"stop_sequence":null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": 50,
                            "ephemeral_1h_input_tokens": 100
                        }
                    }
                }
            }"#,
        )
        .unwrap();
    // Delta restates ONLY the 5m bucket (e.g. an upstream that
    // tracks 5m only); 1h is absent in the wire payload.
    let chunk = state
        .parse_event(
            "test",
            r#"{
                "type":"message_delta",
                "delta": {"stop_reason":"end_turn","stop_sequence":null},
                "usage": {
                    "output_tokens": 5,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 75
                    }
                }
            }"#,
        )
        .unwrap()
        .expect("closing chunk");
    let usage = chunk.usage.expect("usage");
    let cc = usage.cache_creation.expect("cache_creation present");
    // 5m: delta's 75 wins over captured 50.
    assert_eq!(cc.ephemeral_5m_input_tokens, Some(75));
    // 1h: absent from delta, falls back to captured 100. Pre-fix
    // this would be None (whole-object replacement lost it).
    assert_eq!(cc.ephemeral_1h_input_tokens, Some(100));
}

/// Anthropic reports server-side tool invocation counts in
/// `message_delta.usage.server_tool_use`. The streaming usage
/// normalizer must lift that object onto the canonical chunk usage
/// so the usage-accounting layer sees it at end-of-stream.
#[test]
fn message_delta_carries_server_tool_use_onto_chunk_usage() {
    let mut state = SseState::default();
    let _ = state
        .parse_event(
            "test",
            r#"{
                "type":"message_start",
                "message": {
                    "id":"msg_01","type":"message","role":"assistant",
                    "content":[],"model":"claude-opus-4-7",
                    "stop_reason":null,"stop_sequence":null,
                    "usage": {"input_tokens": 10, "output_tokens": 0}
                }
            }"#,
        )
        .unwrap();
    let chunk = state
        .parse_event(
            "test",
            r#"{
                "type":"message_delta",
                "delta": {"stop_reason":"end_turn","stop_sequence":null},
                "usage": {
                    "output_tokens": 5,
                    "server_tool_use": {"web_search_requests": 4}
                }
            }"#,
        )
        .unwrap()
        .expect("closing chunk");
    let usage = chunk.usage.expect("usage");
    assert_eq!(
        usage.server_tool_use,
        Some(serde_json::json!({"web_search_requests": 4})),
        "server_tool_use must ride the canonical chunk usage"
    );
}

/// Absent `server_tool_use` on the streamed usage leaves the canonical
/// chunk field None (no regression on the common path).
#[test]
fn message_delta_without_server_tool_use_leaves_field_none() {
    let mut state = SseState::default();
    let _ = state
        .parse_event(
            "test",
            r#"{
                "type":"message_start",
                "message": {
                    "id":"msg_01","type":"message","role":"assistant",
                    "content":[],"model":"claude-opus-4-7",
                    "stop_reason":null,"stop_sequence":null,
                    "usage": {"input_tokens": 10, "output_tokens": 0}
                }
            }"#,
        )
        .unwrap();
    let chunk = state
        .parse_event(
            "test",
            r#"{
                "type":"message_delta",
                "delta": {"stop_reason":"end_turn","stop_sequence":null},
                "usage": {"output_tokens": 5}
            }"#,
        )
        .unwrap()
        .expect("closing chunk");
    let usage = chunk.usage.expect("usage");
    assert_eq!(usage.server_tool_use, None);
}
