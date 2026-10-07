//! Provider audit regressions using the same string-concatenation contract as
//! OpenAI Chat SDK accumulators (not overwrite/deduplication of metadata).

use std::collections::BTreeMap;

use routectl_core::{ChatChunk, ChatRequest};
use serde_json::{Value, json};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Call {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Default)]
pub struct SdkAccumulator(pub(crate) BTreeMap<(u32, u32), Call>);

impl SdkAccumulator {
    pub(crate) fn push(&mut self, chunk: &ChatChunk) {
        for choice in &chunk.choices {
            for tool in choice.delta.tool_calls.iter().flatten() {
                let index = tool["index"].as_u64().expect("indexed delta") as u32;
                let call = self.0.entry((choice.index, index)).or_default();
                call.id.push_str(tool["id"].as_str().unwrap_or_default());
                call.name
                    .push_str(tool["function"]["name"].as_str().unwrap_or_default());
                call.arguments
                    .push_str(tool["function"]["arguments"].as_str().unwrap_or_default());
            }
        }
    }

    pub(crate) fn assert_call(&self, choice: u32, index: u32, id: &str, name: &str, args: &str) {
        assert_eq!(
            self.0.get(&(choice, index)),
            Some(&Call {
                id: id.into(),
                name: name.into(),
                arguments: args.into(),
            })
        );
    }
}

pub fn request(messages: Value) -> ChatRequest {
    serde_json::from_value(json!({"model":"gemini-2.5-pro", "messages":messages})).unwrap()
}

#[cfg(feature = "openai-responses")]
#[test]
fn responses_interleaved_tools_are_sdk_concatenable_including_empty_calls() {
    use crate::openai_responses::sse::ResponsesStreamState;
    let mut state = ResponsesStreamState::default();
    let mut sdk = SdkAccumulator::default();
    for (idx, id, name) in [
        (7, "call_a", "alpha"),
        (2, "call_b", "beta"),
        (9, "call_c", "empty"),
    ] {
        let event = json!({"type":"response.output_item.added", "output_index":idx,
            "item":{"type":"function_call", "id":format!("fc_{idx}"), "call_id":id, "name":name}});
        for chunk in state
            .parse_event("p", serde_json::from_value(event).unwrap())
            .unwrap()
        {
            sdk.push(&chunk);
        }
    }
    for (idx, delta) in [
        (7, "{\"a\":"),
        (2, "{\"b\":"),
        (7, "1"),
        (2, "2}"),
        (7, "}"),
    ] {
        let event = json!({"type":"response.function_call_arguments.delta", "output_index":idx, "delta":delta});
        for chunk in state
            .parse_event("p", serde_json::from_value(event).unwrap())
            .unwrap()
        {
            assert!(
                chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0]
                    .get("id")
                    .is_none()
            );
            sdk.push(&chunk);
        }
    }
    sdk.assert_call(0, 0, "call_a", "alpha", "{\"a\":1}");
    sdk.assert_call(0, 1, "call_b", "beta", "{\"b\":2}");
    sdk.assert_call(0, 2, "call_c", "empty", "");
}

#[cfg(feature = "anthropic-api")]
fn anthropic_events() -> Vec<Value> {
    let mut events = Vec::new();
    for (idx, id, name, args) in [
        (3, "tool_a", "alpha", vec!["{\"a\":", "1", "}"]),
        (8, "tool_b", "beta", vec!["{", "\"b\":2}"]),
        (9, "tool_empty", "empty", vec![]),
    ] {
        events.push(json!({"type":"content_block_start", "index":idx,
            "content_block":{"type":"tool_use", "id":id, "name":name, "input":{}}}));
        for arg in args {
            events.push(json!({"type":"content_block_delta", "index":idx,
                "delta":{"type":"input_json_delta", "partial_json":arg}}));
        }
        events.push(json!({"type":"content_block_stop", "index":idx}));
    }
    events.push(json!({"type":"message_stop"}));
    events
}

#[cfg(feature = "anthropic-api")]
#[test]
fn anthropic_multiple_tools_and_empty_call_are_sdk_concatenable() {
    let mut state = crate::anthropic_api::sse::SseState::default();
    state
        .tool_reverse
        .insert("alpha".into(), "original_alpha".into());
    let mut sdk = SdkAccumulator::default();
    for event in anthropic_events() {
        if let Some(chunk) = state.parse_event("p", &event.to_string()).unwrap() {
            sdk.push(&chunk);
        }
    }
    sdk.assert_call(0, 0, "tool_a", "original_alpha", "{\"a\":1}");
    sdk.assert_call(0, 1, "tool_b", "beta", "{\"b\":2}");
    sdk.assert_call(0, 2, "tool_empty", "empty", "");
}

#[cfg(feature = "openai-compat")]
#[test]
fn compat_synthesized_ids_are_sdk_concatenable_across_choices_and_late_ids() {
    use crate::openai_compat::sse::StreamedToolCallIds;
    let mut state = StreamedToolCallIds::default();
    let mut sdk = SdkAccumulator::default();
    for (choice, index, tool) in [
        (0, 0, json!({"function":{"name":"alpha", "arguments":"{"}})),
        (1, 0, json!({"function":{"name":"beta", "arguments":""}})),
        (
            0,
            1,
            json!({"id":"real", "function":{"name":"gamma", "arguments":""}}),
        ),
        (
            0,
            0,
            json!({"id":"late", "function":{"arguments":"\"a\":1}"}}),
        ),
        (1, 0, json!({"function":{"arguments":"{}"}})),
        (0, 1, json!({"id":"real", "function":{"arguments":"{}"}})),
    ] {
        let mut tool = tool;
        tool["index"] = json!(index);
        let mut chunk: ChatChunk = serde_json::from_value(json!({"choices":[{"index":choice,
            "delta":{"tool_calls":[tool]}}]}))
        .unwrap();
        state.fill_missing_ids("p", &mut chunk).unwrap();
        sdk.push(&chunk);
    }
    sdk.assert_call(0, 0, "call_0", "alpha", "{\"a\":1}");
    sdk.assert_call(1, 0, "call_1_0", "beta", "{}");
    sdk.assert_call(0, 1, "real", "gamma", "{}");
}

#[cfg(feature = "openai-responses")]
#[test]
fn responses_repeated_tool_start_fails_before_splitting_the_call() {
    use crate::openai_responses::sse::ResponsesStreamState;
    let mut state = ResponsesStreamState::default();
    let start = json!({"type":"response.output_item.added", "output_index":0,
        "item":{"type":"function_call", "id":"fc_0", "call_id":"call_a", "name":"alpha"}});
    let mut sdk = SdkAccumulator::default();
    for event in [
        start.clone(),
        json!({"type":"response.function_call_arguments.delta", "output_index":0, "delta":"{"}),
    ] {
        for chunk in state
            .parse_event("p", serde_json::from_value(event).unwrap())
            .unwrap()
        {
            sdk.push(&chunk);
        }
    }
    assert!(
        state
            .parse_event("p", serde_json::from_value(start).unwrap())
            .is_err()
    );
    assert_eq!(sdk.0.len(), 1);
    sdk.assert_call(0, 0, "call_a", "alpha", "{");
}

#[cfg(feature = "anthropic-api")]
#[test]
fn anthropic_repeated_tool_start_and_post_stop_message_start_are_errors() {
    let mut state = crate::anthropic_api::sse::SseState::default();
    let start = json!({"type":"content_block_start", "index":0,
        "content_block":{"type":"tool_use", "id":"tool_a", "name":"alpha", "input":{}}});
    let mut sdk = SdkAccumulator::default();
    for event in [
        start.clone(),
        json!({"type":"content_block_delta", "index":0,
        "delta":{"type":"input_json_delta", "partial_json":"{"}}),
    ] {
        if let Some(chunk) = state.parse_event("p", &event.to_string()).unwrap() {
            sdk.push(&chunk);
        }
    }
    assert!(state.parse_event("p", &start.to_string()).is_err());
    assert_eq!(sdk.0.len(), 1);
    sdk.assert_call(0, 0, "tool_a", "alpha", "{");
    let mut state = crate::anthropic_api::sse::SseState::default();
    state
        .parse_event("p", r#"{"type":"message_stop"}"#)
        .unwrap();
    assert!(
        state
            .parse_event(
                "p",
                r#"{"type":"message_start","message":{"id":"later","model":"test"}}"#
            )
            .is_err()
    );
}

#[cfg(feature = "bedrock")]
#[path = "provider_audit_bedrock_tests.rs"]
mod bedrock;

#[path = "provider_audit_complete_tests.rs"]
mod complete;
