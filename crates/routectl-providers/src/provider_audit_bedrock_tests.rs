use super::*;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures::{StreamExt, stream::BoxStream};
use routectl_core::{Error, Result};

fn frame(kind: &str, value: Value) -> Bytes {
    let message = Message::new(Bytes::from(value.to_string()))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String(kind.to_string().into()),
        ));
    let mut wire = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&message, &mut wire).unwrap();
    Bytes::from(wire)
}

fn invoke(event: Value) -> Bytes {
    frame("chunk", json!({"bytes":STANDARD.encode(event.to_string())}))
}

fn stream(
    converse: bool,
    frames: Vec<std::result::Result<Bytes, reqwest::Error>>,
) -> BoxStream<'static, Result<ChatChunk>> {
    let bytes = futures::stream::iter(frames);
    if converse {
        crate::bedrock::eventstream::converse_stream("bedrock:audit".into(), bytes)
    } else {
        crate::bedrock::eventstream::invoke_stream("bedrock:audit".into(), bytes)
    }
}

#[tokio::test]
async fn bedrock_repeated_tool_start_and_post_stop_message_start_error_once() {
    for converse in [false, true] {
        let mut repeated = unfinished_frames(converse, true);
        repeated.push(repeated[0].clone());
        // The suffix must not resume as a second, apparently valid tool call.
        repeated.push(repeated[1].clone());
        let chunks: Vec<_> = stream(converse, repeated.into_iter().map(Ok).collect())
            .collect()
            .await;
        assert_eq!(chunks.iter().filter(|c| c.is_err()).count(), 1);
        let mut sdk = SdkAccumulator::default();
        for chunk in chunks.iter().filter_map(|c| c.as_ref().ok()) {
            sdk.push(chunk);
        }
        assert_eq!(sdk.0.len(), 1);
        sdk.assert_call(0, 0, "tool_a", "alpha", "{\"x\":");
        let frames = if converse {
            vec![
                frame("messageStop", json!({"stopReason":"end_turn"})),
                frame("messageStart", json!({"role":"assistant"})),
            ]
        } else {
            vec![
                invoke(json!({"type":"message_stop"})),
                invoke(json!({"type":"message_start","message":{"id":"later","model":"test"}})),
            ]
        };
        let chunks: Vec<_> = stream(converse, frames.into_iter().map(Ok).collect())
            .collect()
            .await;
        assert_eq!(chunks.iter().filter(|c| c.is_err()).count(), 1);
        assert!(matches!(chunks.last(), Some(Err(Error::Streaming(_)))));
    }
}

#[tokio::test]
async fn invoke_multiple_tools_are_sdk_concatenable_through_real_framing() {
    let wire = anthropic_events()
        .into_iter()
        .map(|e| Ok(invoke(e)))
        .collect();
    let chunks: Vec<_> = stream(false, wire).collect().await;
    let mut sdk = SdkAccumulator::default();
    for chunk in chunks {
        sdk.push(&chunk.expect("semantic message_stop received"));
    }
    sdk.assert_call(0, 0, "tool_a", "alpha", "{\"a\":1}");
    sdk.assert_call(0, 1, "tool_b", "beta", "{\"b\":2}");
    sdk.assert_call(0, 2, "tool_empty", "empty", "");
}

fn converse_tool_frames() -> Vec<Bytes> {
    let mut frames = Vec::new();
    for (idx, id, name) in [
        (7, "tool_a", "alpha"),
        (2, "tool_b", "beta"),
        (9, "tool_empty", "empty"),
    ] {
        frames.push(frame(
            "contentBlockStart",
            json!({"contentBlockIndex":idx,
            "start":{"toolUse":{"toolUseId":id,"name":name}}}),
        ));
    }
    for (idx, args) in [
        (2, "{"),
        (7, "{\"a\":"),
        (2, "\"b\":2}"),
        (7, "1"),
        (7, "}"),
    ] {
        frames.push(frame(
            "contentBlockDelta",
            json!({"contentBlockIndex":idx,"delta":{"toolUse":{"input":args}}}),
        ));
    }
    frames.push(frame("messageStop", json!({"stopReason":"tool_use"})));
    frames.push(frame(
        "metadata",
        json!({"usage":{"inputTokens":1,"outputTokens":2,"totalTokens":3}}),
    ));
    frames
}

#[tokio::test]
async fn converse_interleaved_tools_are_sdk_concatenable_through_split_frames() {
    let mut sdk = SdkAccumulator::default();
    let wire = converse_tool_frames()
        .into_iter()
        .flat_map(|b| vec![Ok(b.slice(..12)), Ok(b.slice(12..))])
        .collect();
    for chunk in stream(true, wire).collect::<Vec<_>>().await {
        sdk.push(&chunk.unwrap());
    }
    sdk.assert_call(0, 0, "tool_a", "alpha", "{\"a\":1}");
    sdk.assert_call(0, 1, "tool_b", "beta", "{\"b\":2}");
    sdk.assert_call(0, 2, "tool_empty", "empty", "");
}

fn unfinished_frames(converse: bool, tool: bool) -> Vec<Bytes> {
    if converse {
        if tool {
            vec![
                frame(
                    "contentBlockStart",
                    json!({"contentBlockIndex":0,
            "start":{"toolUse":{"toolUseId":"tool_a","name":"alpha"}}}),
                ),
                frame(
                    "contentBlockDelta",
                    json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"x\":"}}}),
                ),
            ]
        } else {
            vec![frame(
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"text":"visible"}}),
            )]
        }
    } else {
        let (block, delta) = if tool {
            (
                json!({"type":"tool_use","id":"tool_a","name":"alpha","input":{}}),
                json!({"type":"input_json_delta","partial_json":"{\"x\":"}),
            )
        } else {
            (
                json!({"type":"text","text":""}),
                json!({"type":"text_delta","text":"visible"}),
            )
        };
        vec![
            invoke(json!({"type":"content_block_start","index":0,"content_block":block})),
            invoke(json!({"type":"content_block_delta","index":0,"delta":delta})),
        ]
    }
}

#[tokio::test]
async fn bedrock_frame_boundary_eof_requires_semantic_terminal_after_text_or_partial_tool() {
    for converse in [false, true] {
        for frames in [
            Vec::new(),
            unfinished_frames(converse, false),
            unfinished_frames(converse, true),
        ] {
            let chunks: Vec<_> = stream(converse, frames.into_iter().map(Ok).collect())
                .collect()
                .await;
            assert_eq!(chunks.iter().filter(|c| c.is_err()).count(), 1);
            assert!(matches!(
                chunks.last(),
                Some(Err(Error::Upstream { status: 0, .. }))
            ));
        }
    }
}

#[tokio::test]
async fn bedrock_semantic_terminal_is_required_even_if_stop_reason_or_metadata_arrived() {
    let invoke_delta = invoke(
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":1}}),
    );
    let converse_metadata = frame(
        "metadata",
        json!({"usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}),
    );
    for (converse, last) in [(false, invoke_delta), (true, converse_metadata)] {
        let chunks: Vec<_> = stream(converse, vec![Ok(last)]).collect().await;
        assert!(matches!(
            chunks.last(),
            Some(Err(Error::Upstream { status: 0, .. }))
        ));
    }
}

#[tokio::test]
async fn bedrock_semantic_finish_accepts_eof_and_converse_flushes_without_metadata() {
    for converse in [false, true] {
        let mut frames = unfinished_frames(converse, false);
        frames.push(if converse {
            frame("messageStop", json!({"stopReason":"end_turn"}))
        } else {
            invoke(json!({"type":"message_stop"}))
        });
        let chunks: Vec<_> = stream(converse, frames.into_iter().map(Ok).collect())
            .collect()
            .await;
        assert!(chunks.iter().all(|c| c.is_ok()));
        if converse {
            assert_eq!(
                chunks.last().unwrap().as_ref().unwrap().choices[0]
                    .finish_reason
                    .as_deref(),
                Some("stop")
            );
        }
    }
}

#[tokio::test]
async fn bedrock_transport_error_is_not_followed_by_a_duplicate_eof_error() {
    for converse in [false, true] {
        let error = reqwest::Client::new().get("http://[").build().unwrap_err();
        let mut frames: Vec<_> = unfinished_frames(converse, false)
            .into_iter()
            .map(Ok)
            .collect();
        frames.push(Err(error));
        let chunks: Vec<_> = stream(converse, frames).collect().await;
        assert_eq!(chunks.iter().filter(|c| c.is_err()).count(), 1);
        assert!(matches!(chunks.last(), Some(Err(Error::Streaming(_)))));
    }
}

#[tokio::test]
async fn bedrock_incomplete_frame_errors_once_even_after_semantic_finish() {
    for converse in [false, true] {
        let terminal = if converse {
            frame("messageStop", json!({"stopReason":"end_turn"}))
        } else {
            invoke(json!({"type":"message_stop"}))
        };
        let frame = unfinished_frames(converse, false).pop().unwrap();
        for cut in [1, 12, frame.len() - 1] {
            let chunks: Vec<_> =
                stream(converse, vec![Ok(terminal.clone()), Ok(frame.slice(..cut))])
                    .collect()
                    .await;
            assert_eq!(chunks.iter().filter(|c| c.is_err()).count(), 1);
            assert!(matches!(chunks.last(), Some(Err(Error::Streaming(_)))));
        }
    }
}
