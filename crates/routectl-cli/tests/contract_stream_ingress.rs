//! Contract tests for the streaming-side ingress layer.
//!
//! Each scenario hand-builds a `Vec<ChatChunk>` representing what the
//! egress would surface, drives `IngressAdapter::render_chunk` +
//! `render_eos` to produce SSE events, and asserts the resulting
//! event names + parsed JSON data fields explicitly (NOT snapshots --
//! JSON Map field ordering inside an SSE `data:` payload is
//! non-deterministic and a snapshot would be flaky).
//!
//! This file pairs with `contract_stream_egress.rs` in the
//! `routectl-providers` crate (the wire-to-canonical half). It pins
//! ONLY the bug class the contract suite must guard against:
//!
//!   - Bug B class: `message_delta`/`message_stop` ordering on the
//!     Anthropic ingress (the egress can emit a finish_reason chunk
//!     and a separate trailing usage chunk; the ingress must
//!     buffer the finish_reason and emit `message_delta + message_stop`
//!     exactly once, in the right order, carrying both stop_reason
//!     AND usage).
//!
//! Scope: only the Anthropic ingress's `render_chunk` is exercised
//! here. The OpenAI ingress's `render_chunk` is a bare-data-frame
//! pass-through (it serializes the canonical chunk to JSON and emits
//! one unnamed `data: <json>` frame) so it carries no translation
//! logic worth pinning. See the existing unit tests
//! `render_chunk_emits_single_unnamed_data_frame` and
//! `render_eos_emits_done_sentinel` in `openai.rs` for that coverage.

use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::{IngressAdapter, SseEvent};
use routectl_core::{ChatChunk, ChunkChoice, ChunkDelta, UsageDelta};
use serde_json::Value;

// ---------------------------------------------------------------------
// Hand-built canonical chunks
// ---------------------------------------------------------------------
//
// These helpers mirror the canonical shape an openai-compat egress
// surfaces for a multi-token completion: a leading content chunk, a
// second content chunk, a separate finish_reason-only chunk
// (no usage yet), and a trailing usage-only chunk. The Anthropic
// ingress must reassemble this into the strict Anthropic event order.

/// Content-only chunk. Carries one text delta and no finish_reason.
fn content_chunk(text: &str) -> ChatChunk {
    ChatChunk {
        id: "msg_s7".into(),
        model: "claude-3-opus".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

/// Finish-reason-only chunk: empty content delta, no usage. The
/// Anthropic ingress must buffer this `fr` until usage arrives (or
/// `render_eos` runs) so the wire-side `message_delta + message_stop`
/// pair fires exactly once.
fn finish_only_chunk(fr: &str) -> ChatChunk {
    ChatChunk {
        id: "msg_s7".into(),
        model: "claude-3-opus".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some(fr.into()),
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

/// Usage-only trailing chunk. Many openai-compat hosts emit this as
/// the LAST chunk after a finish_reason-only chunk; the Anthropic
/// ingress flushes its buffered fr now and emits the terminal
/// `message_delta + message_stop` pair.
fn usage_only_chunk(prompt: u32, completion: u32) -> ChatChunk {
    ChatChunk {
        id: "msg_s7".into(),
        model: "claude-3-opus".into(),
        choices: vec![],
        usage: Some(UsageDelta {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(prompt + completion),
            ..Default::default()
        }),
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

/// Pump a sequence of canonical chunks through the ingress and append
/// `render_eos` events. Returns the flat SSE event list in wire order.
fn render_all(chunks: Vec<ChatChunk>) -> Vec<SseEvent> {
    let ingress = AnthropicIngress;
    let mut state = ingress.new_stream_state();
    let mut events: Vec<SseEvent> = Vec::new();
    for c in chunks {
        events.extend(
            ingress
                .render_chunk(c, state.as_mut())
                .expect("render_chunk"),
        );
    }
    events.extend(ingress.render_eos(state.as_mut()));
    events
}

/// Extract the `event:` field for each event in arrival order. Unnamed
/// events (which Anthropic never emits) panic.
fn event_names(events: &[SseEvent]) -> Vec<&str> {
    events
        .iter()
        .map(|e| {
            e.event
                .as_deref()
                .expect("Anthropic ingress must emit named SSE events")
        })
        .collect()
}

/// Parse the `data:` JSON payload of one event. Panics on parse
/// failure so the test surfaces a clear error rather than a silent
/// pass.
fn parse_data(ev: &SseEvent) -> Value {
    serde_json::from_str(&ev.data).expect("event data is valid JSON")
}

// =====================================================================
// Scenario 7: basic_stream_sequence
// =====================================================================
//
// Bug B class guard. The Anthropic ingress must emit events in this
// strict order for a two-token completion that finishes with a
// trailing usage chunk:
//
//   message_start
//   content_block_start            (text block, index 0)
//   content_block_delta            (text_delta "Hello")
//   content_block_delta            (text_delta " world")
//   content_block_stop             (index 0)
//   message_delta                  (stop_reason + usage, exactly once)
//   message_stop
//
// The trick the ingress must handle: the finish_reason arrives on a
// chunk BEFORE the usage chunk. A naive emit-immediately renderer
// would emit `message_delta(stop, None)` -> `message_stop` -> and
// then a SECOND `message_delta(None, usage)` AFTER `message_stop`,
// which is a protocol violation. The correct behavior buffers the
// finish_reason and flushes `message_delta + message_stop` ONCE when
// the usage arrives.

#[test]
fn anthropic_ingress_basic_stream_sequence() {
    let events = render_all(vec![
        content_chunk("Hello"),
        content_chunk(" world"),
        finish_only_chunk("stop"),
        usage_only_chunk(5, 7),
    ]);
    let names = event_names(&events);

    // Exact wire order. Any deviation here is a protocol violation.
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "Anthropic ingress emitted events in wrong order: {names:?}"
    );

    // Text deltas carry the expected payload, in order. Index into
    // the events list using the matched names above.
    let first_delta = parse_data(&events[2]);
    assert_eq!(first_delta["type"], "content_block_delta");
    assert_eq!(first_delta["delta"]["type"], "text_delta");
    assert_eq!(first_delta["delta"]["text"], "Hello");
    let second_delta = parse_data(&events[3]);
    assert_eq!(second_delta["delta"]["type"], "text_delta");
    assert_eq!(second_delta["delta"]["text"], " world");

    // The single message_delta must carry BOTH stop_reason and usage
    // -- the Bug B class fail mode emits two separate message_delta
    // events (one for stop_reason, one for usage), with the
    // usage-bearing one landing AFTER message_stop.
    let msg_delta = parse_data(&events[5]);
    assert_eq!(msg_delta["type"], "message_delta");
    assert_eq!(
        msg_delta["delta"]["stop_reason"], "end_turn",
        "canonical `stop` must round-trip to Anthropic wire `end_turn`",
    );
    assert!(
        msg_delta.get("usage").is_some(),
        "message_delta must carry usage in the same event (not a trailing one); got: {msg_delta}"
    );
    // Anthropic's wire `usage.output_tokens` mirrors completion_tokens.
    assert_eq!(
        msg_delta["usage"]["output_tokens"], 7,
        "wire usage.output_tokens must reflect canonical completion_tokens",
    );

    // message_stop is the terminal event with no surplus payload.
    let msg_stop = parse_data(&events[6]);
    assert_eq!(msg_stop["type"], "message_stop");

    // Exactly one message_delta + exactly one message_stop. A repeat
    // of either is the Bug B class failure.
    assert_eq!(
        names.iter().filter(|n| **n == "message_delta").count(),
        1,
        "exactly one message_delta expected; events: {names:?}"
    );
    assert_eq!(
        names.iter().filter(|n| **n == "message_stop").count(),
        1,
        "exactly one message_stop expected; events: {names:?}"
    );
}
