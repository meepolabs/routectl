//! Bedrock InvokeModel streams nest Anthropic SSE events inside
//! eventstream `chunk` frames and share the Anthropic parser, so the
//! opening role chunk must carry the same first-event input and cache
//! breakdown, labelled with the Bedrock origin, through the real framing.

use super::*;
use aws_smithy_types::event_stream::{Header, HeaderValue};
use futures::stream::StreamExt;
use routectl_core::schema::CacheCreation;

fn chunk_frame_bytes(inner_event: &str) -> Vec<u8> {
    let b64 = B64_STANDARD.encode(inner_event.as_bytes());
    let payload = format!(r#"{{"bytes":"{b64}"}}"#);
    let frame = Message::new(Bytes::from(payload.into_bytes()))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".to_string().into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String("chunk".to_string().into()),
        ));
    let mut buf = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf)
        .expect("encode eventstream frame");
    buf
}

async fn decode(events: &[&str]) -> Vec<ChatChunk> {
    let wire: Vec<u8> = events.iter().flat_map(|e| chunk_frame_bytes(e)).collect();
    let byte_stream = futures::stream::iter(vec![Ok(Bytes::from(wire))]);
    invoke_stream("test-bedrock".to_string(), byte_stream)
        .map(|item| item.expect("no stream error"))
        .collect()
        .await
}

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"msg_bdr","type":"message","role":"assistant","content":[],"model":"claude-opus-4-7","usage":{"input_tokens":321,"output_tokens":1,"cache_creation_input_tokens":654,"cache_read_input_tokens":987,"cache_creation":{"ephemeral_5m_input_tokens":600,"ephemeral_1h_input_tokens":54}}}}"#;
const MESSAGE_START_NO_USAGE: &str = r#"{"type":"message_start","message":{"id":"msg_bdr","type":"message","role":"assistant","content":[],"model":"claude-opus-4-7"}}"#;
const TEXT_START: &str =
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
const TEXT_DELTA: &str =
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;

#[tokio::test]
async fn invoke_stream_role_chunk_carries_the_opening_breakdown() {
    // Arrange + Act
    let chunks = decode(&[MESSAGE_START, TEXT_START, TEXT_DELTA]).await;

    // Assert
    let first = chunks.first().expect("the role chunk is yielded");
    assert!(matches!(
        first.choices[0].delta.role,
        Some(routectl_core::Role::Assistant)
    ));
    assert!(first.usage.is_none(), "no canonical usage on the opener");
    let opening = first
        .upstream_meta
        .as_ref()
        .and_then(|meta| meta.opening_usage.as_ref())
        .expect("the Bedrock opener carries the opening usage");
    assert_eq!(opening.origin, OpeningUsageOrigin::BedrockInvoke);
    assert_eq!(opening.input_tokens, 321);
    assert_eq!(opening.cache_creation_input_tokens, Some(654));
    assert_eq!(opening.cache_read_input_tokens, Some(987));
    assert_eq!(
        opening.cache_creation,
        Some(CacheCreation {
            ephemeral_5m_input_tokens: Some(600),
            ephemeral_1h_input_tokens: Some(54),
        })
    );
    for later in &chunks[1..] {
        assert!(later.upstream_meta.is_none());
    }
}

#[tokio::test]
async fn invoke_stream_without_opening_usage_carries_nothing() {
    // Arrange + Act
    let chunks = decode(&[MESSAGE_START_NO_USAGE, TEXT_START, TEXT_DELTA]).await;

    // Assert
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(|c| c.upstream_meta.is_none()));
    assert!(chunks.iter().all(|c| c.usage.is_none()));
}

#[tokio::test]
async fn an_invoke_output_only_delta_carries_the_vendor_opening_source() {
    // Arrange
    const OUTPUT_ONLY_DELTA: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#;

    // Act
    let chunks = decode(&[MESSAGE_START, TEXT_START, TEXT_DELTA, OUTPUT_ONLY_DELTA]).await;

    // Assert
    let terminal = chunks.last().expect("terminal chunk");
    assert_eq!(
        terminal.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(321 + 654 + 987)
    );
    assert_eq!(
        terminal
            .upstream_meta
            .as_ref()
            .and_then(|m| m.usage_input_source),
        Some(routectl_core::UsageInputSource::VendorOpening)
    );
    assert_eq!(
        terminal
            .upstream_meta
            .as_ref()
            .and_then(|m| m.usage_from_vendor_endpoint),
        Some(true)
    );
}

#[tokio::test]
async fn an_invoke_opening_is_from_the_vendor_endpoint() {
    let chunks = decode(&[MESSAGE_START, TEXT_START, TEXT_DELTA]).await;

    let opening = chunks[0]
        .upstream_meta
        .as_ref()
        .and_then(|m| m.opening_usage.as_ref())
        .expect("opening carried");
    assert!(opening.from_vendor_endpoint);
}
