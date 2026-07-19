//! Tests for the Gemini `streamGenerateContent` SSE state machine.

use super::*;
use crate::gemini::types::{
    Candidate, GenerateContentResponse, PromptFeedback, ResponseContent, ResponseFunctionCall,
    ResponsePart, UsageMetadata,
};
use routectl_core::Role;
use routectl_testkit::{CapturedEvent, capture_events};
use tracing::Level;

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
        prompt_feedback: None,
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

    assert!(
        first
            .iter()
            .any(|c| matches!(c.choices[0].delta.role, Some(Role::Assistant)))
    );
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
fn interim_usage_only_does_not_terminate_stream() {
    // usageMetadata placement is model/version dependent and can arrive on
    // an interim chunk. An interim usage-only event (no finishReason) must
    // NOT terminate the stream: later content and the real finishReason
    // that follow it must survive.
    let mut state = GeminiStreamState::default();
    let _ = state
        .parse_event(PID, event(vec![text_part("a")], None, None))
        .expect("parse first");
    // Interim usage-only event: no finishReason, no content.
    let interim = state
        .parse_event(
            PID,
            event(
                vec![],
                None,
                Some(UsageMetadata {
                    prompt_token_count: 5,
                    candidates_token_count: 2,
                    total_token_count: 7,
                    ..Default::default()
                }),
            ),
        )
        .expect("parse interim");
    assert!(
        interim.iter().all(|c| c.choices[0].finish_reason.is_none()),
        "an interim usage-only event must not emit a terminal chunk; got {interim:?}"
    );
    // Content that follows the interim usage event must still stream.
    let more = state
        .parse_event(PID, event(vec![text_part("b")], None, None))
        .expect("parse more");
    assert_eq!(
        more.iter()
            .find_map(|c| c.choices[0].delta.content.as_deref()),
        Some("b"),
        "content after an interim usage event must survive"
    );
    // The real finishReason arrives on a later event.
    let terminal = state
        .parse_event(PID, event(vec![], Some("STOP"), None))
        .expect("parse terminal");
    assert_eq!(
        terminal
            .iter()
            .filter(|c| c.choices[0].finish_reason.is_some())
            .count(),
        1,
        "the real finishReason must produce exactly one terminal chunk"
    );
    assert_eq!(
        terminal
            .iter()
            .find_map(|c| c.choices[0].finish_reason.as_deref()),
        Some("stop"),
    );
}

#[test]
fn split_usage_and_finish_reason_carries_forward_count() {
    // Split shape: usage on an interim event, finishReason on a later event
    // with no usage of its own. The terminal chunk must carry the
    // carried-forward usage count -- otherwise the split loses it.
    let mut state = GeminiStreamState::default();
    let _ = state
        .parse_event(
            PID,
            event(
                vec![text_part("hi")],
                None,
                Some(UsageMetadata {
                    prompt_token_count: 11,
                    candidates_token_count: 9,
                    total_token_count: 20,
                    cached_content_token_count: 3,
                    thoughts_token_count: 2,
                    ..Default::default()
                }),
            ),
        )
        .expect("parse interim usage");
    let terminal = state
        .parse_event(PID, event(vec![], Some("STOP"), None))
        .expect("parse terminal");
    let term = terminal
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("a terminal chunk");
    let u = term
        .usage
        .as_ref()
        .expect("terminal chunk must carry the carried-forward usage");
    assert_eq!(u.prompt_tokens, Some(11));
    assert_eq!(u.completion_tokens, Some(9));
    assert_eq!(u.total_tokens, Some(20));
    assert_eq!(u.cache_read_input_tokens, Some(3));
    assert_eq!(u.reasoning_tokens, Some(2));
}

#[test]
fn terminal_event_own_usage_wins_no_double_count() {
    // When the terminal event carries its OWN usage, that value wins; the
    // interim cached value must NOT be added on top (no double-count).
    let mut state = GeminiStreamState::default();
    let _ = state
        .parse_event(
            PID,
            event(
                vec![text_part("hi")],
                None,
                Some(UsageMetadata {
                    prompt_token_count: 100,
                    candidates_token_count: 100,
                    total_token_count: 200,
                    ..Default::default()
                }),
            ),
        )
        .expect("parse interim usage");
    let terminal = state
        .parse_event(
            PID,
            event(
                vec![],
                Some("STOP"),
                Some(UsageMetadata {
                    prompt_token_count: 10,
                    candidates_token_count: 7,
                    total_token_count: 17,
                    ..Default::default()
                }),
            ),
        )
        .expect("parse terminal");
    let term = terminal
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("a terminal chunk");
    let u = term.usage.as_ref().expect("usage on terminal chunk");
    assert_eq!(u.prompt_tokens, Some(10), "terminal-event usage must win");
    assert_eq!(
        u.completion_tokens,
        Some(7),
        "terminal-event usage must win"
    );
    assert_eq!(
        u.total_tokens,
        Some(17),
        "terminal event usage wins; interim must not be summed on top"
    );
}

#[test]
fn eos_with_cached_usage_and_no_finish_reason_warns() {
    // If the stream reaches EOS having observed usage but no finishReason
    // ever, termination was never proven -- a WARN must fire so silent
    // truncation cannot masquerade as success.
    let events = capture_events(|| {
        let mut state = GeminiStreamState::default();
        let _ = state
            .parse_event(
                PID,
                event(
                    vec![text_part("hi")],
                    None,
                    Some(UsageMetadata {
                        prompt_token_count: 5,
                        candidates_token_count: 2,
                        total_token_count: 7,
                        ..Default::default()
                    }),
                ),
            )
            .expect("parse usage event");
        state.on_eos(PID);
    });
    let warns: Vec<&CapturedEvent> = events.iter().filter(|e| e.level == Level::WARN).collect();
    assert_eq!(warns.len(), 1, "exactly one WARN must fire; got {warns:?}");
    assert_eq!(warns[0].field("provider"), Some(PID));
    assert_eq!(warns[0].field("had_cached_usage"), Some("true"));
}

#[test]
fn eos_after_finish_reason_does_not_warn() {
    // A clean stream that saw a finishReason must not warn at EOS, even
    // though usage was observed.
    let events = capture_events(|| {
        let mut state = GeminiStreamState::default();
        let _ = state
            .parse_event(
                PID,
                event(
                    vec![text_part("hi")],
                    Some("STOP"),
                    Some(UsageMetadata {
                        total_token_count: 7,
                        ..Default::default()
                    }),
                ),
            )
            .expect("parse terminal");
        state.on_eos(PID);
    });
    assert!(
        events.iter().all(|e| e.level != Level::WARN),
        "a proven-terminal stream must not warn; got {events:?}"
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
        prompt_feedback: None,
    };
    let chunks = state.parse_event(PID, ev).expect("parse ok");
    assert_eq!(chunks.len(), 1, "only the opening role chunk");
    assert!(chunks[0].choices[0].finish_reason.is_none());
}

#[test]
fn candidate_content_filter_finish_reason_maps_terminal() {
    let mut state = GeminiStreamState::default();
    let chunks = state
        .parse_event(PID, event(vec![], Some("PROHIBITED_CONTENT"), None))
        .expect("parse ok");
    let terminal = chunks
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("a terminal chunk");
    assert_eq!(
        terminal.choices[0].finish_reason.as_deref(),
        Some("content_filter")
    );
}

#[test]
fn prompt_block_event_emits_single_content_filter_terminal_and_no_eos_warn() {
    let events = capture_events(|| {
        let mut state = GeminiStreamState::default();
        let ev = GenerateContentResponse {
            candidates: vec![],
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 3,
                total_token_count: 3,
                ..Default::default()
            }),
            model_version: Some("gemini-2.5-pro".into()),
            response_id: Some("resp-x".into()),
            prompt_feedback: Some(PromptFeedback {
                block_reason: Some("SAFETY".into()),
            }),
        };
        let chunks = state.parse_event(PID, ev).expect("parse ok");

        // role chunk + one terminal content_filter chunk.
        let terminals: Vec<_> = chunks
            .iter()
            .filter(|c| c.choices[0].finish_reason.is_some())
            .collect();
        assert_eq!(terminals.len(), 1, "exactly one terminal chunk");
        assert_eq!(
            terminals[0].choices[0].finish_reason.as_deref(),
            Some("content_filter")
        );
        // A prompt block is proven terminality -- on_eos must stay silent.
        assert!(
            state.saw_finish_reason,
            "prompt block sets saw_finish_reason"
        );
        state.on_eos(PID);
    });
    assert!(
        events.iter().all(|e| e.level != Level::WARN),
        "a prompt-blocked stream must not warn at EOS; got {events:?}"
    );
    let infos: Vec<_> = events.iter().filter(|e| e.level == Level::INFO).collect();
    assert_eq!(infos.len(), 1, "exactly one INFO; got {infos:?}");
    assert_eq!(infos[0].field("provider"), Some(PID));
    assert_eq!(infos[0].field("surface"), Some("stream"));
    assert_eq!(infos[0].field("origin"), Some("prompt_feedback"));
    assert_eq!(infos[0].field("block_reason"), Some("SAFETY"));
}
