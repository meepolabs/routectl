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
fn function_call_signature_threads_into_tool_delta() {
    // A native Gemini-3 streamed functionCall carries its thoughtSignature on
    // the part; it must ride the tool-call delta so replay round-trips native.
    let mut state = GeminiStreamState::default();
    let mut part = fc_part("get_weather", serde_json::json!({"city": "Paris"}));
    part.thought_signature = Some("stream-sig-1".to_string());
    let chunks = state
        .parse_event(PID, event(vec![part], None, None))
        .expect("parse ok");

    let tc = chunks
        .iter()
        .find_map(|c| c.choices[0].delta.tool_calls.as_ref())
        .expect("a tool_calls delta");
    assert_eq!(tc[0]["thought_signature"], "stream-sig-1");
}

#[test]
fn function_call_without_signature_omits_field_in_tool_delta() {
    // A chunk with no signature must not add the key -- preserving the
    // passthrough shape and letting replay treat it as foreign.
    let mut state = GeminiStreamState::default();
    let chunks = state
        .parse_event(
            PID,
            event(vec![fc_part("f", serde_json::json!({}))], None, None),
        )
        .expect("parse ok");

    let tc = chunks
        .iter()
        .find_map(|c| c.choices[0].delta.tool_calls.as_ref())
        .expect("a tool_calls delta");
    assert!(tc[0].get("thought_signature").is_none());
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

fn tool_call_ids(chunks: &[ChatChunk]) -> Vec<(i64, String)> {
    chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.tool_calls.as_ref())
        .flatten()
        .map(|tc| {
            (
                tc["index"].as_i64().expect("tool call index"),
                tc["id"].as_str().expect("tool call id").to_string(),
            )
        })
        .collect()
}

fn decode_tool_turn(names: &[&str]) -> Vec<(i64, String)> {
    let mut state = GeminiStreamState::default();
    let mut chunks = Vec::new();
    for name in names {
        chunks.extend(
            state
                .parse_event(
                    PID,
                    event(vec![fc_part(name, serde_json::json!({}))], None, None),
                )
                .expect("parse tool event"),
        );
    }
    chunks.extend(
        state
            .parse_event(PID, event(vec![], Some("STOP"), None))
            .expect("parse terminal"),
    );
    tool_call_ids(&chunks)
}

#[test]
fn tool_call_ids_differ_between_two_decoded_streams() {
    // Arrange: two responses that each open with a call at index 0, as two
    // consecutive turns of one conversation would.
    let first_turn = decode_tool_turn(&["lookup"]);
    let second_turn = decode_tool_turn(&["lookup"]);

    // Assert
    assert_eq!(first_turn[0].0, 0);
    assert_eq!(second_turn[0].0, 0);
    assert_ne!(
        first_turn[0].1, second_turn[0].1,
        "two responses must not both name their first call the same id"
    );
}

#[test]
fn tool_call_ids_are_distinct_and_wire_safe_within_one_stream() {
    // Act
    let ids = decode_tool_turn(&["a", "b", "c"]);

    // Assert: one id per call index, no two calls share one, and every id
    // is inside the charset and length every egress sends verbatim.
    let indices: Vec<i64> = ids.iter().map(|(i, _)| *i).collect();
    assert_eq!(indices, vec![0, 1, 2]);
    let distinct: std::collections::HashSet<&str> = ids.iter().map(|(_, id)| id.as_str()).collect();
    assert_eq!(
        distinct.len(),
        ids.len(),
        "ids repeat within one stream: {ids:?}"
    );
    for (_, id) in &ids {
        assert!(
            id.len() <= 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "id {id:?} would be rewritten on replay"
        );
    }
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

fn in_band_upstream_error(data: &str) -> (u16, String, Option<Box<str>>, Option<Box<str>>) {
    match parse_data_line(PID, data)
        .expect_err("an in-band error object must not parse as a success")
    {
        routectl_core::Error::Upstream {
            status,
            body,
            upstream_type,
            upstream_code,
            ..
        } => (status, body, upstream_type, upstream_code),
        other => panic!("expected Error::Upstream, got: {other:?}"),
    }
}

#[test]
fn in_band_error_object_surfaces_as_upstream_error_with_its_status() {
    let data = r#"{"error":{"code":429,"message":"Resource has been exhausted","status":"RESOURCE_EXHAUSTED"}}"#;

    let (status, body, upstream_type, upstream_code) = in_band_upstream_error(data);

    assert_eq!(status, 429);
    assert!(
        body.contains("Resource has been exhausted"),
        "the upstream message must reach the error body, got: {body}"
    );
    assert_eq!(upstream_type.as_deref(), Some("RESOURCE_EXHAUSTED"));
    assert_eq!(upstream_code.as_deref(), Some("429"));
}

#[test]
fn in_band_error_object_with_non_error_status_code_clamps_to_502() {
    for code in ["200", "302", "600", "70000", "\"429\""] {
        let data = format!(r#"{{"error":{{"code":{code},"message":"m"}}}}"#);

        let (status, _, _, _) = in_band_upstream_error(&data);

        assert_eq!(status, 502, "code {code} must clamp to 502");
    }
}

#[test]
fn in_band_error_object_without_code_or_message_defaults_to_502() {
    let (status, body, _, _) = in_band_upstream_error(r#"{"error":{"status":"INTERNAL"}}"#);

    assert_eq!(status, 502);
    assert!(!body.is_empty(), "a fallback message must be supplied");
}

#[test]
fn null_or_empty_error_field_on_a_content_event_parses_as_content() {
    for error in ["null", "{}"] {
        let data = format!(
            r#"{{"candidates":[{{"content":{{"parts":[{{"text":"hi"}}],"role":"model"}},"index":0}}],"error":{error}}}"#
        );

        let parsed = parse_data_line(PID, &data)
            .unwrap_or_else(|e| panic!("error:{error} must not abort the stream: {e:?}"));

        let text = parsed.candidates[0]
            .content
            .as_ref()
            .and_then(|c| c.parts[0].text.as_deref());
        assert_eq!(text, Some("hi"), "error:{error} content must survive");
    }
}

#[test]
fn ordinary_content_event_parses_as_content() {
    let data = r#"{"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"},"finishReason":"STOP","index":0}],"responseId":"r"}"#;

    let parsed = parse_data_line(PID, data).expect("a healthy event must parse");

    assert_eq!(parsed.response_id.as_deref(), Some("r"));
    assert_eq!(parsed.candidates[0].finish_reason.as_deref(), Some("STOP"));
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
fn eos_warn_says_whether_usage_had_arrived() {
    let rows = [
        (
            "content only",
            event(vec![text_part("hi")], None, None),
            "false",
        ),
        (
            "content with usage",
            event(vec![text_part("hi")], None, Some(usage_meta(5))),
            "true",
        ),
    ];
    for (name, ev, had_usage) in rows {
        // Act
        let events = capture_events(|| {
            let mut state = GeminiStreamState::default();
            let _ = state.parse_event(PID, ev).expect("parse ok");
            state.on_eos(PID);
        });

        // Assert
        let warns: Vec<&CapturedEvent> = events.iter().filter(|e| e.level == Level::WARN).collect();
        assert_eq!(warns.len(), 1, "{name}: exactly one WARN; got {warns:?}");
        assert_eq!(warns[0].field("provider"), Some(PID), "{name}");
        assert_eq!(
            warns[0].field("had_cached_usage"),
            Some(had_usage),
            "{name}"
        );
    }
}

#[test]
fn terminal_event_predicate_matches_the_events_that_end_the_stream() {
    type Build = fn() -> GenerateContentResponse;
    let rows: [(&str, Build, bool); 8] = [
        ("STOP finish", || event(vec![], Some("STOP"), None), true),
        (
            "MAX_TOKENS finish with content",
            || event(vec![text_part("x")], Some("MAX_TOKENS"), None),
            true,
        ),
        ("prompt block", || prompt_block_event("SAFETY"), true),
        (
            "content, no finish",
            || event(vec![text_part("x")], None, None),
            false,
        ),
        (
            "unspecified finish",
            || event(vec![], Some("FINISH_REASON_UNSPECIFIED"), None),
            false,
        ),
        (
            "unspecified prompt block",
            || prompt_block_event("BLOCK_REASON_UNSPECIFIED"),
            false,
        ),
        (
            "block reason beside a candidate",
            || {
                let mut ev = event(vec![text_part("x")], None, None);
                ev.prompt_feedback = Some(PromptFeedback {
                    block_reason: Some("SAFETY".into()),
                });
                ev
            },
            false,
        ),
        (
            "usage only",
            || GenerateContentResponse {
                candidates: vec![],
                usage_metadata: Some(usage_meta(5)),
                model_version: None,
                response_id: None,
                prompt_feedback: None,
            },
            false,
        ),
    ];
    for (name, build, terminal) in rows {
        // Arrange: the decoder's own verdict on the same event.
        let emits_terminal = GeminiStreamState::default()
            .parse_event(PID, build())
            .expect("parse ok")
            .iter()
            .any(|c| c.choices[0].finish_reason.is_some());

        // Act
        let predicate = is_terminal_event(&build());

        // Assert
        assert_eq!(predicate, terminal, "{name}");
        assert_eq!(emits_terminal, terminal, "{name}: decoder disagrees");
    }
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
fn prompt_block_event_emits_single_content_filter_terminal() {
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
    });
    let blocks: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains("blocked on 200 surface"))
        .collect();
    assert_eq!(blocks.len(), 1, "exactly one block line; got {blocks:?}");
    assert_eq!(blocks[0].level, Level::DEBUG);
    assert_eq!(blocks[0].field("provider"), Some(PID));
    assert_eq!(blocks[0].field("surface"), Some("stream"));
    assert_eq!(blocks[0].field("origin"), Some("prompt_feedback"));
    assert_eq!(blocks[0].field("block_reason"), Some("SAFETY"));
}

fn usage_meta(prompt: u32) -> UsageMetadata {
    UsageMetadata {
        prompt_token_count: prompt,
        candidates_token_count: 2,
        total_token_count: prompt + 2,
        ..Default::default()
    }
}

fn usage_source(chunk: &ChatChunk) -> Option<routectl_core::UsageInputSource> {
    chunk
        .upstream_meta
        .as_ref()
        .and_then(|m| m.usage_input_source)
}

#[test]
fn a_finish_event_with_its_own_usage_is_an_explicit_final_report() {
    let mut state = GeminiStreamState::default();

    let chunks = state
        .parse_event(
            PID,
            event(vec![text_part("hi")], Some("STOP"), Some(usage_meta(50))),
        )
        .unwrap();

    let terminal = chunks.last().expect("terminal");
    assert_eq!(
        terminal.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(50)
    );
    assert_eq!(
        usage_source(terminal),
        Some(routectl_core::UsageInputSource::ExplicitFinal)
    );
}

#[test]
fn usage_carried_from_an_interim_event_onto_the_finish_is_marked_interim() {
    let mut state = GeminiStreamState::default();
    state
        .parse_event(PID, event(vec![text_part("a")], None, Some(usage_meta(50))))
        .unwrap();

    let chunks = state
        .parse_event(PID, event(vec![text_part("b")], Some("STOP"), None))
        .unwrap();

    let terminal = chunks.last().expect("terminal");
    assert_eq!(
        terminal.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(50),
        "positive control: the interim count was carried"
    );
    assert_eq!(
        usage_source(terminal),
        Some(routectl_core::UsageInputSource::InterimCarry)
    );
}

#[test]
fn a_stream_prompt_block_reason_is_scrubbed_in_its_debug_line() {
    use crate::upstream_log::tests::{assert_tag_scrubbed, planted_tags};
    for (row, tag, expected, planted) in planted_tags() {
        let events = capture_events(|| {
            let mut state = GeminiStreamState::default();
            let ev = GenerateContentResponse {
                candidates: vec![],
                usage_metadata: None,
                model_version: None,
                response_id: None,
                prompt_feedback: Some(PromptFeedback {
                    block_reason: Some(tag.clone()),
                }),
            };
            let _ = state.parse_event(PID, ev);
        });

        assert_tag_scrubbed(
            row,
            &events,
            (Level::DEBUG, "prompt blocked on 200 surface"),
            "block_reason",
            &expected,
            planted,
        );
    }
}

fn prompt_block_event(reason: &str) -> GenerateContentResponse {
    GenerateContentResponse {
        candidates: vec![],
        usage_metadata: None,
        model_version: Some("gemini-2.5-pro".into()),
        response_id: Some("resp-x".into()),
        prompt_feedback: Some(PromptFeedback {
            block_reason: Some(reason.into()),
        }),
    }
}

fn delivered_text_and_finishes(chunks: &[ChatChunk]) -> (String, Vec<String>) {
    let text = chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.content.clone())
        .collect();
    let finishes = chunks
        .iter()
        .filter_map(|c| c.choices[0].finish_reason.clone())
        .collect();
    (text, finishes)
}

#[test]
fn unspecified_finish_or_block_reason_does_not_end_the_stream() {
    let rows: [(&str, GenerateContentResponse); 4] = [
        (
            "unspecified finishReason",
            event(
                vec![text_part("a")],
                Some("FINISH_REASON_UNSPECIFIED"),
                None,
            ),
        ),
        (
            "empty finishReason",
            event(vec![text_part("a")], Some(""), None),
        ),
        (
            "unspecified prompt blockReason",
            prompt_block_event("BLOCK_REASON_UNSPECIFIED"),
        ),
        ("empty prompt blockReason", prompt_block_event("")),
    ];
    for (name, sentinel) in rows {
        // Arrange
        let mut state = GeminiStreamState::default();
        let mut chunks = Vec::new();

        // Act: the sentinel, then more content, then a real finish.
        for ev in [
            sentinel,
            event(vec![text_part("b")], None, None),
            event(vec![text_part("c")], Some("MAX_TOKENS"), None),
        ] {
            chunks.extend(state.parse_event(PID, ev).expect("parse ok"));
        }

        // Assert
        let (text, finishes) = delivered_text_and_finishes(&chunks);
        assert!(text.ends_with("bc"), "{name}: later content lost: {text:?}");
        assert_eq!(finishes, vec!["length"], "{name}: wrong terminal");
    }
}

#[test]
fn a_real_prompt_block_still_ends_the_stream() {
    // Arrange
    let mut state = GeminiStreamState::default();
    let mut chunks = Vec::new();

    // Act
    for ev in [
        prompt_block_event("SAFETY"),
        event(vec![text_part("late")], Some("STOP"), None),
    ] {
        chunks.extend(state.parse_event(PID, ev).expect("parse ok"));
    }

    // Assert
    let (text, finishes) = delivered_text_and_finishes(&chunks);
    assert_eq!(text, "", "content after a prompt block must be dropped");
    assert_eq!(finishes, vec!["content_filter"]);
}
