//! The bytes each Bedrock send site deposits on the attempt's wire tap are
//! exactly the bytes the upstream received, on both API shapes and on both
//! the complete and the stream path.

use std::sync::{Arc, Mutex, PoisonError};

use aws_smithy_types::event_stream::{Header, HeaderValue, Message as FrameMessage};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use futures::StreamExt;
use routectl_core::wire_check::WireTap;
use routectl_core::{ChatRequest, Provider};
use serde_json::json;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

use super::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth};

#[derive(Clone, Copy, Debug)]
enum Call {
    Complete,
    Stream,
}

struct Capture {
    shape: BedrockApiShape,
    call: Call,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Respond for Capture {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.bodies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.body.clone());
        match (self.call, self.shape) {
            (Call::Complete, BedrockApiShape::Invoke) => {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "msg_d", "type": "message", "role": "assistant",
                    "model": "anthropic.claude-opus-5-5",
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 3, "output_tokens": 1}
                }))
            }
            (Call::Complete, BedrockApiShape::Converse) => ResponseTemplate::new(200)
                .set_body_json(json!({
                    "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
                    "stopReason": "end_turn",
                    "usage": {"inputTokens": 3, "outputTokens": 1, "totalTokens": 4}
                })),
            (Call::Stream, shape) => ResponseTemplate::new(200)
                .set_body_raw(stream_bytes(shape), "application/vnd.amazon.eventstream"),
        }
    }
}

fn frame(event_type: &str, payload: &str) -> Vec<u8> {
    let message = FrameMessage::new(Bytes::from(payload.to_string().into_bytes()))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".to_string().into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String(event_type.to_string().into()),
        ));
    let mut buf = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&message, &mut buf).expect("encode frame");
    buf
}

fn stream_bytes(shape: BedrockApiShape) -> Vec<u8> {
    match shape {
        BedrockApiShape::Invoke => [
            r#"{"type":"message_start","message":{"id":"msg_d","type":"message","role":"assistant","content":[],"model":"anthropic.claude-opus-5-5","usage":{"input_tokens":3,"output_tokens":1}}}"#,
            r#"{"type":"message_stop"}"#,
        ]
        .iter()
        .flat_map(|event| {
            frame(
                "chunk",
                &json!({ "bytes": B64.encode(event.as_bytes()) }).to_string(),
            )
        })
        .collect(),
        BedrockApiShape::Converse => [
            ("messageStart", r#"{"role":"assistant"}"#),
            ("messageStop", r#"{"stopReason":"end_turn"}"#),
        ]
        .iter()
        .flat_map(|(event_type, payload)| frame(event_type, payload))
        .collect(),
    }
}

async fn lane(
    shape: BedrockApiShape,
    call: Call,
) -> (BedrockProvider, Arc<Mutex<Vec<Vec<u8>>>>, MockServer) {
    let server = MockServer::start().await;
    let bodies = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .respond_with(Capture {
            shape,
            call,
            bodies: Arc::clone(&bodies),
        })
        .mount(&server)
        .await;
    let creds = BedrockCreds::BearerKey {
        key: "deposit-test-key".into(),
    };
    let cfg = BedrockConfig {
        id: "bedrock-deposit".into(),
        region: "us-east-1".into(),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        api_shape: shape,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = auth::resolve(&creds, "us-east-1").await.expect("resolve");
    let mut provider = BedrockProvider::new(cfg, resolved).expect("canonical region");
    provider.client =
        crate::http_client::build_no_redirect(None, &server.uri()).expect("loopback client");
    provider.runtime_origin = server.uri();
    (provider, bodies, server)
}

fn tapped_request() -> (ChatRequest, WireTap) {
    let tap = WireTap::default();
    let mut req = ChatRequest {
        model: "us.anthropic.claude-opus-5-5".into(),
        messages: vec![routectl_core::test_utils::user_msg("hello")].into(),
        max_tokens: Some(16),
        ..Default::default()
    };
    req.routectl_internal.wire_tap = Some(tap.clone());
    (req, tap)
}

async fn send(provider: &BedrockProvider, call: Call, req: ChatRequest) {
    match call {
        Call::Complete => drop(provider.complete(req).await.expect("completes")),
        Call::Stream => {
            let chunks: Vec<_> = provider.stream(req).await.expect("opens").collect().await;
            for chunk in chunks {
                chunk.expect("no stream error");
            }
        }
    }
}

#[tokio::test]
async fn each_send_site_deposits_the_bytes_the_upstream_received() {
    for shape in [BedrockApiShape::Invoke, BedrockApiShape::Converse] {
        for call in [Call::Complete, Call::Stream] {
            let (provider, bodies, _server) = lane(shape, call).await;
            let (req, tap) = tapped_request();

            send(&provider, call, req).await;

            let wire = bodies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            assert_eq!(wire.len(), 1, "{shape:?} {call:?}: one upstream call");
            let contents = tap.take_request();
            let deposited = contents
                .request()
                .expect("the send site deposited the body");
            assert_eq!(
                Some(deposited.as_ref()),
                wire.first().map(Vec::as_slice),
                "{shape:?} {call:?}: deposit differs from the transmitted body"
            );
        }
    }
}

#[tokio::test]
async fn count_tokens_deposits_nothing() {
    let (provider, _bodies, _server) = lane(BedrockApiShape::Invoke, Call::Complete).await;
    let (req, tap) = tapped_request();

    let _ = provider.count_tokens(req).await;

    assert!(tap.take_request().request().is_none());
}
