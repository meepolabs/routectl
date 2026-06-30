//! Tests for the Gemini `streamGenerateContent` SSE state machine.

use super::*;
use crate::gemini::types::{
    Candidate, GenerateContentResponse, ResponseContent, ResponseFunctionCall, ResponsePart,
    UsageMetadata,
};
use routectl_core::Role;

const PID: &str = "gemini:test";

fn text_part(text: &str) -> ResponsePart {
    ResponsePart {
        text: Some(text.to_string()),
        ..Default::default()
    }
}

fn thought_part(text: &str, sig: Option<&str>) -> ResponsePart {
    ResponsePart {
        text: Some(text.to_string()),
        thought: Some(true),
        thought_signature: sig.map(str::to_string),
        ..Default::default()
    }
}

fn fc_part(name: &str, args: serde_json::Value) -> ResponsePart {
    ResponsePart {
        function_call: Some(ResponseFunctionCall {
            name: name.to_string(),
            args,
        }),
        ..Default::default()
    }
}

fn event(
    parts: Vec<ResponsePart>,
    finish: Option<&str>,
    usage: Option<UsageMetadata>,
) -> GenerateContentResponse {
    GenerateContentResponse {
        candidates: vec![Candidate {
            content: Some(ResponseContent {
                parts,
                role: Some("model".into()),
            }),
            finish_reason: finish.map(str::to_string),
            index: 0,
        }],
        usage_metadata: usage,
        model_version: Some("gemini-2.5-pro".into()),
        response_id: Some("resp-x".into()),
    }
}

#[test]
fn first_event_emits_role_then_text() {
    let mut state = GeminiStreamState::default();
    let chunks = state
        .parse_event(PID, event(vec![text_part("hi")], None, None))
        .expect("parse ok");

    assert_eq!(chunks.len(), 2, "expected a role chunk then a text chunk");
    assert!(matches!(
        chunks[0].choices[0].delta.role,
        Some(Role::Assistant)
    ));
    assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("hi"));
    // Id / model are threaded onto every chunk.
    assert_eq!(chunks[0].id, "resp-x");
    assert_eq!(chunks[0].model, "gemini-2.5-pro");
}

#[test]
fn role_emitted_only_once_across_events() {
    let mut state = GeminiStreamState::default();
    let first = state
        .parse_event(PID, event(vec![text_part("a")], None, None))
        .expect("parse first");
    let second = state
        .parse_event(PID, event(vec![text_part("b")], None, None))
        .expect("parse second");

    assert!(first
        .iter()
        .any(|c| matches!(c.choices[0].delta.role, Some(Role::Assistant))));
    // The second event must NOT re-emit the role.
    assert!(second.iter().all(|c| c.choices[0].delta.role.is_none()));
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].choices[0].delta.content.as_deref(), Some("b"));
}

#[test]
fn function_call_part_streams_tool_call_delta() {
    let mut state = GeminiStreamState::default();
    let chunks = state
        .parse_event(
            PID,
            event(
                vec![fc_part("get_weather", serde_json::json!({"city": "Paris"}))],
                None,
                None,
            ),
        )
        .expect("parse ok");

    let tc = chunks
        .iter()
        .find_map(|c| c.choices[0].delta.tool_calls.as_ref())
        .expect("a tool_calls delta");
    assert_eq!(tc[0]["index"], 0);
    assert_eq!(tc[0]["function"]["name"], "get_weather");
    let args: serde_json::Value =
        serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Paris");
}

#[test]
fn two_function_calls_get_dense_indices() {
    let mut state = GeminiStreamState::default();
    let chunks = state
        .parse_event(
            PID,
            event(
                vec![
                    fc_part("a", serde_json::json!({})),
                    fc_part("b", serde_json::json!({})),
                ],
                None,
                None,
            ),
        )
        .expect("parse ok");
    let indices: Vec<i64> = chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.tool_calls.as_ref())
        .map(|tc| tc[0]["index"].as_i64().unwrap())
        .collect();
    assert_eq!(indices, vec![0, 1], "tool-call indices must be dense");
}

#[test]
fn thought_part_streams_reasoning_with_signature() {
    let mut state = GeminiStreamState::default();
    let chunks = state
        .parse_event(
            PID,
            event(
                vec![thought_part("thinking...", Some("sig-abc"))],
                None,
                None,
            ),
        )
        .expect("parse ok");

    let rc = chunks
        .iter()
        .find(|c| c.choices[0].delta.reasoning.is_some())
        .expect("a reasoning chunk");
    assert_eq!(
        rc.choices[0].delta.reasoning.as_deref(),
        Some("thinking...")
    );
    let detail = &rc.choices[0].delta.reasoning_details[0];
    assert_eq!(detail.format.as_deref(), Some(GEMINI_FORMAT));
    assert_eq!(detail.payload["thought_signature"], "sig-abc");
    // A thought part must NOT leak into visible assistant content.
    assert!(rc.choices[0].delta.content.is_none());
}

#[test]
fn terminal_event_carries_finish_reason_and_usage() {
    let mut state = GeminiStreamState::default();
    let usage = UsageMetadata {
        prompt_token_count: 10,
        candidates_token_count: 7,
        total_token_count: 17,
        cached_content_token_count: 4,
        thoughts_token_count: 3,
        ..Default::default()
    };
    let chunks = state
        .parse_event(PID, event(vec![], Some("STOP"), Some(usage)))
        .expect("parse ok");

    let term = chunks
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("a terminal chunk");
    assert_eq!(term.choices[0].finish_reason.as_deref(), Some("stop"));
    let u = term.usage.as_ref().expect("usage on terminal chunk");
    assert_eq!(u.prompt_tokens, Some(10));
    assert_eq!(u.completion_tokens, Some(7));
    assert_eq!(u.total_tokens, Some(17));
    assert_eq!(u.cache_read_input_tokens, Some(4));
    assert_eq!(u.reasoning_tokens, Some(3));
}

#[test]
fn function_call_then_stop_maps_terminal_to_tool_calls() {
    let mut state = GeminiStreamState::default();
    let _ = state
        .parse_event(
            PID,
            event(vec![fc_part("f", serde_json::json!({}))], None, None),
        )
        .expect("parse first");
    // The terminal event reports STOP, but a functionCall was already seen,
    // so the sticky flag must map the terminal finish_reason to tool_calls.
    let chunks = state
        .parse_event(PID, event(vec![], Some("STOP"), None))
        .expect("parse terminal");
    let term = chunks
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("a terminal chunk");
    assert_eq!(term.choices[0].finish_reason.as_deref(), Some("tool_calls"));
}

#[test]
fn bad_sse_json_is_streaming_error() {
    let err = parse_data_line(PID, "{not valid json").expect_err("must error");
    assert!(
        matches!(err, routectl_core::Error::Streaming(_)),
        "a malformed SSE payload must surface as Error::Streaming, got: {err:?}"
    );
}

#[test]
fn trailing_usage_only_event_does_not_emit_second_terminal() {
    let mut state = GeminiStreamState::default();
    // Event 1: content + finishReason + usage -- the real terminal.
    let first = state
        .parse_event(
            PID,
            event(
                vec![text_part("hi")],
                Some("STOP"),
                Some(UsageMetadata {
                    prompt_token_count: 1,
                    candidates_token_count: 1,
                    total_token_count: 2,
                    ..Default::default()
                }),
            ),
        )
        .expect("parse first");
    assert_eq!(
        first
            .iter()
            .filter(|c| c.choices[0].finish_reason.is_some())
            .count(),
        1,
        "exactly one terminal chunk on the finish event"
    );
    // Event 2: a trailing usage-only event (no finishReason). The
    // terminal_emitted guard must suppress a second terminal -- and any
    // content -- so the stream has a single terminal frame.
    let second = state
        .parse_event(
            PID,
            event(
                vec![],
                None,
                Some(UsageMetadata {
                    total_token_count: 2,
                    ..Default::default()
                }),
            ),
        )
        .expect("parse trailing");
    assert!(
        second.is_empty(),
        "a post-terminal event must emit nothing; got {second:?}"
    );
}

#[test]
fn empty_candidates_event_without_usage_emits_only_role() {
    // A prompt-level safety block can arrive as an event with no
    // candidates and no usageMetadata. Today that yields just the role
    // chunk and no terminal -- pin the behavior so a future change to
    // surface a synthetic content_filter terminal is a conscious one.
    let mut state = GeminiStreamState::default();
    let ev = GenerateContentResponse {
        candidates: vec![],
        usage_metadata: None,
        model_version: Some("gemini-2.5-pro".into()),
        response_id: Some("resp-x".into()),
    };
    let chunks = state.parse_event(PID, ev).expect("parse ok");
    assert_eq!(chunks.len(), 1, "only the opening role chunk");
    assert!(chunks[0].choices[0].finish_reason.is_none());
}
