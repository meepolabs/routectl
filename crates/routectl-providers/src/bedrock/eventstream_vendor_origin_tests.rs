//! The vendor-origin usage flag on the native Bedrock stream decoders is
//! derived from the endpoint that actually answered: true only when its
//! host is the Bedrock-runtime host the validated region derives.

use super::*;
use aws_smithy_types::event_stream::{Header, HeaderValue};
use futures::stream::StreamExt;

fn event_frame(event_type: &str, payload: &str) -> Vec<u8> {
    let frame = Message::new(Bytes::from(payload.as_bytes().to_vec()))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".to_string().into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String(event_type.to_string().into()),
        ));
    let mut buf = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf)
        .expect("encode eventstream frame");
    buf
}

fn invoke_chunk(inner_event: &str) -> Vec<u8> {
    let b64 = B64_STANDARD.encode(inner_event.as_bytes());
    event_frame("chunk", &format!(r#"{{"bytes":"{b64}"}}"#))
}

const INVOKE_OPENING: &str = r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","usage":{"input_tokens":7,"output_tokens":1}}}"#;

async fn decode(shape: BedrockApiShape, region: &str, endpoint: &str) -> Vec<ChatChunk> {
    let wire = match shape {
        BedrockApiShape::Invoke => [
            invoke_chunk(INVOKE_OPENING),
            invoke_chunk(r#"{"type":"message_stop"}"#),
        ]
        .concat(),
        BedrockApiShape::Converse => [
            event_frame("messageStop", r#"{"stopReason":"end_turn"}"#),
            event_frame(
                "metadata",
                r#"{"usage":{"inputTokens":7,"outputTokens":1,"totalTokens":8}}"#,
            ),
        ]
        .concat(),
    };
    let endpoint = reqwest::Url::parse(endpoint).unwrap();
    response_stream(
        shape,
        "bedrock:origin".into(),
        region,
        &endpoint,
        futures::stream::iter(vec![Ok(Bytes::from(wire))]),
    )
    .map(|c| c.expect("no stream error"))
    .collect()
    .await
}

/// The vendor flag carried by the usage-bearing chunk of each shape.
async fn vendor_flag(shape: BedrockApiShape, region: &str, endpoint: &str) -> bool {
    let chunks = decode(shape, region, endpoint).await;
    let meta = chunks
        .iter()
        .find_map(|c| c.upstream_meta.as_ref())
        .expect("usage-bearing chunk");
    match shape {
        BedrockApiShape::Invoke => {
            meta.opening_usage
                .as_ref()
                .expect("opening usage carried")
                .from_vendor_endpoint
        }
        BedrockApiShape::Converse => meta.usage_from_vendor_endpoint.expect("flag set"),
    }
}

const SHAPES: [BedrockApiShape; 2] = [BedrockApiShape::Invoke, BedrockApiShape::Converse];

#[tokio::test]
async fn flag_is_true_for_the_derived_commercial_govcloud_and_china_runtime_hosts() {
    let cases = [
        (
            "us-west-2",
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/m/invoke",
        ),
        (
            "us-gov-west-1",
            "https://bedrock-runtime.us-gov-west-1.amazonaws.com/model/m/converse-stream",
        ),
        (
            "cn-north-1",
            "https://bedrock-runtime.cn-north-1.amazonaws.com.cn/model/m/invoke",
        ),
    ];

    for shape in SHAPES {
        for (region, endpoint) in cases {
            assert!(
                vendor_flag(shape, region, endpoint).await,
                "{shape:?} {endpoint}"
            );
        }
    }
}

#[tokio::test]
async fn flag_is_false_when_the_answering_host_is_not_the_derived_runtime_host() {
    let cases = [
        ("us-west-2", "http://127.0.0.1:9/model/m/invoke"),
        (
            "us-west-2",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/invoke",
        ),
        (
            "us-west-2",
            "https://bedrock-runtime.us-west-2.amazonaws.com.evil.example/",
        ),
        (
            "us-west-2",
            "https://evil.bedrock-runtime.us-west-2.amazonaws.com/",
        ),
        ("us-west-2", "https://bedrock-mantle.us-west-2.api.aws/"),
        (
            "us-west-2",
            "http://bedrock-runtime.us-west-2.amazonaws.com/",
        ),
        (
            "us-west-2",
            "https://bedrock-runtime.us-west-2.amazonaws.com:8443/",
        ),
        (
            "us-west-2",
            "https://u@bedrock-runtime.us-west-2.amazonaws.com/",
        ),
        (
            "us-west-2",
            "https://:p@bedrock-runtime.us-west-2.amazonaws.com/",
        ),
        (
            "cn-north-1",
            "https://bedrock-runtime.cn-north-1.amazonaws.com/",
        ),
        (
            "us-west-2.evil.example",
            "https://bedrock-runtime.us-west-2.evil.example.amazonaws.com/",
        ),
    ];

    for shape in SHAPES {
        for (region, endpoint) in cases {
            assert!(
                !vendor_flag(shape, region, endpoint).await,
                "{shape:?} {region} {endpoint}"
            );
        }
    }
}
