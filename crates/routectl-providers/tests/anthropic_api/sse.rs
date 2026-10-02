//! SSE state-machine tests: streaming deltas, tool_use, and signature aggregation.

use super::*;
use pretty_assertions::assert_eq;

/// Strategy A: streaming thinking deltas carry the live `reasoning`
/// string (no structured detail); the structured `ReasoningDetail`
/// is aggregated and emitted exactly once per thinking block at
/// `content_block_stop` with both `text` and `signature`. This is
/// what the replay path on the next-turn request expects.
#[test]
fn sse_full_stream_sequence() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    let pid = "test";
    let mut chunks = Vec::new();

    let events = vec![
        r#"{"type":"message_start","message":{"id":"msg_sse01","model":"claude-3-opus","usage":{"input_tokens":100,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_xyz789"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello world!"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":20}}"#,
        r#"{"type":"message_stop"}"#,
    ];

    for event_data in &events {
        if let Some(chunk) = state.parse_event(pid, event_data).unwrap() {
            chunks.push(chunk);
        }
    }

    // The stream opens with the role chunk (delta.role="assistant").
    assert!(matches!(
        chunks[0].choices[0].delta.role,
        Some(routectl_core::Role::Assistant)
    ));

    // Live thinking-string chunk: carries `reasoning` only. NO
    // structured ReasoningDetail (deferred to terminal chunk).
    let live = &chunks[1];
    let live_delta = &live.choices[0].delta;
    assert_eq!(live_delta.reasoning.as_deref(), Some("Let me think..."));
    assert!(
        live_delta.reasoning_details.is_empty(),
        "live thinking chunk must carry only the string; structured detail is deferred"
    );

    // Terminal aggregated detail at content_block_stop: ONE entry
    // with BOTH text and signature.
    let terminal = &chunks[2];
    let terminal_delta = &terminal.choices[0].delta;
    assert!(
        terminal_delta.reasoning.is_none(),
        "terminal chunk has only the structured detail, not the string"
    );
    assert_eq!(terminal_delta.reasoning_details.len(), 1);
    let detail = &terminal_delta.reasoning_details[0];
    assert_eq!(detail.payload["text"], "Let me think...");
    assert_eq!(detail.payload["signature"], "sig_xyz789");

    // Text content chunk follows.
    let text_chunk = &chunks[3];
    assert_eq!(
        text_chunk.choices[0].delta.content.as_deref(),
        Some("Hello world!")
    );

    // Last chunk carries finish_reason.
    let finish_chunk = chunks.last().unwrap();
    assert_eq!(
        finish_chunk.choices[0].finish_reason.as_deref(),
        Some("stop")
    );

    // State captures id and model from message_start.
    assert_eq!(state.id, "msg_sse01");
    assert_eq!(state.model, "claude-3-opus");
}

/// Edge case: missing signature_delta (Anthropic 4.5 sometimes omits
/// on tool-only thinking turns). Terminal aggregated detail still
/// emits with an empty signature; replay code skips the entry at
/// DEBUG instead of erroring.
#[test]
fn sse_thinking_block_without_signature_emits_empty_signature() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    let mut chunks = Vec::new();
    let events = vec![
        r#"{"type":"message_start","message":{"id":"m","model":"claude-haiku-4-5","usage":{"input_tokens":1,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thoughts"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_stop"}"#,
    ];
    for ev in &events {
        if let Some(c) = state.parse_event("test", ev).unwrap() {
            chunks.push(c);
        }
    }
    // [0] role chunk
    // [1] live string chunk
    // [2] terminal aggregated detail
    let terminal = &chunks[2];
    let detail = &terminal.choices[0].delta.reasoning_details[0];
    assert_eq!(detail.payload["text"], "thoughts");
    assert_eq!(detail.payload["signature"], "");
}

/// Edge case: empty thinking block (no deltas at all). Skip
/// terminal-chunk emission so replay doesn't push a doomed empty
/// Thinking block.
#[test]
fn sse_empty_thinking_block_emits_no_terminal_chunk() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    let mut chunks = Vec::new();
    let events = vec![
        r#"{"type":"message_start","message":{"id":"m","model":"claude-haiku-4-5","usage":{"input_tokens":1,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_stop"}"#,
    ];
    for ev in &events {
        if let Some(c) = state.parse_event("test", ev).unwrap() {
            chunks.push(c);
        }
    }
    // No terminal chunk for the empty block.
    assert!(
        chunks
            .iter()
            .all(|c| c.choices[0].delta.reasoning_details.is_empty()),
        "empty thinking block must not produce a structured detail"
    );
}

#[test]
fn sse_tool_use_delta_emits_tool_calls() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    let pid = "test";
    let mut chunks = Vec::new();

    let events = vec![
        r#"{"type":"message_start","message":{"id":"msg_tool","model":"claude-3-opus","usage":{"input_tokens":20,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"search"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"rust\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}"#,
    ];

    for event_data in &events {
        if let Some(chunk) = state.parse_event(pid, event_data).unwrap() {
            chunks.push(chunk);
        }
    }

    assert_eq!(
        assemble_tool_calls(&chunks),
        vec![(
            "toolu_01".to_string(),
            "search".to_string(),
            "{\"q\":\"rust\"}".to_string()
        )]
    );

    // Finish reason chunk
    let finish = chunks.last().unwrap();
    assert_eq!(
        finish.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
}

/// Stream reversal: a tool_use block whose upstream name is the
/// doubled-prefix `mcp__linear_get_issue` is reversed ONCE at
/// content_block_start via the per-request reverse map; every
/// input_json_delta chunk inherits the client's original
/// single-underscore name.
#[test]
fn sse_tool_use_name_reversed_via_reverse_map() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    state.tool_reverse.insert(
        "mcp__linear_get_issue".to_string(),
        "mcp_linear_get_issue".to_string(),
    );
    let pid = "test";
    let mut chunks = Vec::new();

    let events = vec![
        r#"{"type":"message_start","message":{"id":"msg_mcp","model":"claude-opus-4-8","usage":{"input_tokens":20,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"mcp__linear_get_issue"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"id\":1}"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"more"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
    ];

    for event_data in &events {
        if let Some(chunk) = state.parse_event(pid, event_data).unwrap() {
            chunks.push(chunk);
        }
    }

    // Every emitted tool-call chunk reads the client's original name.
    let tool_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| c.choices[0].delta.tool_calls.is_some())
        .collect();
    assert!(
        tool_chunks.len() >= 2,
        "expected a tool-call chunk per input_json_delta"
    );
    for c in &tool_chunks {
        let tc = c.choices[0].delta.tool_calls.as_ref().unwrap();
        assert_eq!(
            tc[0]["function"]["name"], "mcp_linear_get_issue",
            "reversed name must ride on the start AND every delta chunk"
        );
    }
}

/// Stream reversal for a BARE tool name: a tool_use block whose upstream
/// name is the prefixed `mcp__read_file` (the cloak's forward form for a
/// bare `read_file`) is reversed ONCE at content_block_start via the
/// per-request reverse map; every input_json_delta chunk inherits the
/// client's original bare name.
#[test]
fn sse_bare_tool_use_name_reversed_via_reverse_map() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    state
        .tool_reverse
        .insert("mcp__read_file".to_string(), "read_file".to_string());
    let pid = "test";
    let mut chunks = Vec::new();

    let events = vec![
        r#"{"type":"message_start","message":{"id":"msg_bare","model":"claude-opus-4-8","usage":{"input_tokens":20,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"mcp__read_file"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"/x\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
    ];

    for event_data in &events {
        if let Some(chunk) = state.parse_event(pid, event_data).unwrap() {
            chunks.push(chunk);
        }
    }

    let tool_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| c.choices[0].delta.tool_calls.is_some())
        .collect();
    assert!(
        tool_chunks.len() >= 2,
        "expected a tool-call chunk per input_json_delta"
    );
    for c in &tool_chunks {
        let tc = c.choices[0].delta.tool_calls.as_ref().unwrap();
        assert_eq!(
            tc[0]["function"]["name"], "read_file",
            "reversed bare name must ride on the start AND every delta chunk"
        );
    }
}

/// Stream contract: `server_tool_use` on the closing
/// `message_delta.usage` lands on the canonical chunk usage.
#[test]
fn sse_server_tool_use_lands_on_chunk_usage() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    let pid = "test";
    let mut chunks = Vec::new();
    let events = vec![
        r#"{"type":"message_start","message":{"id":"msg_stu","model":"claude-3-opus","usage":{"input_tokens":20,"output_tokens":0}}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7,"server_tool_use":{"web_search_requests":5}}}"#,
    ];
    for event_data in &events {
        if let Some(chunk) = state.parse_event(pid, event_data).unwrap() {
            chunks.push(chunk);
        }
    }
    let closing = chunks.last().unwrap();
    let usage = closing.usage.as_ref().expect("usage on closing chunk");
    assert_eq!(
        usage.server_tool_use,
        Some(json!({"web_search_requests": 5}))
    );
}

#[test]
fn sse_unknown_events_return_none() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    let pid = "test";

    // Ping event
    let result = state.parse_event(pid, r#"{"type":"ping"}"#).unwrap();
    assert!(result.is_none());

    // message_stop
    let result = state
        .parse_event(pid, r#"{"type":"message_stop"}"#)
        .unwrap();
    assert!(result.is_none());
}

/// Assemble the canonical tool calls a chunk stream describes the way
/// every ingress does: group fragments by `index`, keep the last
/// non-empty id and name, concatenate the argument fragments.
fn assemble_tool_calls(chunks: &[routectl_core::ChatChunk]) -> Vec<(String, String, String)> {
    let mut calls: std::collections::BTreeMap<u64, (String, String, String)> =
        std::collections::BTreeMap::new();
    for tc in chunks
        .iter()
        .flat_map(|c| c.choices.iter())
        .filter_map(|choice| choice.delta.tool_calls.as_ref())
        .flatten()
    {
        let index = tc["index"].as_u64().expect("tool call carries an index");
        let entry = calls.entry(index).or_default();
        if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
            entry.0 = id.to_string();
        }
        if let Some(name) = tc["function"]["name"].as_str().filter(|s| !s.is_empty()) {
            entry.1 = name.to_string();
        }
        if let Some(args) = tc["function"]["arguments"].as_str() {
            entry.2.push_str(args);
        }
    }
    calls.into_values().collect()
}

fn parse_all(
    state: &mut routectl_providers::anthropic_api::sse::SseState,
    events: &[&str],
) -> Vec<routectl_core::ChatChunk> {
    events
        .iter()
        .filter_map(|event| state.parse_event("test", event).expect("event parses"))
        .collect()
}

/// A tool_use block that opens and closes with no `input_json_delta`
/// (a zero-argument call relayed by an upstream that omits the empty
/// delta) still yields its tool call, with empty arguments.
#[test]
fn sse_tool_use_without_input_delta_yields_one_call_with_empty_arguments() {
    use routectl_providers::anthropic_api::sse::SseState;

    // Arrange
    let mut state = SseState::default();
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_zero","model":"claude-opus-4-8","usage":{"input_tokens":20,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_zero","name":"list_agents","input":{}}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}"#,
    ];

    // Act
    let chunks = parse_all(&mut state, &events);

    // Assert
    assert_eq!(
        assemble_tool_calls(&chunks),
        vec![(
            "toolu_zero".to_string(),
            "list_agents".to_string(),
            String::new()
        )]
    );
}

/// Input deltas that follow the block opener append to the same call:
/// one call whose arguments are the exact concatenation of the
/// fragments, never a second call and never a stray prefix.
#[test]
fn sse_tool_use_with_fragmented_input_yields_one_call_with_concatenated_arguments() {
    use routectl_providers::anthropic_api::sse::SseState;

    // Arrange
    let mut state = SseState::default();
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_frag","model":"claude-opus-4-8","usage":{"input_tokens":20,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_frag","name":"search","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":":1}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
    ];

    // Act
    let chunks = parse_all(&mut state, &events);

    // Assert
    assert_eq!(
        assemble_tool_calls(&chunks),
        vec![(
            "toolu_frag".to_string(),
            "search".to_string(),
            r#"{"a":1}"#.to_string()
        )]
    );
}

/// The cloak reverse map applies to a zero-argument call too: the call
/// emitted at block open already carries the client's original name.
#[test]
fn sse_zero_argument_tool_use_carries_reversed_name() {
    use routectl_providers::anthropic_api::sse::SseState;

    // Arrange
    let mut state = SseState::default();
    state
        .tool_reverse
        .insert("mcp__list_agents".to_string(), "list_agents".to_string());
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_rev","model":"claude-opus-4-8","usage":{"input_tokens":20,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_rev","name":"mcp__list_agents","input":{}}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
    ];

    // Act
    let chunks = parse_all(&mut state, &events);

    // Assert
    assert_eq!(
        assemble_tool_calls(&chunks),
        vec![(
            "toolu_rev".to_string(),
            "list_agents".to_string(),
            String::new()
        )]
    );
}
