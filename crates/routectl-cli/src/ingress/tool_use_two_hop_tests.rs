//! A tool_use survives routectl consuming routectl's own Anthropic stream:
//! upstream events -> real Anthropic SSE parser -> Anthropic ingress render
//! (the back hop's wire output) -> a fresh parser -> Anthropic ingress render
//! again (what the client sees). Assertions are on the assembled tool_use
//! blocks the client would reconstruct, not on chunk or event counts.

use super::anthropic::AnthropicIngress;
use super::openai_responses::ResponsesIngress;
use super::{IngressAdapter, SseEvent, StreamRequestContext};
use routectl_core::ChatChunk;
use routectl_providers::anthropic_api::sse::SseState;
use serde_json::{Value, json};

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"msg_hop","type":"message","role":"assistant","content":[],"model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":1}}}"#;
const TOOL_USE_STOP: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}"#;
const MESSAGE_STOP: &str = r#"{"type":"message_stop"}"#;

/// One client-visible tool_use block: id, name, and its input as the
/// client reassembles it (the start event's `input` unless deltas follow,
/// in which case the concatenated `partial_json`).
#[derive(Debug, PartialEq)]
struct AssembledToolUse {
    id: String,
    name: String,
    input: Value,
}

fn tool_use_start(index: u32, id: &str, name: &str) -> String {
    format!(
        r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}}}"#
    )
}

fn input_delta(index: u32, partial_json: &str) -> String {
    json!({
        "type": "content_block_delta",
        "index": index,
        "delta": {"type": "input_json_delta", "partial_json": partial_json},
    })
    .to_string()
}

fn block_stop(index: u32) -> String {
    format!(r#"{{"type":"content_block_stop","index":{index}}}"#)
}

fn single_tool_stream(deltas: &[&str]) -> Vec<String> {
    let mut events = vec![
        MESSAGE_START.to_string(),
        tool_use_start(0, "toolu_hop", "list_agents"),
    ];
    events.extend(deltas.iter().map(|d| input_delta(0, d)));
    events.extend([
        block_stop(0),
        TOOL_USE_STOP.to_string(),
        MESSAGE_STOP.to_string(),
    ]);
    events
}

fn parse<S: AsRef<str>>(events: &[S]) -> Vec<ChatChunk> {
    let mut state = SseState::default();
    events
        .iter()
        .filter_map(|event| {
            state
                .parse_event("test", event.as_ref())
                .expect("event parses")
        })
        .collect()
}

fn render(adapter: &dyn IngressAdapter, chunks: Vec<ChatChunk>) -> Vec<SseEvent> {
    let mut state = adapter.new_stream_state(&StreamRequestContext::default());
    let mut events: Vec<SseEvent> = chunks
        .into_iter()
        .flat_map(|chunk| {
            adapter
                .render_chunk(chunk, state.as_mut())
                .expect("renders")
        })
        .collect();
    events.extend(adapter.render_eos(state.as_mut()));
    events
}

fn data_of(events: &[SseEvent]) -> Vec<String> {
    events.iter().map(|e| e.data.clone()).collect()
}

fn two_hops(upstream: &[String]) -> Vec<SseEvent> {
    let back_hop = render(&AnthropicIngress, parse(upstream));
    render(&AnthropicIngress, parse(&data_of(&back_hop)))
}

fn assembled_tool_uses(events: &[SseEvent]) -> Vec<AssembledToolUse> {
    let mut blocks: Vec<(u64, AssembledToolUse, String)> = Vec::new();
    for body in events
        .iter()
        .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
    {
        match body["type"].as_str() {
            Some("content_block_start") if body["content_block"]["type"] == "tool_use" => {
                let block = &body["content_block"];
                blocks.push((
                    body["index"].as_u64().expect("block index"),
                    AssembledToolUse {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        input: block["input"].clone(),
                    },
                    String::new(),
                ));
            }
            Some("content_block_delta") if body["delta"]["type"] == "input_json_delta" => {
                let index = body["index"].as_u64();
                let (_, _, partial) = blocks
                    .iter_mut()
                    .find(|(i, _, _)| Some(*i) == index)
                    .expect("input delta follows its tool_use start");
                partial.push_str(body["delta"]["partial_json"].as_str().unwrap_or_default());
            }
            _ => {}
        }
    }
    blocks
        .into_iter()
        .map(|(_, mut block, partial)| {
            if !partial.is_empty() {
                block.input = serde_json::from_str(&partial).expect("assembled input is JSON");
            }
            block
        })
        .collect()
}

fn stop_reason(events: &[SseEvent]) -> Option<String> {
    events
        .iter()
        .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
        .find(|body| body["type"] == "message_delta")
        .and_then(|body| body["delta"]["stop_reason"].as_str().map(str::to_string))
}

fn zero_arg_list_agents() -> Vec<AssembledToolUse> {
    vec![AssembledToolUse {
        id: "toolu_hop".to_string(),
        name: "list_agents".to_string(),
        input: json!({}),
    }]
}

#[test]
fn zero_argument_tool_use_without_input_delta_survives_two_hops() {
    // Arrange
    let upstream = single_tool_stream(&[]);

    // Act
    let client_view = two_hops(&upstream);

    // Assert
    assert_eq!(assembled_tool_uses(&client_view), zero_arg_list_agents());
    assert_eq!(stop_reason(&client_view).as_deref(), Some("tool_use"));
}

#[test]
fn zero_argument_tool_use_with_empty_input_delta_survives_two_hops() {
    // Arrange
    let upstream = single_tool_stream(&[""]);

    // Act
    let client_view = two_hops(&upstream);

    // Assert
    assert_eq!(assembled_tool_uses(&client_view), zero_arg_list_agents());
    assert_eq!(stop_reason(&client_view).as_deref(), Some("tool_use"));
}

#[test]
fn fragmented_tool_input_survives_two_hops_as_exact_concatenation() {
    // Arrange
    let upstream = single_tool_stream(&["", r#"{"a""#, ":1}"]);

    // Act
    let client_view = two_hops(&upstream);

    // Assert
    assert_eq!(
        assembled_tool_uses(&client_view),
        vec![AssembledToolUse {
            id: "toolu_hop".to_string(),
            name: "list_agents".to_string(),
            input: json!({"a": 1}),
        }]
    );
    let partials: String = client_view
        .iter()
        .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
        .filter(|body| body["delta"]["type"] == "input_json_delta")
        .filter_map(|body| body["delta"]["partial_json"].as_str().map(str::to_string))
        .collect();
    assert_eq!(partials, r#"{"a":1}"#);
}

/// Responses `function_call` items from the `response.output_item.done`
/// events, as (call_id, name, arguments).
fn responses_function_calls(events: &[SseEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter(|e| e.event.as_deref() == Some("response.output_item.done"))
        .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
        .map(|body| body["item"].clone())
        .filter(|item| item["type"] == "function_call")
        .map(|item| {
            let field = |k: &str| item[k].as_str().unwrap_or_default().to_string();
            (field("call_id"), field("name"), field("arguments"))
        })
        .collect()
}

#[test]
fn responses_ingress_renders_zero_argument_and_argument_bearing_calls() {
    // Arrange
    let upstream = vec![
        MESSAGE_START.to_string(),
        tool_use_start(0, "toolu_zero", "list_agents"),
        block_stop(0),
        tool_use_start(1, "toolu_args", "search"),
        input_delta(1, r#"{"q":"#),
        input_delta(1, r#""rust"}"#),
        block_stop(1),
        TOOL_USE_STOP.to_string(),
        MESSAGE_STOP.to_string(),
    ];

    // Act
    let events = render(&ResponsesIngress, parse(&upstream));

    // Assert
    assert_eq!(
        responses_function_calls(&events),
        vec![
            (
                "toolu_zero".to_string(),
                "list_agents".to_string(),
                String::new()
            ),
            (
                "toolu_args".to_string(),
                "search".to_string(),
                r#"{"q":"rust"}"#.to_string()
            ),
        ]
    );
}
