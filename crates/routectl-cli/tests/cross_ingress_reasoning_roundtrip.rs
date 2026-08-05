//! Cross-ingress round-trip pin: a response routectl serves through the
//! Anthropic ingress must be accepted, unmodified, when a client echoes
//! it back on the OpenAI ingress.
//!
//! The two ingresses speak different reasoning vocabularies on the same
//! canonical type. The Anthropic egress renders reasoning as `thinking` /
//! `redacted_thinking` content blocks, and clients echo assistant turns
//! back verbatim -- including into a `reasoning_details` array when the
//! client normalizes to the OpenAI shape. Before the kind aliases, the
//! OpenAI ingress rejected both spellings with a 400: routectl refusing
//! its own output, before any upstream call.
//!
//! These tests drive the REAL adapters end to end -- capture from
//! `AnthropicIngress::render_response`, replay into
//! `OpenAiIngress::parse_request` -- rather than asserting on the serde
//! derive in isolation. The parse-level pins live in
//! `routectl-core/src/reasoning_ingest_tests.rs`.

use serde_json::{Value, json};

use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::openai::OpenAiIngress;
use routectl_core::{
    ChatResponse, Choice, Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Role,
    Usage,
};

const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// A canonical assistant response carrying both Anthropic-shape
/// reasoning kinds: a signed `Text` detail and an `Encrypted` detail.
/// This is the shape an Anthropic-API egress produces.
fn canonical_response_with_anthropic_reasoning() -> ChatResponse {
    ChatResponse {
        id: "msg_roundtrip".into(),
        model: "claude-sonnet-4-5".into(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("the answer is 42".into()),
                reasoning: None,
                reasoning_details: vec![
                    ReasoningDetail {
                        kind: ReasoningDetailKind::Text,
                        id: None,
                        format: Some(ANTHROPIC_FORMAT.into()),
                        index: Some(0),
                        payload: json!({"text": "let me think", "signature": "sig-abc"}),
                    },
                    ReasoningDetail {
                        kind: ReasoningDetailKind::Encrypted,
                        id: None,
                        format: Some(ANTHROPIC_FORMAT.into()),
                        index: Some(1),
                        payload: json!({"data": "opaque-redacted-blob"}),
                    },
                ],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Usage::default()
        }),
        ..ChatResponse::default()
    }
}

/// Render the canonical response through the Anthropic ingress -- the
/// literal bytes a claude-code style client receives.
fn capture_from_anthropic_ingress() -> Value {
    serde_json::from_slice(
        &AnthropicIngress
            .render_response(canonical_response_with_anthropic_reasoning())
            .expect("anthropic ingress render_response must succeed"),
    )
    .expect("rendered anthropic body is valid JSON")
}

/// Capture the Anthropic-ingress body and lift its reasoning blocks out
/// of `content` verbatim -- no key rewriting, which is the whole point.
/// Returns the captured body alongside the lifted blocks.
fn captured_anthropic_reasoning_details() -> (Value, Vec<Value>) {
    let captured = capture_from_anthropic_ingress();
    let details: Vec<Value> = captured
        .get("content")
        .and_then(|v| v.as_array())
        .expect("content array")
        .iter()
        .filter(|b| {
            matches!(
                b.get("type").and_then(Value::as_str),
                Some("thinking" | "redacted_thinking")
            )
        })
        .cloned()
        .collect();
    (captured, details)
}

/// Replay a captured assistant turn into the OpenAI ingress as a
/// follow-up request, exactly as a client echoing history would.
fn replay_to_openai_ingress(assistant_turn: Value) -> routectl_core::ChatRequest {
    let body = json!({
        "model": "claude-sonnet-4-5",
        "messages": [
            {"role": "user", "content": "what is the answer?"},
            assistant_turn,
            {"role": "user", "content": "are you sure?"}
        ]
    });
    OpenAiIngress
        .parse_request(
            &http::HeaderMap::new(),
            &serde_json::to_vec(&body).expect("request body serializes"),
        )
        .expect("openai ingress must accept a turn captured from the anthropic ingress")
}

/// The Anthropic ingress renders reasoning as content BLOCKS. A client
/// echoing that turn back with the blocks intact must be accepted, and
/// the blocks must survive as typed content parts.
#[test]
fn anthropic_capture_replays_to_openai_ingress_as_content_blocks() {
    // Arrange: capture the real anthropic-ingress body.
    let captured = capture_from_anthropic_ingress();
    let content = captured
        .get("content")
        .and_then(|v| v.as_array())
        .expect("anthropic body renders a content array")
        .clone();
    assert!(
        content
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("redacted_thinking")),
        "premise: the anthropic ingress emits a redacted_thinking block; got: {captured}"
    );

    // Act: echo the assistant turn back, content array UNMODIFIED.
    let req = replay_to_openai_ingress(json!({
        "role": "assistant",
        "content": content,
    }));

    // Assert: both reasoning blocks survive as typed parts.
    let assistant = req
        .messages
        .iter()
        .find(|m| matches!(m.role, Role::Assistant))
        .expect("assistant turn present");
    let MessageContent::Parts(parts) = &assistant.content else {
        panic!("expected typed parts, got: {:?}", assistant.content);
    };
    let tags: Vec<&str> = parts
        .iter()
        .map(routectl_core::ContentPart::type_tag)
        .collect();
    assert!(
        tags.contains(&"redacted_thinking"),
        "redacted_thinking block must survive the replay; got tags: {tags:?}"
    );
    assert!(
        tags.contains(&"thinking"),
        "thinking block must survive the replay; got tags: {tags:?}"
    );
}

/// The same capture, echoed back the way a client that normalizes to the
/// OpenAI message shape does it: reasoning hoisted out of `content` into
/// a `reasoning_details` array, still spelled in Anthropic vocabulary.
/// This is the shape that produced the validation_error.
#[test]
fn anthropic_reasoning_vocabulary_replays_to_openai_ingress_as_reasoning_details() {
    // Arrange: lift the captured blocks into a reasoning_details array
    // verbatim -- no key rewriting, which is the whole point.
    let (_captured, details) = captured_anthropic_reasoning_details();
    assert_eq!(details.len(), 2, "premise: two reasoning blocks captured");

    // Act
    let req = replay_to_openai_ingress(json!({
        "role": "assistant",
        "content": "the answer is 42",
        "reasoning_details": details,
    }));

    // Assert: both Anthropic spellings mapped onto canonical kinds.
    let assistant = req
        .messages
        .iter()
        .find(|m| matches!(m.role, Role::Assistant))
        .expect("assistant turn present");
    assert_eq!(assistant.reasoning_details.len(), 2);

    let encrypted = assistant
        .reasoning_details
        .iter()
        .find(|d| matches!(d.kind, ReasoningDetailKind::Encrypted))
        .expect("redacted_thinking must map to Encrypted");
    assert_eq!(
        encrypted.payload.get("data").and_then(Value::as_str),
        Some("opaque-redacted-blob"),
        "the opaque blob must survive byte-verbatim"
    );

    let text = assistant
        .reasoning_details
        .iter()
        .find(|d| matches!(d.kind, ReasoningDetailKind::Text))
        .expect("thinking must map to Text");
    assert_eq!(
        text.payload.get("text").and_then(Value::as_str),
        Some("let me think"),
        "the Anthropic `thinking` payload key must land on canonical `text`"
    );
    assert_eq!(
        text.payload.get("signature").and_then(Value::as_str),
        Some("sig-abc"),
        "the signature must survive -- Anthropic 400s on replay without it"
    );
}

/// Closing the loop: a detail that entered via the Anthropic spelling
/// must re-render to the Anthropic wire spelling, so the round trip is
/// stable across an arbitrary number of turns rather than lossy after
/// the first.
#[test]
fn reasoning_details_replayed_from_anthropic_vocabulary_re_render_to_anthropic_blocks() {
    // Arrange: capture -> replay -> canonical request.
    let (captured, details) = captured_anthropic_reasoning_details();
    let req = replay_to_openai_ingress(json!({
        "role": "assistant",
        "content": "the answer is 42",
        "reasoning_details": details,
    }));
    let assistant = req
        .messages
        .iter()
        .find(|m| matches!(m.role, Role::Assistant))
        .expect("assistant turn present")
        .clone();

    // Act: render that canonical turn back out the Anthropic ingress.
    let mut resp = canonical_response_with_anthropic_reasoning();
    resp.choices[0].message = assistant;
    let rendered: Value = serde_json::from_slice(
        &AnthropicIngress
            .render_response(resp)
            .expect("render_response must succeed"),
    )
    .expect("rendered body is valid JSON");

    // Assert: the second render matches the first -- the vocabulary
    // survived a full lap through the OpenAI ingress.
    let block_types = |body: &Value| -> Vec<String> {
        body.get("content")
            .and_then(|v| v.as_array())
            .expect("content array")
            .iter()
            .filter_map(|b| b.get("type").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(
        block_types(&rendered),
        block_types(&captured),
        "re-render must reproduce the captured block vocabulary; got: {rendered}"
    );

    // The thinking block must re-render with its CONTENT, not as an
    // empty shell: the renderer reads canonical `text`, so a payload key
    // left in Anthropic spelling silently renders empty thinking.
    let thinking = rendered
        .get("content")
        .and_then(|v| v.as_array())
        .expect("content array")
        .iter()
        .find(|b| b.get("type").and_then(Value::as_str) == Some("thinking"))
        .expect("thinking re-rendered");
    assert_eq!(
        thinking.get("thinking").and_then(Value::as_str),
        Some("let me think"),
        "re-rendered thinking must carry its text, not an empty string"
    );
    assert_eq!(
        thinking.get("signature").and_then(Value::as_str),
        Some("sig-abc"),
        "the signature must survive the full lap"
    );

    let redacted = rendered
        .get("content")
        .and_then(|v| v.as_array())
        .expect("content array")
        .iter()
        .find(|b| b.get("type").and_then(Value::as_str) == Some("redacted_thinking"))
        .expect("redacted_thinking re-rendered");
    assert_eq!(
        redacted.get("data").and_then(Value::as_str),
        Some("opaque-redacted-blob"),
        "an Anthropic-sourced blob re-emits byte-verbatim, never re-wrapped"
    );
}

/// The other egress direction: a detail that entered via the Anthropic
/// spelling must serialize outward in CANONICAL vocabulary, so an
/// OpenAI-dialect client sees `reasoning.encrypted` rather than a
/// leaked Anthropic block name. The aliases widen input only.
#[test]
fn anthropic_sourced_details_render_canonical_vocabulary_on_openai_egress() {
    // Arrange: canonical response whose details came in Anthropic-spelled.
    let (_captured, details) = captured_anthropic_reasoning_details();
    let req = replay_to_openai_ingress(json!({
        "role": "assistant",
        "content": "the answer is 42",
        "reasoning_details": details,
    }));
    let assistant = req
        .messages
        .iter()
        .find(|m| matches!(m.role, Role::Assistant))
        .expect("assistant turn present")
        .clone();
    let mut resp = canonical_response_with_anthropic_reasoning();
    resp.choices[0].message = assistant;

    // Act: render out the OpenAI ingress.
    let rendered: Value = serde_json::from_slice(
        &OpenAiIngress
            .render_response(resp)
            .expect("openai ingress render_response must succeed"),
    )
    .expect("rendered body is valid JSON");

    // Assert: canonical `reasoning.*` spellings on the wire.
    let kinds: Vec<&str> = rendered["choices"][0]["message"]["reasoning_details"]
        .as_array()
        .expect("reasoning_details rendered")
        .iter()
        .filter_map(|d| d.get("type").and_then(Value::as_str))
        .collect();
    assert!(
        kinds.contains(&"reasoning.encrypted"),
        "openai egress must emit the canonical encrypted spelling; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"reasoning.text"),
        "openai egress must emit the canonical text spelling; got: {kinds:?}"
    );
    assert!(
        !kinds
            .iter()
            .any(|k| *k == "redacted_thinking" || *k == "thinking"),
        "the inbound aliases must never leak back onto the wire; got: {kinds:?}"
    );
}
