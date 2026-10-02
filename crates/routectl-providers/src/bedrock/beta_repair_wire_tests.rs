//! Wire-level tests for the named beta-rejection repair: the provider is aimed
//! at a mock bedrock-runtime that answers like AWS, rejecting a request whose
//! `anthropic_beta` carries a trigger flag with a 400 `ValidationException`
//! (flat `{"message"}` body, discriminator in `x-amzn-errortype`) and serving
//! a success otherwise. Every request body the mock received is recorded, so
//! each test asserts the exact flags that reached the wire.

use std::sync::{Arc, Mutex, PoisonError};

use aws_smithy_types::event_stream::{Header, HeaderValue, Message as FrameMessage};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

use super::tests::{INVOKE_MIXED, INVOKE_NEVER_ISSUED, NON_NAMING};
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth};
use routectl_core::{ChatRequest, Error, Provider};

const NEVER_ISSUED: &str = "zz-probe-2099-01-01";
const KEPT_BETA: &str = "context-management-2025-06-27";
const ERROR_TYPE_HEADER: &str =
    "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/";

/// Converse wraps the Invoke message in this prefix and is otherwise
/// byte-identical, so the Converse fixtures are the captured Invoke envelope
/// behind the captured prefix.
fn converse_wrapped(message: &str) -> String {
    format!("The model returned the following errors: {message}")
}

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
    async fn start(
        shape: BedrockApiShape,
        call: Call,
        floor: &[&str],
        rules: &[(&str, &str)],
    ) -> Self {
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
            key: "beta-repair-test-key".into(),
        };
        let cfg = BedrockConfig {
            id: "bedrock-beta-repair".into(),
            region: "us-east-1".into(),
            model_id: "us.anthropic.claude-opus-5-5".into(),
            api_shape: shape,
            creds: creds.clone(),
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: floor.iter().map(|f| (*f).to_string()).collect(),
            allowed_betas: Vec::new(),
            allowed_body_fields: Vec::new(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
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

    async fn send(&self, call: Call, client_betas: &[&str]) -> Result<(), Error> {
        self.send_request(call, request(client_betas)).await
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

/// The rejection reaches the caller unrepaired, and the lane did not remember
/// anything: a second request still ships the named flag.
async fn assert_unrepaired_and_unremembered(lane: &Lane, call: Call, client: &[&str]) {
    let first = lane.send(call, client).await.expect_err("no repair");
    let second = lane.send(call, client).await.expect_err("still no repair");

    assert_eq!(upstream_message(&first), upstream_message(&second));
    let sent = lane.wire_betas();
    assert_eq!(sent.len(), 2, "exactly one request per call: no retry");
    assert!(
        sent[1].iter().any(|b| b == NEVER_ISSUED),
        "nothing may be remembered from an unrepaired rejection"
    );
}

// ---------------------------------------------------------------------------
// Repair on every carrier and call
// ---------------------------------------------------------------------------

async fn assert_repairs_once(shape: BedrockApiShape, call: Call, message: &str) {
    // Arrange
    let lane = Lane::start(shape, call, &[], &[(NEVER_ISSUED, message)]).await;

    // Act
    let outcome = lane.send(call, &[KEPT_BETA, NEVER_ISSUED]).await;

    // Assert
    outcome.expect("the retry without the named flag succeeds");
    assert_eq!(
        lane.wire_betas(),
        vec![betas(&[KEPT_BETA, NEVER_ISSUED]), betas(&[KEPT_BETA])],
        "one retry, carrying every flag except exactly the named one"
    );
}

#[tokio::test]
async fn invoke_complete_retries_once_without_the_named_flag() {
    assert_repairs_once(BedrockApiShape::Invoke, Call::Complete, INVOKE_NEVER_ISSUED).await;
}

#[tokio::test]
async fn converse_complete_retries_once_on_the_prefixed_envelope() {
    let message = converse_wrapped(INVOKE_NEVER_ISSUED);
    assert_repairs_once(BedrockApiShape::Converse, Call::Complete, &message).await;
}

#[tokio::test]
async fn invoke_stream_retries_once_before_any_byte_reaches_the_caller() {
    assert_repairs_once(BedrockApiShape::Invoke, Call::Stream, INVOKE_NEVER_ISSUED).await;
}

#[tokio::test]
async fn converse_stream_retries_once_before_any_byte_reaches_the_caller() {
    let message = converse_wrapped(INVOKE_NEVER_ISSUED);
    assert_repairs_once(BedrockApiShape::Converse, Call::Stream, &message).await;
}

#[tokio::test]
async fn invoke_count_tokens_retries_once_without_the_named_flag() {
    assert_repairs_once(
        BedrockApiShape::Invoke,
        Call::CountTokens,
        INVOKE_NEVER_ISSUED,
    )
    .await;
}

#[tokio::test]
async fn converse_count_tokens_retries_once_without_the_named_flag() {
    let message = converse_wrapped(INVOKE_NEVER_ISSUED);
    assert_repairs_once(BedrockApiShape::Converse, Call::CountTokens, &message).await;
}

#[tokio::test]
async fn a_later_request_on_the_lane_withholds_the_confirmed_flag_without_a_400() {
    // Arrange: one repaired request teaches the lane.
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, INVOKE_NEVER_ISSUED)],
    )
    .await;
    lane.send(Call::Complete, &[KEPT_BETA, NEVER_ISSUED])
        .await
        .expect("repaired");

    // Act
    let later = lane.send(Call::Complete, &[KEPT_BETA, NEVER_ISSUED]).await;

    // Assert: the later call is a single request that never carried the flag.
    later.expect("served first time");
    let sent = lane.wire_betas();
    assert_eq!(sent.len(), 3, "the later call must not need a retry");
    assert_eq!(sent[2], betas(&[KEPT_BETA]));
}

#[tokio::test]
async fn a_converse_stream_repair_is_remembered_for_later_requests() {
    // Arrange
    let lane = Lane::start(
        BedrockApiShape::Converse,
        Call::Stream,
        &[],
        &[(NEVER_ISSUED, &converse_wrapped(INVOKE_NEVER_ISSUED))],
    )
    .await;
    lane.send(Call::Stream, &[KEPT_BETA, NEVER_ISSUED])
        .await
        .expect("repaired");

    // Act
    let later = lane.send(Call::Stream, &[KEPT_BETA, NEVER_ISSUED]).await;

    // Assert
    later.expect("served first time");
    let sent = lane.wire_betas();
    assert_eq!(sent.len(), 3, "the later call must not need a retry");
    assert_eq!(sent[2], betas(&[KEPT_BETA]));
}

#[tokio::test]
async fn a_count_tokens_repair_retries_but_is_not_remembered() {
    // Arrange: a token-count rejection says nothing about inference.
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::CountTokens,
        &[],
        &[(NEVER_ISSUED, INVOKE_NEVER_ISSUED)],
    )
    .await;
    lane.send(Call::CountTokens, &[KEPT_BETA, NEVER_ISSUED])
        .await
        .expect("repaired");

    // Act
    let later = lane
        .send(Call::CountTokens, &[KEPT_BETA, NEVER_ISSUED])
        .await;

    // Assert: the later call ships the flag again and repairs again.
    later.expect("repaired again");
    assert_eq!(
        lane.wire_betas(),
        vec![
            betas(&[KEPT_BETA, NEVER_ISSUED]),
            betas(&[KEPT_BETA]),
            betas(&[KEPT_BETA, NEVER_ISSUED]),
            betas(&[KEPT_BETA]),
        ]
    );
}

#[tokio::test]
async fn a_remembered_flag_pinned_by_header_extras_is_not_pre_stripped() {
    // Arrange: an unpinned request teaches the lane the flag.
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, INVOKE_NEVER_ISSUED)],
    )
    .await;
    lane.send(Call::Complete, &[KEPT_BETA, NEVER_ISSUED])
        .await
        .expect("repaired");

    // Act: a later request on a model that pins the same flag.
    let pinned = lane
        .send_request(
            Call::Complete,
            header_pinned_request(&[KEPT_BETA], &[NEVER_ISSUED]),
        )
        .await;

    // Assert: the pin reaches the wire; the upstream's answer is surfaced.
    let err = pinned.expect_err("the pinned flag ships and is rejected");
    assert_eq!(upstream_message(&err), INVOKE_NEVER_ISSUED);
    assert_eq!(lane.wire_betas()[2], betas(&[KEPT_BETA, NEVER_ISSUED]));
}

#[tokio::test]
async fn a_multi_flag_envelope_strips_every_named_flag_in_one_retry() {
    // Arrange: the captured three-flag envelope, triggered by the one named
    // flag the built-in deny set does not already withhold.
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, INVOKE_MIXED)],
    )
    .await;
    let client = [
        KEPT_BETA,
        "advanced-tool-use-2025-11-20",
        "prompt-caching-scope-2026-01-05",
        NEVER_ISSUED,
    ];

    // Act
    let outcome = lane.send(Call::Complete, &client).await;

    // Assert
    outcome.expect("repaired");
    assert_eq!(
        lane.wire_betas(),
        vec![betas(&[KEPT_BETA, NEVER_ISSUED]), betas(&[KEPT_BETA])]
    );
}

// ---------------------------------------------------------------------------
// Refusals: no retry, no memo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unanchored_envelope_is_not_repaired() {
    let message = format!("Error: {INVOKE_NEVER_ISSUED}");
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, &message)],
    )
    .await;

    assert_unrepaired_and_unremembered(&lane, Call::Complete, &[KEPT_BETA, NEVER_ISSUED]).await;
}

#[tokio::test]
async fn an_envelope_naming_a_flag_the_client_did_not_send_is_not_repaired() {
    // The captured envelope names two flags this client never sent.
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, INVOKE_MIXED)],
    )
    .await;

    assert_unrepaired_and_unremembered(&lane, Call::Complete, &[KEPT_BETA, NEVER_ISSUED]).await;
}

#[tokio::test]
async fn an_envelope_naming_an_operator_floor_flag_is_not_repaired() {
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[NEVER_ISSUED],
        &[(NEVER_ISSUED, INVOKE_NEVER_ISSUED)],
    )
    .await;

    assert_unrepaired_and_unremembered(&lane, Call::Complete, &[KEPT_BETA, NEVER_ISSUED]).await;
}

#[tokio::test]
async fn an_envelope_naming_a_header_extras_pinned_flag_is_not_repaired() {
    // Arrange
    let lane = Lane::start(
        BedrockApiShape::Converse,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, &converse_wrapped(INVOKE_NEVER_ISSUED))],
    )
    .await;
    let pinned = || header_pinned_request(&[KEPT_BETA], &[NEVER_ISSUED]);

    // Act
    let first = lane.send_request(Call::Complete, pinned()).await;
    let second = lane.send_request(Call::Complete, pinned()).await;

    // Assert: one request per call, the pin never stripped or remembered.
    first.expect_err("no repair");
    second.expect_err("still no repair");
    assert_eq!(
        lane.wire_betas(),
        vec![
            betas(&[KEPT_BETA, NEVER_ISSUED]),
            betas(&[KEPT_BETA, NEVER_ISSUED])
        ]
    );
}

#[tokio::test]
async fn the_non_naming_envelope_is_not_repaired() {
    let lane = Lane::start(
        BedrockApiShape::Converse,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, NON_NAMING)],
    )
    .await;

    assert_unrepaired_and_unremembered(&lane, Call::Complete, &[KEPT_BETA, NEVER_ISSUED]).await;
}

#[tokio::test]
async fn a_retry_that_draws_another_beta_rejection_is_not_repaired_again() {
    // Arrange: stripping the first named flag exposes a second one.
    const SECOND: &str = "zz-second-2099-01-01";
    let second_message = INVOKE_NEVER_ISSUED.replace(NEVER_ISSUED, SECOND);
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[],
        &[
            (NEVER_ISSUED, INVOKE_NEVER_ISSUED),
            (SECOND, &second_message),
        ],
    )
    .await;
    let client = [KEPT_BETA, NEVER_ISSUED, SECOND];

    // Act
    let err = lane
        .send(Call::Complete, &client)
        .await
        .expect_err("a second rejection is surfaced");
    let _ = lane.send(Call::Complete, &client).await;

    // Assert: one retry, the retry's own error surfaced, nothing remembered.
    assert_eq!(upstream_message(&err), second_message);
    let sent = lane.wire_betas();
    assert_eq!(
        sent[..2],
        [betas(&client), betas(&[KEPT_BETA, SECOND])],
        "exactly one retry"
    );
    assert_eq!(
        sent[2],
        betas(&client),
        "a failed retry must not teach the lane"
    );
}

// ---------------------------------------------------------------------------
// Refusals: a named flag the retry body would still carry
// ---------------------------------------------------------------------------

const DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";

/// The captured Invoke envelope, naming `flag` instead.
fn naming(flag: &str) -> String {
    INVOKE_NEVER_ISSUED.replace(NEVER_ISSUED, flag)
}

/// A thinking request whose display is `updates`, which the Converse body
/// gates behind its own beta.
fn display_updates_request(client_betas: &[&str]) -> ChatRequest {
    let mut req = request(client_betas);
    req.max_tokens = Some(2048);
    req.reasoning = Some(routectl_core::ReasoningConfig {
        effort: Some("medium".into()),
        max_tokens: None,
        exclude: None,
        enabled: Some(true),
    });
    req.routectl_internal.anthropic_thinking_display = Some("updates".into());
    req
}

/// A request whose body carries `output_config.format`, which gains the
/// structured-outputs beta.
fn structured_output_request(client_betas: &[&str]) -> ChatRequest {
    let mut req = request(client_betas);
    req.response_format = Some(json!({
        "type": "json_schema",
        "json_schema": { "name": "widget", "schema": { "type": "object" } },
    }));
    req
}

fn remembered(lane: &Lane) -> Vec<String> {
    lane.provider
        .rejected_betas
        .flags
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

/// One upstream call, the original rejection surfaced, nothing remembered.
async fn assert_refused_without_retry(lane: &Lane, req: ChatRequest, message: &str) {
    // Act
    let outcome = lane.send_request(Call::Complete, req).await;

    // Assert
    let err = outcome.expect_err("the rejection is surfaced unrepaired");
    assert_eq!(upstream_message(&err), message);
    assert_eq!(
        lane.wire_betas().len(),
        1,
        "a retry that would re-send the named flag must not be made"
    );
    assert!(remembered(lane).is_empty(), "nothing may be remembered");
}

#[tokio::test]
async fn converse_does_not_retry_when_the_named_flag_is_the_display_updates_beta() {
    // Arrange
    let message = converse_wrapped(&naming(DISPLAY_UPDATES_BETA));
    let lane = Lane::start(
        BedrockApiShape::Converse,
        Call::Complete,
        &[],
        &[(DISPLAY_UPDATES_BETA, &message)],
    )
    .await;
    let req = display_updates_request(&[KEPT_BETA, DISPLAY_UPDATES_BETA]);

    // Act + Assert
    assert_refused_without_retry(&lane, req, &message).await;
}

#[tokio::test]
async fn invoke_does_not_retry_when_the_named_flag_is_the_structured_outputs_beta() {
    // Arrange
    let flag = routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA;
    let message = naming(flag);
    let lane = Lane::start(
        BedrockApiShape::Invoke,
        Call::Complete,
        &[],
        &[(flag, &message)],
    )
    .await;
    let req = structured_output_request(&[KEPT_BETA, flag]);

    // Act + Assert
    assert_refused_without_retry(&lane, req, &message).await;
}

#[tokio::test]
async fn a_feature_carrying_request_still_repairs_an_unrelated_named_flag() {
    // Arrange: the body re-adds the display beta, but AWS named another flag.
    let lane = Lane::start(
        BedrockApiShape::Converse,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, &converse_wrapped(INVOKE_NEVER_ISSUED))],
    )
    .await;
    let req = display_updates_request(&[KEPT_BETA, NEVER_ISSUED]);

    // Act
    let outcome = lane.send_request(Call::Complete, req).await;

    // Assert
    outcome.expect("the retry without the named flag succeeds");
    let sent = lane.wire_betas();
    assert_eq!(sent.len(), 2, "exactly one retry");
    assert!(sent[1].iter().any(|b| b == DISPLAY_UPDATES_BETA));
    assert!(!sent[1].iter().any(|b| b == NEVER_ISSUED));
    assert_eq!(remembered(&lane), betas(&[NEVER_ISSUED]));
}

// ---------------------------------------------------------------------------
// Translation telemetry counts only dispatched bodies
// ---------------------------------------------------------------------------
//
// A Converse normalization of a request carrying a built-in rejected client
// beta records the `anthropic_beta_rejected_by_bedrock` drop exactly once, so
// that counter's delta is the number of bodies built. The registry is
// process-global: every test reaching this class shares the serial guard, and
// only deltas are read.

const REJECTED_CLIENT_BETA: &str = "advisor-tool-2026-03-01";

fn converse_rejected_beta_drops() -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| {
            e.lane == "bedrock-converse" && e.drop_class == "anthropic_beta_rejected_by_bedrock"
        })
        .map_or(0, |e| e.drop_count)
}

#[tokio::test]
#[serial_test::serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
async fn a_refused_repair_counts_one_translation_for_its_one_upstream_call() {
    // Arrange
    let message = converse_wrapped(&naming(DISPLAY_UPDATES_BETA));
    let lane = Lane::start(
        BedrockApiShape::Converse,
        Call::Complete,
        &[],
        &[(DISPLAY_UPDATES_BETA, &message)],
    )
    .await;
    let req = display_updates_request(&[KEPT_BETA, DISPLAY_UPDATES_BETA, REJECTED_CLIENT_BETA]);
    let before = converse_rejected_beta_drops();

    // Act
    let outcome = lane.send_request(Call::Complete, req).await;

    // Assert
    outcome.expect_err("the rejection is surfaced unrepaired");
    assert_eq!(lane.wire_betas().len(), 1, "precondition: no retry");
    assert_eq!(
        converse_rejected_beta_drops() - before,
        1,
        "one body was sent, so exactly one translation may be counted"
    );
}

#[tokio::test]
#[serial_test::serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
async fn a_successful_repair_counts_one_translation_per_upstream_call() {
    // Arrange
    let lane = Lane::start(
        BedrockApiShape::Converse,
        Call::Complete,
        &[],
        &[(NEVER_ISSUED, &converse_wrapped(INVOKE_NEVER_ISSUED))],
    )
    .await;
    let req = display_updates_request(&[KEPT_BETA, NEVER_ISSUED, REJECTED_CLIENT_BETA]);
    let before = converse_rejected_beta_drops();

    // Act
    let outcome = lane.send_request(Call::Complete, req).await;

    // Assert
    outcome.expect("repaired");
    assert_eq!(lane.wire_betas().len(), 2, "precondition: one retry");
    assert_eq!(
        converse_rejected_beta_drops() - before,
        2,
        "two bodies were sent, so exactly two translations may be counted"
    );
}
