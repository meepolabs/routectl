//! Cross-lane guard for the opening-role-chunk contract.
//!
//! An OpenAI-Chat stream opens with a single `delta.role="assistant"`
//! chunk before any content, and every egress lane must emit it exactly
//! once in first position. This test drives all four lanes (gemini,
//! responses, anthropic, bedrock) through their opening events and pins
//! that invariant for every lane at once -- it is the template a new
//! lane extends.

use aws_smithy_types::event_stream::{Header, HeaderValue, Message as AwsMessage};
use bytes::Bytes;

use routectl_core::{ChatChunk, Role};

/// Assert a lane's opening chunk sequence carries exactly one
/// `delta.role="assistant"` chunk, in first position, with no content.
fn assert_opens_with_single_role_chunk(lane: &str, chunks: &[ChatChunk]) {
    assert!(!chunks.is_empty(), "{lane}: expected an opening chunk");
    let first = &chunks[0];
    assert!(
        matches!(first.choices[0].delta.role, Some(Role::Assistant)),
        "{lane}: the first chunk must carry role=assistant"
    );
    assert!(
        first.choices[0].delta.content.is_none(),
        "{lane}: the opening role chunk must carry no content"
    );
    let role_chunks = chunks
        .iter()
        .filter(|c| c.choices.iter().any(|ch| ch.delta.role.is_some()))
        .count();
    assert_eq!(
        role_chunks, 1,
        "{lane}: the role chunk must appear exactly once"
    );
}

fn gemini_opening() -> Vec<ChatChunk> {
    use crate::gemini::sse::{GeminiStreamState, parse_data_line};
    let mut state = GeminiStreamState::default();
    let first = parse_data_line(
        "gemini",
        r#"{"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"},"index":0}],
            "responseId":"r","modelVersion":"m"}"#,
    )
    .expect("gemini event parse");
    let second = parse_data_line(
        "gemini",
        r#"{"candidates":[{"content":{"parts":[{"text":"!"}],"role":"model"},"index":0}],
            "responseId":"r","modelVersion":"m"}"#,
    )
    .expect("gemini event parse");
    let mut out = state.parse_event("gemini", first).expect("gemini parse");
    out.extend(state.parse_event("gemini", second).expect("gemini parse"));
    out
}

fn responses_opening() -> Vec<ChatChunk> {
    use crate::openai_responses::response_types::ResponsesStreamEvent;
    use crate::openai_responses::sse::ResponsesStreamState;
    let mut state = ResponsesStreamState::default();
    let created: ResponsesStreamEvent = serde_json::from_value(serde_json::json!({
        "type": "response.created",
        "response": {"id": "resp_1", "model": "m"}
    }))
    .expect("responses event parse");
    let item_added: ResponsesStreamEvent = serde_json::from_value(serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}
    }))
    .expect("responses event parse");
    let text_delta: ResponsesStreamEvent = serde_json::from_value(serde_json::json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": "hi"
    }))
    .expect("responses event parse");
    let mut out = state
        .parse_event("responses", created)
        .expect("responses parse");
    out.extend(
        state
            .parse_event("responses", item_added)
            .expect("responses parse"),
    );
    out.extend(
        state
            .parse_event("responses", text_delta)
            .expect("responses parse"),
    );
    out
}

fn anthropic_opening() -> Vec<ChatChunk> {
    use crate::anthropic_api::sse::SseState;
    let mut state = SseState::default();
    let message_start = r#"{
        "type":"message_start",
        "message": {"id":"msg_1","type":"message","role":"assistant","content":[],
            "model":"m","stop_reason":null,"stop_sequence":null,
            "usage":{"input_tokens":1,"output_tokens":0}}
    }"#;
    let block_start = r#"{"type":"content_block_start","index":0,
        "content_block":{"type":"text","text":""}}"#;
    let text_delta = r#"{"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":"hi"}}"#;
    let mut out = Vec::new();
    for event in [message_start, block_start, text_delta] {
        if let Some(chunk) = state
            .parse_event("anthropic", event)
            .expect("anthropic parse")
        {
            out.push(chunk);
        }
    }
    out
}

fn bedrock_opening() -> Vec<ChatChunk> {
    use crate::bedrock::converse::eventstream::{ConverseStreamState, handle_converse_frame};
    let mut state = ConverseStreamState::default();
    let mut out = Vec::new();
    for (event_type, payload) in [
        ("messageStart", r#"{"role":"assistant"}"#),
        ("contentBlockStart", r#"{"contentBlockIndex":0}"#),
        (
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#,
        ),
    ] {
        let frame =
            AwsMessage::new(Bytes::from(payload.as_bytes().to_vec())).add_header(Header::new(
                ":event-type",
                HeaderValue::String(event_type.to_string().into()),
            ));
        out.extend(handle_converse_frame("bedrock", frame, &mut state).expect("bedrock parse"));
    }
    out
}

/// All four egress lanes open their stream with a single
/// `delta.role="assistant"` chunk in first position.
#[test]
fn all_lanes_open_with_single_role_chunk() {
    assert_opens_with_single_role_chunk("gemini", &gemini_opening());
    assert_opens_with_single_role_chunk("responses", &responses_opening());
    assert_opens_with_single_role_chunk("anthropic", &anthropic_opening());
    assert_opens_with_single_role_chunk("bedrock", &bedrock_opening());
}
