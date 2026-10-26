//! Wire-level tests for the Bedrock beta carrier with no repair of its own:
//! the provider is aimed at a mock bedrock-runtime that answers like AWS,
//! rejecting a request whose `anthropic_beta` carries a trigger flag with a
//! 400 `ValidationException` (flat `{"message"}` body, discriminator in
//! `x-amzn-errortype`) and serving a success otherwise. Every request body
//! the mock received is recorded, so each test asserts the exact flags that
//! reached the wire. A named beta rejection is returned to the caller after
//! one upstream call on every carrier and call: the router owns the repair.

use std::sync::{Arc, Mutex, PoisonError};

use aws_smithy_types::event_stream::{Header, HeaderValue, Message as FrameMessage};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth};
use routectl_core::{ChatRequest, Error, Provider};

/// The flag the rejection names.
const REJECTED: &str = "zz-named-2099-01-01";

/// A flat Bedrock `ValidationException` message naming [`REJECTED`]; the
/// provider surfaces it without reading it.
fn rejection_message() -> String {
    format!("Unexpected value(s) `{REJECTED}` for the `anthropic-beta` header.")
}
const KEPT_BETA: &str = "context-management-2025-06-27";
const ERROR_TYPE_HEADER: &str =
    "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/";

/// Which wire call a test drives; selects the success body the mock serves.
#[derive(Clone, Copy)]
enum Call {
    Complete,
    Stream,
    CountTokens,
}

/// A mock bedrock-runtime: the first rule whose trigger flag is on the wire
/// answers 400 with that rule's message; otherwise the call's success body.
struct AwsLikeUpstream {
    shape: BedrockApiShape,
    call: Call,
    rules: Vec<(String, String)>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Respond for AwsLikeUpstream {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("request body is JSON");
        let betas = wire_betas(&body);
        self.bodies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(body);
        if let Some((_, message)) = self.rules.iter().find(|(flag, _)| betas.contains(flag)) {
            return ResponseTemplate::new(400)
                .insert_header("x-amzn-ErrorType", ERROR_TYPE_HEADER)
                .set_body_raw(
                    json!({ "message": message }).to_string(),
                    "application/json",
                );
        }
        success(self.shape, self.call)
    }
}

/// The `anthropic_beta` array wherever this request carries it: the Invoke
/// body, the Converse extras bag, or either one inside a CountTokens input.
fn wire_betas(body: &Value) -> Vec<String> {
    let invoke_tokens = body["input"]["invokeModel"]["body"].as_str().map(|b64| {
        let raw = B64.decode(b64).expect("count-tokens body is base64");
        serde_json::from_slice::<Value>(&raw).expect("count-tokens body is JSON")
    });
    let bag = match &invoke_tokens {
        Some(inner) => &inner["anthropic_beta"],
        None if body["input"]["converse"].is_object() => {
            &body["input"]["converse"]["additionalModelRequestFields"]["anthropic_beta"]
        }
        None if body["additionalModelRequestFields"].is_object() => {
            &body["additionalModelRequestFields"]["anthropic_beta"]
        }
        None => &body["anthropic_beta"],
    };
    bag.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn success(shape: BedrockApiShape, call: Call) -> ResponseTemplate {
    match (call, shape) {
        (Call::CountTokens, _) => {
            ResponseTemplate::new(200).set_body_json(json!({ "inputTokens": 7 }))
        }
        (Call::Complete, BedrockApiShape::Invoke) => {
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_repair",
                "type": "message",
                "role": "assistant",
                "model": "anthropic.claude-opus-5-5",
                "content": [{ "type": "text", "text": "ok" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 3, "output_tokens": 1 }
            }))
        }
        (Call::Complete, BedrockApiShape::Converse) => {
            ResponseTemplate::new(200).set_body_json(json!({
                "output": { "message": { "role": "assistant", "content": [{ "text": "ok" }] } },
                "stopReason": "end_turn",
                "usage": { "inputTokens": 3, "outputTokens": 1, "totalTokens": 4 }
            }))
        }
        (Call::Stream, shape) => ResponseTemplate::new(200)
            .set_body_raw(stream_bytes(shape), "application/vnd.amazon.eventstream"),
    }
}

fn frame(event_type: &str, payload: String) -> Vec<u8> {
    let message = FrameMessage::new(Bytes::from(payload.into_bytes()))
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
            r#"{"type":"message_start","message":{"id":"msg_repair","type":"message","role":"assistant","content":[],"model":"anthropic.claude-opus-5-5","usage":{"input_tokens":3,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{"type":"message_stop"}"#,
        ]
        .iter()
        .flat_map(|event| {
            frame(
                "chunk",
                json!({ "bytes": B64.encode(event.as_bytes()) }).to_string(),
            )
        })
        .collect(),
        BedrockApiShape::Converse => [
            ("messageStart", r#"{"role":"assistant"}"#),
            ("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"ok"}}"#),
            ("messageStop", r#"{"stopReason":"end_turn"}"#),
        ]
        .iter()
        .flat_map(|(event_type, payload)| frame(event_type, (*payload).to_string()))
        .collect(),
    }
}

/// A provider on one lane, aimed at a mock upstream that applies `rules`.
struct Lane {
    provider: BedrockProvider,
    bodies: Arc<Mutex<Vec<Value>>>,
    _server: MockServer,
}

impl Lane {
    async fn start(shape: BedrockApiShape, call: Call, rules: &[(&str, &str)]) -> Self {
        let server = MockServer::start().await;
        let bodies = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .respond_with(AwsLikeUpstream {
                shape,
                call,
                rules: rules
                    .iter()
                    .map(|(flag, message)| ((*flag).to_string(), (*message).to_string()))
                    .collect(),
                bodies: Arc::clone(&bodies),
            })
            .mount(&server)
            .await;
        let creds = BedrockCreds::BearerKey {
            key: "beta-withhold-test-key".into(),
        };
        let cfg = BedrockConfig {
            id: "bedrock-beta-withhold".into(),
            region: "us-east-1".into(),
            model_id: "us.anthropic.claude-opus-5-5".into(),
            api_shape: shape,
            creds: creds.clone(),
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
        };
        let resolved = auth::resolve(&creds, "us-east-1").await.expect("resolve");
        let mut provider = BedrockProvider::new(cfg, resolved).expect("canonical region");
        provider.client =
            crate::http_client::build_no_redirect(None, &server.uri()).expect("loopback client");
        provider.runtime_origin = server.uri();
        Self {
            provider,
            bodies,
            _server: server,
        }
    }

    /// The `anthropic_beta` array of every request the upstream received.
    fn wire_betas(&self) -> Vec<Vec<String>> {
        self.bodies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(wire_betas)
            .collect()
    }

    async fn send_request(&self, call: Call, req: ChatRequest) -> Result<(), Error> {
        match call {
            Call::Complete => self.provider.complete(req).await.map(drop),
            Call::CountTokens => self.provider.count_tokens(req).await.map(drop),
            Call::Stream => {
                let chunks: Vec<_> = self.provider.stream(req).await?.collect().await;
                assert!(!chunks.is_empty(), "the stream must yield chunks");
                for chunk in chunks {
                    chunk.expect("no stream error after a successful open");
                }
                Ok(())
            }
        }
    }
}

fn request(client_betas: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "us.anthropic.claude-opus-5-5".into(),
        messages: vec![routectl_core::test_utils::user_msg("hello")].into(),
        max_tokens: Some(16),
        anthropic_beta: client_betas.iter().map(|b| (*b).to_string()).collect(),
        ..Default::default()
    }
}

/// A request whose `pinned` flags came from `header_extras["anthropic-beta"]`:
/// the router unions them into `anthropic_beta` and records them as the
/// operator floor.
fn header_pinned_request(client_betas: &[&str], pinned: &[&str]) -> ChatRequest {
    let mut req = request(client_betas);
    req.anthropic_beta
        .extend(pinned.iter().map(|b| (*b).to_string()));
    req.routectl_internal.operator_betas = pinned.iter().map(|b| (*b).to_string()).collect();
    req
}

/// `req` with `withheld` as the caller-decided withheld set.
fn withholding(mut req: ChatRequest, withheld: &[&str]) -> ChatRequest {
    req.routectl_internal.withheld_betas = withheld.iter().map(|b| (*b).to_string()).collect();
    req
}

fn betas(flags: &[&str]) -> Vec<String> {
    flags.iter().map(|f| (*f).to_string()).collect()
}

fn upstream_message(err: &Error) -> String {
    let Error::Upstream { status, body, .. } = err else {
        panic!("expected the upstream rejection, got {err:?}");
    };
    assert_eq!(*status, 400);
    let parsed: Value = serde_json::from_str(body).expect("carried envelope is JSON");
    parsed["message"].as_str().expect("message").to_string()
}

// ---------------------------------------------------------------------------
// A named rejection is returned after one upstream call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_named_beta_rejection_reaches_the_caller_after_one_upstream_call() {
    let rows = [
        ("invoke complete", BedrockApiShape::Invoke, Call::Complete),
        ("invoke stream", BedrockApiShape::Invoke, Call::Stream),
        (
            "invoke count_tokens",
            BedrockApiShape::Invoke,
            Call::CountTokens,
        ),
        (
            "converse complete",
            BedrockApiShape::Converse,
            Call::Complete,
        ),
        ("converse stream", BedrockApiShape::Converse, Call::Stream),
        (
            "converse count_tokens",
            BedrockApiShape::Converse,
            Call::CountTokens,
        ),
    ];
    for (name, shape, call) in rows {
        // Arrange
        let message = rejection_message();
        let lane = Lane::start(shape, call, &[(REJECTED, &message)]).await;

        // Act
        let outcome = lane
            .send_request(call, request(&[KEPT_BETA, REJECTED]))
            .await;

        // Assert
        let err = outcome.expect_err("the rejection is surfaced");
        assert_eq!(upstream_message(&err), message, "{name}");
        assert_eq!(
            lane.wire_betas(),
            vec![betas(&[KEPT_BETA, REJECTED])],
            "{name}: one upstream call, carrying the client set"
        );
    }
}

#[tokio::test]
async fn a_withheld_flag_pinned_by_header_extras_is_sent() {
    // Arrange
    let message = rejection_message();
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[(REJECTED, &message)],
    )
    .await;
    let req = withholding(
        header_pinned_request(&[KEPT_BETA], &[REJECTED]),
        &[REJECTED],
    );

    // Act
    let pinned = lane.send_request(Call::Complete, req).await;

    // Assert: the pin reaches the wire; the upstream's answer is surfaced.
    let err = pinned.expect_err("the pinned flag ships and is rejected");
    assert_eq!(upstream_message(&err), message);
    assert_eq!(lane.wire_betas(), vec![betas(&[KEPT_BETA, REJECTED])]);
}

// ---------------------------------------------------------------------------
// The withheld set at egress
// ---------------------------------------------------------------------------

/// A withheld flag never reaches the wire, while an unrelated client flag in
/// the same header still ships.
#[tokio::test]
async fn a_withheld_flag_never_reaches_the_wire() {
    let rows = [
        ("invoke complete", BedrockApiShape::Invoke, Call::Complete),
        ("invoke stream", BedrockApiShape::Invoke, Call::Stream),
        (
            "converse complete",
            BedrockApiShape::Converse,
            Call::Complete,
        ),
        ("converse stream", BedrockApiShape::Converse, Call::Stream),
    ];
    for (name, shape, call) in rows {
        // Arrange: the upstream would reject the withheld flag if it arrived.
        let message = rejection_message();
        let lane = Lane::start(shape, call, &[(REJECTED, &message)]).await;
        let req = withholding(request(&[KEPT_BETA, REJECTED]), &[REJECTED]);

        // Act
        let outcome = lane.send_request(call, req).await;

        // Assert
        outcome.unwrap_or_else(|e| panic!("{name}: served without a rejection: {e:?}"));
        assert_eq!(lane.wire_betas(), vec![betas(&[KEPT_BETA])], "{name}");
    }
}
