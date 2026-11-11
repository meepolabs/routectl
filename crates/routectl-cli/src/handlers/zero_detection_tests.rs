//! Zero-detection harness: every committed driver fixture, plus the
//! representative request shapes of each FIDELITY lane and of Bedrock
//! Converse, through the real ingress and a real egress against a mock
//! upstream, with every request checked on each leg its lane compares. Any
//! finding is either an intended routectl edit, which must be declared at
//! the site that makes it, or a known loss listed in [`KNOWN_UNDECLARED`] by
//! lane, leg and path with its reason.
//!
//! The Responses lane through Bedrock Mantle is walked with a real Mantle
//! egress aimed at the mock.
//!
//! Bedrock Converse is a TRANSLATION lane: its request leg is checked for
//! client extras the Converse body does not carry, and its response leg is
//! not compared at all, since the upstream answers in a dialect no ingress
//! speaks. Its cases count the request leg only.
//!
//! The harness prints what it walked per lane and leg, and fails when it
//! walks nothing: a green run over an empty corpus proves nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use routectl_core::wire_check::Leg;
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
};
use routectl_router::wire_check_state::{CheckOutcome, WireCheckSnapshot};
use routectl_router::{
    AliasValue, CURRENT_CONFIG_VERSION, Config, ModelEntry, ProviderEntry, ResolvedModel,
    RetryPolicy, Router, parse_config,
};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;

const ALIAS: &str = "zero-detection";
const NICKNAME: &str = "m";
const PROVIDER: &str = "p";
const UPSTREAM: &str = "upstream";

/// A finding the harness tolerates, keyed by lane, leg and the path the
/// check names, with why it is not declared.
struct KnownUndeclared {
    lane: &'static str,
    leg: Leg,
    path: &'static str,
    reason: &'static str,
}

const ANTHROPIC: &str = "anthropic->anthropic-api";
const CHAT: &str = "chat->openai-compat";
const BEDROCK_INVOKE: &str = "anthropic->bedrock-invoke";
const BEDROCK_CONVERSE: &str = "anthropic->bedrock-converse";
const RESPONSES_MANTLE: &str = "responses->openai-responses-mantle";
const ANTHROPIC_TO_RESPONSES: &str = "anthropic->openai-responses";
const CHAT_TO_RESPONSES: &str = "chat->openai-responses";

/// Findings the check correctly reports that no declaration explains.
const KNOWN_UNDECLARED: &[KnownUndeclared] = &[KnownUndeclared {
    lane: ANTHROPIC,
    leg: Leg::Response,
    path: "/usage/output_tokens_details",
    reason: "real loss: the closing message_delta render drops output_tokens_details",
}];

/// The client dialect a case speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ingress {
    Anthropic,
    Chat,
    Responses,
}

impl Ingress {
    const fn lane(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic->anthropic-api",
            Self::Chat => "chat->openai-compat",
            Self::Responses => "responses->openai-responses",
        }
    }
}

/// The Bedrock API a case's provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BedrockShape {
    Invoke,
    Converse,
}

/// What carries a case's request upstream: a provider entry the router
/// builds aimed at the mock, or a real Bedrock provider whose transport is
/// aimed at the mock (a Bedrock entry derives its host from its region).
#[derive(Clone, Copy)]
enum Egress {
    Entry(fn(&str) -> ProviderEntry),
    Bedrock {
        shape: BedrockShape,
        model_id: &'static str,
    },
    /// A real openai-responses provider on the Bedrock Mantle surface.
    ResponsesMantle,
    /// A provider entry speaking another dialect than the client, recorded
    /// on `lane`: a TRANSLATION lane, so only its request leg is compared.
    Translation {
        provider: fn(&str) -> ProviderEntry,
        lane: &'static str,
    },
}

/// One request through the harness: what the client sends, how the
/// upstream answers, and the provider config the lane uses.
struct Case {
    name: String,
    ingress: Ingress,
    headers: HeaderMap,
    body: Value,
    upstream: ResponseTemplate,
    egress: Egress,
    model: fn() -> ModelEntry,
    /// A member the router's tap removes from the deposited egress body, so
    /// the check sees a body that lost it; `None` for real traffic.
    tap_drop: Option<&'static str>,
}

impl Case {
    /// The lane the check records this case's findings on.
    const fn lane(&self) -> &'static str {
        match self.egress {
            Egress::Entry(_) => self.ingress.lane(),
            Egress::ResponsesMantle => RESPONSES_MANTLE,
            Egress::Translation { lane, .. } => lane,
            Egress::Bedrock {
                shape: BedrockShape::Invoke,
                ..
            } => BEDROCK_INVOKE,
            Egress::Bedrock {
                shape: BedrockShape::Converse,
                ..
            } => BEDROCK_CONVERSE,
        }
    }

    /// The legs the check compares on this case's lane.
    const fn legs(&self) -> &'static [Leg] {
        match self.egress {
            Egress::Bedrock {
                shape: BedrockShape::Converse,
                ..
            }
            | Egress::Translation { .. } => &[Leg::Request],
            Egress::Entry(_)
            | Egress::ResponsesMantle
            | Egress::Bedrock {
                shape: BedrockShape::Invoke,
                ..
            } => &[Leg::Request, Leg::Response],
        }
    }
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers
}

fn anthropic_provider(base: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")).with_base_url(base)
}

fn chat_provider(base: &str) -> ProviderEntry {
    ProviderEntry::openai_compat(base, crate::test_secret::file_ref("k"))
}

fn responses_provider(base: &str) -> ProviderEntry {
    ProviderEntry::openai_responses(crate::test_secret::file_ref("k"))
        .with_openai_responses_auth_kind(routectl_providers::openai_responses::AuthKind::ApiKey)
        .with_openai_responses_base_url(base)
}

fn plain_model() -> ModelEntry {
    ModelEntry::new(PROVIDER, UPSTREAM)
}

// ---------------------------------------------------------------------------
// Upstream answers
// ---------------------------------------------------------------------------

fn sse(frames: &[(Option<&str>, Value)], done: bool) -> String {
    let mut wire = String::new();
    for (event, data) in frames {
        if let Some(event) = event {
            wire.push_str(&format!("event: {event}\n"));
        }
        wire.push_str(&format!("data: {data}\n\n"));
    }
    if done {
        wire.push_str("data: [DONE]\n\n");
    }
    wire
}

fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn anthropic_complete() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 1}
    }))
}

fn anthropic_stream() -> ResponseTemplate {
    sse_response(sse(
        &[
            (
                Some("message_start"),
                json!({"type": "message_start", "message": {
                    "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": 3, "output_tokens": 0}}}),
            ),
            (
                Some("content_block_start"),
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "text", "text": ""}}),
            ),
            (
                Some("content_block_delta"),
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": "ok"}}),
            ),
            (
                Some("content_block_stop"),
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                Some("message_delta"),
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                       "usage": {"input_tokens": 3, "output_tokens": 1}}),
            ),
            (Some("message_stop"), json!({"type": "message_stop"})),
        ],
        false,
    ))
}

fn anthropic_complete_without_stop_sequence() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    }))
}

fn chat_complete() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": UPSTREAM, "system_fingerprint": null,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    }))
}

fn chat_complete_with_tool_call() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": UPSTREAM, "system_fingerprint": null,
        "choices": [{"index": 0, "finish_reason": "tool_calls",
                     "message": {"role": "assistant", "content": null,
                                 "tool_calls": [{"id": "call_1", "type": "function",
                                     "function": {"name": "search",
                                                  "arguments": "{\"q\":\"rust\"}"}}]}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    }))
}

fn chat_stream() -> ResponseTemplate {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "c1", "object": "chat.completion.chunk",
               "created": 1_700_000_000_i64, "model": UPSTREAM,
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    sse_response(sse(
        &[
            (
                None,
                chunk(json!({"role": "assistant", "content": "ok"}), Value::Null),
            ),
            (None, chunk(json!({}), json!("stop"))),
        ],
        true,
    ))
}

fn chat_stream_with_usage(fingerprint: Option<&str>) -> ResponseTemplate {
    let mut first = json!({"id": "c1", "object": "chat.completion.chunk",
        "created": 1_700_000_000_i64, "model": UPSTREAM,
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"},
                     "finish_reason": null}]});
    if let Some(fp) = fingerprint {
        first["system_fingerprint"] = json!(fp);
    }
    sse_response(sse(
        &[
            (None, first),
            (
                None,
                json!({"id": "c1", "object": "chat.completion.chunk",
                       "created": 1_700_000_000_i64, "model": UPSTREAM,
                       "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ),
            (
                None,
                json!({"id": "c1", "object": "chat.completion.chunk",
                       "created": 1_700_000_000_i64, "model": UPSTREAM, "choices": [],
                       "usage": {"prompt_tokens": 8, "completion_tokens": 1, "total_tokens": 9,
                                 "prompt_tokens_details": {"cached_tokens": 4}}}),
            ),
        ],
        true,
    ))
}

fn chat_complete_with_reasoning_content() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": UPSTREAM, "system_fingerprint": null,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "42",
                                 "reasoning_content": "six times seven"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    }))
}

fn chat_complete_with_cached_tokens() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": UPSTREAM, "system_fingerprint": null,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 8, "completion_tokens": 1, "total_tokens": 9,
                  "prompt_tokens_details": {"cached_tokens": 4}}
    }))
}

fn chat_stream_with_reasoning_content() -> ResponseTemplate {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "c1", "object": "chat.completion.chunk",
               "created": 1_700_000_000_i64, "model": UPSTREAM,
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    sse_response(sse(
        &[
            (
                None,
                chunk(
                    json!({"role": "assistant", "reasoning_content": "six times seven"}),
                    Value::Null,
                ),
            ),
            (None, chunk(json!({"content": "42"}), Value::Null)),
            (None, chunk(json!({}), json!("stop"))),
        ],
        true,
    ))
}

fn deepseek_model() -> ModelEntry {
    ModelEntry::new(PROVIDER, UPSTREAM)
        .with_reasoning_dialect(routectl_router::ReasoningDialect::Deepseek)
}

fn anthropic_stream_with_usage_detail() -> ResponseTemplate {
    sse_response(sse(
        &[
            (
                Some("message_start"),
                json!({"type": "message_start", "message": {
                    "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": 3, "output_tokens": 0,
                              "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                              "cache_creation": {"ephemeral_5m_input_tokens": 0,
                                                 "ephemeral_1h_input_tokens": 0}}}}),
            ),
            (
                Some("content_block_start"),
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "text", "text": ""}}),
            ),
            (
                Some("content_block_delta"),
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": "ok"}}),
            ),
            (
                Some("content_block_stop"),
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                Some("message_delta"),
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                       "usage": {"output_tokens": 1,
                                 "output_tokens_details": {"thinking_tokens": 0},
                                 "server_tool_use": {"web_search_requests": 1}}}),
            ),
            (Some("message_stop"), json!({"type": "message_stop"})),
        ],
        false,
    ))
}

/// A real upstream always stamps `created_at`; the complete path mints one
/// for a zero, so a zero fixture would compare a minted value.
const RESPONSES_CREATED_AT: i64 = 1_700_000_000;

fn responses_document() -> Value {
    json!({
        "id": "resp_1", "object": "response", "created_at": RESPONSES_CREATED_AT,
        "status": "completed", "model": UPSTREAM,
        "output": [{"type": "message", "id": "m1", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "ok", "annotations": []}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    })
}

/// The Responses egress reads every answer as a stream.
fn responses_stream() -> ResponseTemplate {
    responses_stream_of(responses_document())
}

/// A text answer streamed event by event, ending in `done`.
fn responses_stream_of(done: Value) -> ResponseTemplate {
    let mut created = done.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    sse_response(sse(
        &[
            (
                Some("response.created"),
                json!({"type": "response.created", "response": created}),
            ),
            (
                Some("response.output_item.added"),
                json!({"type": "response.output_item.added", "output_index": 0,
                       "item": {"type": "message", "id": "m1", "role": "assistant",
                                "status": "in_progress", "content": []}}),
            ),
            (
                Some("response.content_part.added"),
                json!({"type": "response.content_part.added", "output_index": 0,
                       "content_index": 0,
                       "part": {"type": "output_text", "text": "", "annotations": []}}),
            ),
            (
                Some("response.output_text.delta"),
                json!({"type": "response.output_text.delta", "output_index": 0,
                       "content_index": 0, "delta": "ok"}),
            ),
            (
                Some("response.completed"),
                json!({"type": "response.completed", "response": done}),
            ),
        ],
        false,
    ))
}

// ---------------------------------------------------------------------------
// The committed driver corpus
// ---------------------------------------------------------------------------

fn driver_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/driver")
}

/// Headers the fixture recorded, minus the ones that name the capture's
/// own transport or credential.
fn fixture_headers(dir: &Path) -> HeaderMap {
    let mut headers = json_headers();
    let raw = std::fs::read_to_string(dir.join("ingress_request.headers.json"))
        .expect("fixture headers readable");
    let pairs: Vec<(String, String)> = serde_json::from_str(&raw).expect("fixture headers json");
    for (name, value) in pairs {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host" | "content-length" | "authorization" | "x-api-key" | "connection"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(lower.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// Every committed driver fixture as a case, its body re-aimed at the
/// harness alias. Each fixture's lane directory must be one this harness
/// knows how to serve.
fn driver_cases() -> Vec<Case> {
    let root = driver_root();
    let mut cases = Vec::new();
    let mut lanes: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("driver corpus root readable")
        .map(|e| e.expect("dir entry").path())
        .collect();
    lanes.sort();
    for lane in lanes {
        let lane_name = lane
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            lane_name, "anthropic-api",
            "a driver lane the harness does not serve yet: {lane_name}"
        );
        let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&lane)
            .expect("lane dir readable")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.join("ingress_request.json").is_file())
            .collect();
        fixtures.sort();
        for dir in fixtures {
            let raw = std::fs::read(dir.join("ingress_request.json")).expect("fixture body");
            let mut body: Value = serde_json::from_slice(&raw).expect("fixture body json");
            body["model"] = json!(ALIAS);
            let streamed = body.get("stream").and_then(Value::as_bool) == Some(true);
            cases.push(Case {
                name: format!(
                    "driver/{lane_name}/{}",
                    dir.file_name().and_then(|n| n.to_str()).unwrap_or_default()
                ),
                ingress: Ingress::Anthropic,
                headers: fixture_headers(&dir),
                body,
                upstream: if streamed {
                    anthropic_stream()
                } else {
                    anthropic_complete()
                },
                egress: Egress::Entry(anthropic_provider),
                model: plain_model,
                tap_drop: None,
            });
        }
    }
    cases
}

// ---------------------------------------------------------------------------
// Representative traffic for the other FIDELITY lanes
// ---------------------------------------------------------------------------

fn case(
    name: &str,
    ingress: Ingress,
    body: Value,
    upstream: ResponseTemplate,
    provider: fn(&str) -> ProviderEntry,
) -> Case {
    Case {
        name: name.to_string(),
        ingress,
        headers: json_headers(),
        body,
        upstream,
        egress: Egress::Entry(provider),
        model: plain_model,
        tap_drop: None,
    }
}

fn chat_cases() -> Vec<Case> {
    let hi = json!([{"role": "user", "content": "hi"}]);
    vec![
        case(
            "chat/plain",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": hi}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/stream",
            Ingress::Chat,
            json!({"model": ALIAS, "stream": true, "messages": hi}),
            chat_stream(),
            chat_provider,
        ),
        case(
            "chat/sampling",
            Ingress::Chat,
            json!({"model": ALIAS, "temperature": 0.3, "top_p": 0.9, "max_tokens": 32,
                   "stop": ["END"], "messages": hi}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/system-first",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/system-mid-conversation",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "system", "content": "now be brief"},
                {"role": "user", "content": "again"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/two-system-messages",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
                {"role": "system", "content": "be kind"},
                {"role": "user", "content": "again"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/system-with-image",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [
                    {"type": "text", "text": "match this"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]},
                {"role": "user", "content": "again"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/developer-with-unknown-part",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "developer", "content": [
                    {"type": "text", "text": "match this"},
                    {"type": "future_block", "payload": {"k": 1}}]},
                {"role": "user", "content": "hi"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/kept-developer-before-lifted-system",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "developer", "content": [
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]},
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/lifted-system-before-kept-developer",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "developer", "content": [
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]},
                {"role": "user", "content": "hi"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/kept-developer-before-two-lifted-systems",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "developer", "content": [{"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]},
                {"role": "system", "content": "a"},
                {"role": "system", "content": "b"},
                {"role": "user", "content": "hi"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/kept-developer-between-lifted-systems",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "system", "content": "a"},
                {"role": "developer", "content": [{"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]},
                {"role": "system", "content": "b"},
                {"role": "user", "content": "hi"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/kept-system-before-lifted-system",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "system", "content": [{"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]},
                {"role": "system", "content": "a"},
                {"role": "user", "content": "hi"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "responses/kept-system-before-lifted-system",
            Ingress::Responses,
            json!({"model": ALIAS, "input": [
                {"type": "message", "role": "system", "content": [{"type": "input_image", "image_url": "https://example.com/a.png"}]},
                {"type": "message", "role": "system", "content": "a"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/kept-system-between-lifted-systems",
            Ingress::Responses,
            json!({"model": ALIAS, "input": [
                {"type": "message", "role": "system", "content": "a"},
                {"type": "message", "role": "system", "content": [{"type": "input_image", "image_url": "https://example.com/a.png"}]},
                {"type": "message", "role": "system", "content": "b"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "chat/one-turn-before-lifted-system",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "user", "content": "x"},
                {"role": "system", "content": "a"},
                {"role": "user", "content": "hi"}]}),
            chat_complete(),
            chat_provider,
        ),
        case(
            "chat/stream-usage-cached",
            Ingress::Chat,
            json!({"model": ALIAS, "stream": true, "messages": hi}),
            chat_stream_with_usage(None),
            chat_provider,
        ),
        case(
            "chat/stream-system-fingerprint",
            Ingress::Chat,
            json!({"model": ALIAS, "stream": true, "messages": hi}),
            chat_stream_with_usage(Some("fp_1")),
            chat_provider,
        ),
        Case {
            model: deepseek_model,
            ..case(
                "chat/deepseek-reasoning-content",
                Ingress::Chat,
                json!({"model": ALIAS, "messages": hi}),
                chat_complete_with_reasoning_content(),
                chat_provider,
            )
        },
        case(
            "chat/complete-cached-tokens",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": hi}),
            chat_complete_with_cached_tokens(),
            chat_provider,
        ),
        Case {
            model: deepseek_model,
            ..case(
                "chat/deepseek-stream-reasoning-content",
                Ingress::Chat,
                json!({"model": ALIAS, "stream": true, "messages": hi}),
                chat_stream_with_reasoning_content(),
                chat_provider,
            )
        },
        case(
            "chat/tools",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": hi,
                   "tools": [{"type": "function", "function": {
                       "name": "search", "description": "find",
                       "parameters": {"type": "object",
                                      "properties": {"q": {"type": "string"}}}}}],
                   "tool_choice": "auto"}),
            chat_complete_with_tool_call(),
            chat_provider,
        ),
        case(
            "chat/tool-round-trip",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "no results"}],
                   "tools": [{"type": "function", "function": {
                       "name": "search",
                       "parameters": {"type": "object", "properties": {}}}}]}),
            chat_complete(),
            chat_provider,
        ),
    ]
}

fn responses_cases() -> Vec<Case> {
    let input = json!([{"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}]}]);
    vec![
        case(
            "responses/plain",
            Ingress::Responses,
            json!({"model": ALIAS, "input": input}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/unmodeled-output-item",
            Ingress::Responses,
            json!({"model": ALIAS, "input": input}),
            responses_stream_with_unmodeled_item(),
            responses_provider,
        ),
        case(
            "responses/unmodeled-members",
            Ingress::Responses,
            json!({"model": ALIAS, "input": input}),
            responses_stream_with_unmodeled_members(),
            responses_provider,
        ),
        case(
            "responses/stream",
            Ingress::Responses,
            json!({"model": ALIAS, "stream": true, "input": input}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/unmodeled-usage-stream",
            Ingress::Responses,
            json!({"model": ALIAS, "stream": true, "input": input}),
            responses_stream_with_unmodeled_usage(),
            responses_provider,
        ),
        case(
            "responses/sampling",
            Ingress::Responses,
            json!({"model": ALIAS, "temperature": 0.3, "top_p": 0.9,
                   "max_output_tokens": 32, "input": input}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/instructions",
            Ingress::Responses,
            json!({"model": ALIAS, "instructions": "be brief", "input": input}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/tools",
            Ingress::Responses,
            json!({"model": ALIAS, "input": input,
                   "tools": [{"type": "function", "name": "search", "description": "find",
                              "parameters": {"type": "object",
                                             "properties": {"q": {"type": "string"}}}}],
                   "tool_choice": {"type": "function", "name": "search"}}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/system-first",
            Ingress::Responses,
            json!({"model": ALIAS, "input": [
                {"type": "message", "role": "system", "content": "be brief"},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]}]}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/developer-mid-conversation",
            Ingress::Responses,
            json!({"model": ALIAS, "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "developer", "content": "now be brief"},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "again"}]}]}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/instructions-and-system",
            Ingress::Responses,
            json!({"model": ALIAS, "instructions": "be brief", "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "system", "content": "be kind"}]}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/system-with-image",
            Ingress::Responses,
            json!({"model": ALIAS, "input": [
                {"type": "message", "role": "system", "content": [
                    {"type": "input_text", "text": "match this"},
                    {"type": "input_image", "image_url": "https://example.com/a.png"}]},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]}]}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/lifted-system-before-kept-developer",
            Ingress::Responses,
            json!({"model": ALIAS, "input": [
                {"type": "message", "role": "system", "content": "be brief"},
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_image", "image_url": "https://example.com/a.png"}]},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]}]}),
            responses_stream(),
            responses_provider,
        ),
        case(
            "responses/kept-developer-before-lifted-system",
            Ingress::Responses,
            json!({"model": ALIAS, "input": [
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_image", "image_url": "https://example.com/a.png"}]},
                {"type": "message", "role": "system", "content": "be brief"},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]}]}),
            responses_stream(),
            responses_provider,
        ),
    ]
}

/// A Responses answer carrying a top-level member and usage members no
/// typed field models, beside the typed counts read out of the same
/// details objects.
fn responses_stream_with_unmodeled_members() -> ResponseTemplate {
    let mut done = responses_document_with_unmodeled_usage();
    done["service_tier"] = json!("default");
    responses_stream_of(done)
}

/// The same answer for a streaming client, whose terminal event does not
/// carry top-level members yet: usage members only.
fn responses_stream_with_unmodeled_usage() -> ResponseTemplate {
    responses_stream_of(responses_document_with_unmodeled_usage())
}

fn responses_document_with_unmodeled_usage() -> Value {
    let mut done = responses_document();
    done["usage"] = json!({
        "input_tokens": 3, "output_tokens": 1, "total_tokens": 4,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0, "x": 1}
    });
    done
}

/// Responses traffic through the Bedrock Mantle surface, walked by a real
/// Mantle egress.
fn responses_mantle_cases() -> Vec<Case> {
    vec![Case {
        egress: Egress::ResponsesMantle,
        ..case(
            "responses-mantle/plain",
            Ingress::Responses,
            json!({"model": ALIAS, "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "hi"}]}]}),
            responses_stream(),
            responses_provider,
        )
    }]
}

/// A Responses answer carrying an output item no other client dialect can
/// carry, after the message: a Responses client gets it back as a top-level
/// item, and the provider removes it for any other client.
fn responses_stream_with_unmodeled_item() -> ResponseTemplate {
    let mut done = responses_document();
    done["output"].as_array_mut().expect("output array").push(
        json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1",
                     "name": "exec", "input": "ls", "status": "completed"}),
    );
    sse_response(sse(
        &[(
            Some("response.completed"),
            json!({"type": "response.completed", "response": done}),
        )],
        false,
    ))
}

/// Anthropic and Chat clients served by openai-responses, whose upstream
/// answers with an item neither dialect can carry.
fn cross_dialect_responses_cases() -> Vec<Case> {
    let hi = json!([{"role": "user", "content": "hi"}]);
    vec![
        Case {
            egress: Egress::Translation {
                provider: responses_provider,
                lane: ANTHROPIC_TO_RESPONSES,
            },
            ..case(
                "anthropic-to-responses/unmodeled-output-item",
                Ingress::Anthropic,
                json!({"model": ALIAS, "max_tokens": 16, "messages": hi}),
                responses_stream_with_unmodeled_item(),
                responses_provider,
            )
        },
        Case {
            egress: Egress::Translation {
                provider: responses_provider,
                lane: CHAT_TO_RESPONSES,
            },
            ..case(
                "chat-to-responses/unmodeled-output-item",
                Ingress::Chat,
                json!({"model": ALIAS, "messages": hi}),
                responses_stream_with_unmodeled_item(),
                responses_provider,
            )
        },
    ]
}

fn anthropic_cases() -> Vec<Case> {
    let hi = json!([{"role": "user", "content": "hi"}]);
    vec![
        case(
            "anthropic/plain",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi}),
            anthropic_complete(),
            anthropic_provider,
        ),
        case(
            "anthropic/stream",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16, "stream": true, "messages": hi}),
            anthropic_stream(),
            anthropic_provider,
        ),
        case(
            "anthropic/stream-usage-detail",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16, "stream": true, "messages": hi}),
            anthropic_stream_with_usage_detail(),
            anthropic_provider,
        ),
        case(
            "anthropic/no-upstream-stop-sequence",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi}),
            anthropic_complete_without_stop_sequence(),
            anthropic_provider,
        ),
        case(
            "anthropic/max-tokens-omitted",
            Ingress::Anthropic,
            json!({"model": ALIAS, "messages": hi}),
            anthropic_complete(),
            anthropic_provider,
        ),
        case(
            "anthropic/legacy-output-format",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi,
                   "output_format": {"type": "json_schema", "schema": {
                       "type": "object", "properties": {"x": {"type": "integer"}}}}}),
            anthropic_complete(),
            anthropic_provider,
        ),
        case(
            "anthropic/output-config-format",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi,
                   "output_config": {"format": {"type": "json_schema", "schema": {
                       "type": "object", "properties": {"x": {"type": "integer"}}}}}}),
            anthropic_complete(),
            anthropic_provider,
        ),
        case(
            "anthropic/empty-system-turn",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16,
                   "system": [{"type": "text", "text": "you are helpful"}],
                   "messages": [
                       {"role": "user", "content": "earlier"},
                       {"role": "system", "content": ""},
                       {"role": "assistant", "content": "ok"},
                       {"role": "user", "content": "continue"}]}),
            anthropic_complete(),
            anthropic_provider,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Bedrock: Invoke on both legs, Converse on the request leg
// ---------------------------------------------------------------------------

fn bedrock_case(name: &str, shape: BedrockShape, body: Value, upstream: ResponseTemplate) -> Case {
    Case {
        egress: Egress::Bedrock {
            shape,
            model_id: BEDROCK_MODEL_ID,
        },
        ..case(name, Ingress::Anthropic, body, upstream, anthropic_provider)
    }
}

fn invoke_stream() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        super::response_wire::trace_tests::invoke_stream(),
        "application/vnd.amazon.eventstream",
    )
}

fn converse_complete() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
        "stopReason": "end_turn",
        "usage": {"inputTokens": 3, "outputTokens": 1, "totalTokens": 4},
    }))
}

fn bedrock_invoke_cases() -> Vec<Case> {
    let hi = json!([{"role": "user", "content": "hi"}]);
    let invoke = |name, body| bedrock_case(name, BedrockShape::Invoke, body, anthropic_complete());
    vec![
        invoke(
            "bedrock-invoke/plain",
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi}),
        ),
        Case {
            upstream: invoke_stream(),
            ..invoke(
                "bedrock-invoke/stream",
                json!({"model": ALIAS, "max_tokens": 16, "stream": true, "messages": hi}),
            )
        },
        invoke(
            "bedrock-invoke/sampling",
            json!({"model": ALIAS, "max_tokens": 16, "temperature": 0.3,
                   "stop_sequences": ["END"], "messages": hi}),
        ),
        invoke(
            "bedrock-invoke/thinking-effort",
            json!({"model": ALIAS, "max_tokens": 4096, "messages": hi,
                   "thinking": {"type": "adaptive"},
                   "output_config": {"effort": "high"}}),
        ),
        invoke(
            "bedrock-invoke/top-level-cache-marker",
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi,
                   "cache_control": {"type": "ephemeral"}}),
        ),
        invoke(
            "bedrock-invoke/client-metadata",
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi,
                   "metadata": {"user_id": "u-1"}}),
        ),
    ]
}

fn bedrock_converse_cases() -> Vec<Case> {
    let hi = json!([{"role": "user", "content": "hi"}]);
    let converse =
        |name, body| bedrock_case(name, BedrockShape::Converse, body, converse_complete());
    vec![
        converse(
            "bedrock-converse/plain",
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi}),
        ),
        converse(
            "bedrock-converse/sampling",
            json!({"model": ALIAS, "max_tokens": 16, "temperature": 0.3,
                   "stop_sequences": ["END"], "messages": hi}),
        ),
        converse(
            "bedrock-converse/thinking-effort",
            json!({"model": ALIAS, "max_tokens": 4096, "messages": hi,
                   "thinking": {"type": "adaptive"},
                   "output_config": {"effort": "high"}}),
        ),
        converse(
            "bedrock-converse/forward-compat-extra",
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi,
                   "context_management": {"edits": []}}),
        ),
        converse(
            "bedrock-converse/client-metadata",
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi,
                   "metadata": {"user_id": "u-1"}}),
        ),
        Case {
            egress: Egress::Bedrock {
                shape: BedrockShape::Converse,
                model_id: NON_CLAUDE_MODEL_ID,
            },
            ..converse(
                "bedrock-converse/non-claude-output-config",
                json!({"model": ALIAS, "max_tokens": 16, "messages": hi,
                       "output_config": {"effort": "high"}}),
            )
        },
    ]
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// Checks every request on each leg its lane compares.
async fn router_for(case: &Case, base: &str) -> Router {
    let provider = match case.egress {
        Egress::Entry(provider) | Egress::Translation { provider, .. } => provider,
        Egress::Bedrock { shape, model_id } => return bedrock_router(shape, model_id, base).await,
        Egress::ResponsesMantle => return responses_mantle_router(base),
    };
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut config = Config {
        providers: BTreeMap::from([(PROVIDER.to_string(), provider(base))]),
        models: BTreeMap::from([(NICKNAME.to_string(), (case.model)())]),
        aliases: BTreeMap::from([(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()))]),
        retry,
        ..Default::default()
    };
    check_every_request(&mut config);
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("build router")
}

const BEDROCK_REGION: &str = "us-east-1";
const BEDROCK_MODEL_ID: &str = "us.anthropic.claude-sonnet-4-5";
/// A model id outside the Anthropic family, whose Converse bag cannot carry
/// `output_config`.
const NON_CLAUDE_MODEL_ID: &str = "zz.zero-detection-test";

fn bedrock_key() -> String {
    ["zero", "detection", "key"].join("-")
}

/// A router whose one target is a real Bedrock provider of `shape` serving
/// `model_id`, its transport aimed at `origin`, checking every request.
async fn bedrock_router(shape: BedrockShape, model_id: &str, origin: &str) -> Router {
    let (api_shape, shape_token) = match shape {
        BedrockShape::Invoke => (BedrockApiShape::Invoke, "invoke"),
        BedrockShape::Converse => (BedrockApiShape::Converse, "converse"),
    };
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.{PROVIDER}]\n\
         kind = \"bedrock\"\n\
         region = \"{BEDROCK_REGION}\"\n\
         api_shape = \"{shape_token}\"\n\
         creds = {{ kind = \"bearer-key\", key_ref = \"{key_ref}\" }}\n\n\
         [models.{NICKNAME}]\n\
         provider = \"{PROVIDER}\"\n\
         upstream = \"{model_id}\"\n\n\
         [aliases]\n\
         {ALIAS} = \"{NICKNAME}\"\n\n\
         [retry]\n\
         max_attempts = 1\n",
        key_ref = crate::test_secret::file_ref(&bedrock_key()),
    );
    let mut config = parse_config(&text).expect("valid bedrock test config");
    check_every_request(&mut config);
    let creds = BedrockCreds::BearerKey { key: bedrock_key() };
    let cfg = BedrockConfig {
        id: format!("bedrock:{PROVIDER}"),
        region: BEDROCK_REGION.into(),
        model_id: model_id.into(),
        api_shape,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    };
    let resolved = auth::resolve(&creds, BEDROCK_REGION)
        .await
        .expect("bearer creds resolve");
    let provider = BedrockProvider::new(cfg, resolved)
        .expect("canonical region")
        .with_runtime_origin_for_tests(origin);
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        NICKNAME.to_string(),
        Arc::new(ResolvedModel::new(
            NICKNAME,
            PROVIDER,
            Arc::new(provider),
            model_id,
        )),
    )]));
    router
}

const MANTLE_UPSTREAM: &str = "openai.gpt-oss-120b";

fn mantle_key() -> String {
    ["zero", "detection", "mantle"].join("-")
}

/// A router whose one target is a real Bedrock Mantle Responses provider
/// dialing `origin`, checking every request. The config entry keeps its
/// `bedrock_mantle` block, so the check names the Mantle lane.
fn responses_mantle_router(origin: &str) -> Router {
    use routectl_providers::bedrock::auth::ResolvedCreds;
    use routectl_providers::mantle::MantleAuth;
    use routectl_providers::openai_responses::{
        AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
    };
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.{PROVIDER}]\n\
         kind = \"openai-responses\"\n\
         api_key_ref = \"\"\n\
         bedrock_mantle = {{ region = \"{BEDROCK_REGION}\", creds = {{ kind = \"bearer-key\", key_ref = \"{key_ref}\" }} }}\n\n\
         [models.{NICKNAME}]\n\
         provider = \"{PROVIDER}\"\n\
         upstream = \"{MANTLE_UPSTREAM}\"\n\n\
         [aliases]\n\
         {ALIAS} = \"{NICKNAME}\"\n\n\
         [retry]\n\
         max_attempts = 1\n",
        key_ref = crate::test_secret::file_ref(&mantle_key()),
    );
    let mut config = parse_config(&text).expect("valid mantle test config");
    check_every_request(&mut config);
    let mut cfg = OpenAiResponsesConfig::new_with_auth(
        PROVIDER,
        Arc::new(routectl_core::StaticToken::new(String::new())),
    );
    cfg.base_url = format!("{origin}/openai/v1");
    cfg.auth_kind = AuthKind::BedrockMantle;
    cfg.mantle = Some(MantleAuth {
        region: BEDROCK_REGION.into(),
        creds: ResolvedCreds::Bearer { key: mantle_key() },
    });
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        NICKNAME.to_string(),
        Arc::new(ResolvedModel::new(
            NICKNAME,
            PROVIDER,
            Arc::new(OpenAiResponsesProvider::new(cfg)),
            MANTLE_UPSTREAM,
        )),
    )]));
    router
}

fn check_every_request(config: &mut Config) {
    config.wire_check.first_n = u32::MAX;
    config.wire_check.sample_one_in = std::num::NonZeroU32::MIN;
}

/// One finding the walk saw, with the case that produced it.
struct Seen {
    case: String,
    lane: &'static str,
    leg: Leg,
    path: String,
    class: &'static str,
}

fn findings_of(case: &str, snapshot: &WireCheckSnapshot) -> Vec<Seen> {
    snapshot
        .findings
        .iter()
        .filter(|f| f.class.is_some())
        .map(|f| Seen {
            case: case.to_string(),
            lane: f.lane.label(),
            leg: f.leg,
            path: f.path.as_str().to_string(),
            class: f.class.map_or("-", |c| c.label()),
        })
        .collect()
}

/// Checks that compared a document, clean or not; a budget or incomplete
/// outcome compared nothing.
fn compared_checks(snapshot: &WireCheckSnapshot) -> u64 {
    snapshot
        .checks
        .iter()
        .filter(|c| matches!(c.outcome, CheckOutcome::Clean | CheckOutcome::Loss))
        .map(|c| c.reading.count)
        .sum()
}

/// Send `case` through the ingress against its own mock upstream and return
/// what the check recorded, once the response is over.
async fn walk(case: Case) -> (Case, WireCheckSnapshot) {
    let (swap, _body) = serve(&case).await;
    let snapshot = settled_snapshot(&swap, case.legs().len()).await;
    (case, snapshot)
}

/// Send `case` through the ingress against its own mock upstream; return
/// the router it ran on and the body its client received.
async fn serve(case: &Case) -> (Arc<arc_swap::ArcSwap<Router>>, Bytes) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(case.upstream.clone())
        .mount(&server)
        .await;
    let router = router_for(case, &server.uri()).await;
    let router = match case.tap_drop {
        Some(pointer) => router.with_wire_tap_drop_for_tests(pointer),
        None => router,
    };
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(router));
    let state = AppState::for_test(Arc::clone(&swap));
    let body = Bytes::from(serde_json::to_vec(&case.body).expect("case body"));
    let headers = case.headers.clone();
    let resp = match case.ingress {
        Ingress::Anthropic => {
            ingress_handle(state, headers, None, Ok(body), AnthropicIngress).await
        }
        Ingress::Chat => ingress_handle(state, headers, None, Ok(body), OpenAiIngress).await,
        Ingress::Responses => {
            ingress_handle(state, headers, None, Ok(body), ResponsesIngress).await
        }
    };
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("client body");
    assert!(
        status.is_success(),
        "{}: {status} {}",
        case.name,
        String::from_utf8_lossy(&bytes)
    );
    (swap, bytes)
}

/// The wire-check state once `legs` checks have recorded: a stream's
/// response leg records when its render task finishes, after the client
/// body ends.
async fn settled_snapshot(swap: &Arc<arc_swap::ArcSwap<Router>>, legs: usize) -> WireCheckSnapshot {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let snapshot = swap.load().wire_check_snapshot();
        let checks: u64 = snapshot.checks.iter().map(|c| c.reading.count).sum();
        if checks >= legs as u64 || std::time::Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn is_known(seen: &Seen) -> bool {
    KNOWN_UNDECLARED
        .iter()
        .any(|k| k.lane == seen.lane && k.leg == seen.leg && k.path == seen.path)
}

/// What one walk over `cases` saw: requests compared per (lane, leg), the
/// requests that did not compare every leg their lane compares, and every
/// finding.
struct Walked {
    counts: BTreeMap<(&'static str, &'static str), u64>,
    incomplete: Vec<String>,
    seen: Vec<Seen>,
}

impl Walked {
    fn total(&self) -> u64 {
        self.counts.values().sum()
    }

    fn unexplained(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter(|s| !is_known(s))
            .map(|s| {
                format!(
                    "{} {} {} {} ({})",
                    s.lane,
                    s.leg.label(),
                    s.path,
                    s.class,
                    s.case
                )
            })
            .collect()
    }
}

async fn walk_all(cases: Vec<Case>) -> Walked {
    let mut walked = Walked {
        counts: BTreeMap::new(),
        incomplete: Vec::new(),
        seen: Vec::new(),
    };
    for case in cases {
        let lane = case.lane();
        let (case, snapshot) = walk(case).await;
        if compared_checks(&snapshot) < case.legs().len() as u64 {
            let checks: Vec<(&str, u64)> = snapshot
                .checks
                .iter()
                .map(|c| (c.outcome.label(), c.reading.count))
                .collect();
            walked.incomplete.push(format!("{}: {checks:?}", case.name));
        }
        for leg in case.legs() {
            *walked.counts.entry((lane, leg.label())).or_insert(0) += 1;
        }
        walked.seen.extend(findings_of(&case.name, &snapshot));
    }
    walked
}

#[tokio::test]
async fn every_walked_request_is_clean_or_a_listed_known_loss_on_both_legs() {
    let cases: Vec<Case> = driver_cases()
        .into_iter()
        .chain(anthropic_cases())
        .chain(chat_cases())
        .chain(responses_cases())
        .chain(responses_mantle_cases())
        .chain(cross_dialect_responses_cases())
        .chain(bedrock_invoke_cases())
        .chain(bedrock_converse_cases())
        .collect();

    let walked = Box::pin(walk_all(cases)).await;

    for ((lane, leg), count) in &walked.counts {
        println!("zero-detection walked lane={lane} leg={leg} requests={count}");
    }
    assert!(walked.total() > 0, "the harness walked nothing");
    assert!(
        walked.incomplete.is_empty(),
        "every walked request must compare each leg its lane compares: {:#?}",
        walked.incomplete
    );
    let unexplained = walked.unexplained();
    for line in &unexplained {
        println!("zero-detection finding {line}");
    }
    let seen = &walked.seen;
    let matches = |k: &KnownUndeclared| {
        seen.iter()
            .any(|s| k.lane == s.lane && k.leg == s.leg && k.path == s.path)
    };
    for k in KNOWN_UNDECLARED.iter().filter(|k| matches(k)) {
        println!(
            "zero-detection known {} {} {}: {}",
            k.lane,
            k.leg.label(),
            k.path,
            k.reason
        );
    }
    let stale: Vec<&str> = KNOWN_UNDECLARED
        .iter()
        .filter(|k| !matches(k))
        .map(|k| k.path)
        .collect();
    assert!(
        stale.is_empty(),
        "listed known losses the walk no longer sees; remove them: {stale:?}"
    );
    assert!(
        unexplained.is_empty(),
        "undeclared findings: {unexplained:#?}"
    );
}

/// Positive control: the walk surfaces an unlisted, undeclared difference.
/// An upstream stream member the Chat stream render has no slot for must
/// come back as a response-leg finding, or a clean walk above proves
/// nothing.
#[tokio::test]
async fn the_walk_surfaces_an_undeclared_difference() {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "c1", "object": "chat.completion.chunk",
               "created": 1_700_000_000_i64, "model": UPSTREAM, "unmodelled_member": 1,
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let upstream = sse_response(sse(
        &[
            (
                None,
                chunk(json!({"role": "assistant", "content": "ok"}), Value::Null),
            ),
            (None, chunk(json!({}), json!("stop"))),
        ],
        true,
    ));
    let probe = case(
        "control/unmodelled-upstream-member",
        Ingress::Chat,
        json!({"model": ALIAS, "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
        upstream,
        chat_provider,
    );

    let walked = Box::pin(walk_all(vec![probe])).await;

    assert_eq!(walked.total(), 2, "one request, both legs");
    assert_eq!(
        walked.unexplained(),
        [format!(
            "{CHAT} response /unmodelled_member missing (control/unmodelled-upstream-member)"
        )]
    );
}

/// Positive control on each Bedrock lane: a member the deposited egress body
/// lost, which no declaration explains, comes back as a request-leg finding
/// on that lane, or a clean Bedrock walk above proves nothing.
#[tokio::test]
async fn the_walk_surfaces_an_undeclared_difference_on_each_bedrock_lane() {
    let hi = json!([{"role": "user", "content": "hi"}]);
    let probes = vec![
        Case {
            tap_drop: Some("/max_tokens"),
            ..bedrock_case(
                "control/invoke-lost-member",
                BedrockShape::Invoke,
                json!({"model": ALIAS, "max_tokens": 16, "messages": hi}),
                anthropic_complete(),
            )
        },
        Case {
            tap_drop: Some("/additionalModelRequestFields/context_management"),
            ..bedrock_case(
                "control/converse-lost-extra",
                BedrockShape::Converse,
                json!({"model": ALIAS, "max_tokens": 16, "messages": hi,
                       "context_management": {"edits": []}}),
                converse_complete(),
            )
        },
    ];

    let walked = Box::pin(walk_all(probes)).await;

    assert_eq!(
        walked.total(),
        3,
        "invoke on both legs, converse on its request leg"
    );
    assert_eq!(
        walked.unexplained(),
        [
            format!("{BEDROCK_INVOKE} request /max_tokens missing (control/invoke-lost-member)"),
            format!(
                "{BEDROCK_CONVERSE} request /context_management translation_unconsumed \
                 (control/converse-lost-extra)"
            ),
        ]
    );
}

/// A client of another dialect served by openai-responses never receives
/// the members only a Responses client can carry. Positive control: the
/// same upstream answer gives a Responses client both members.
#[tokio::test]
async fn only_a_responses_client_receives_responses_top_level_and_usage_members() {
    let hi = json!([{"role": "user", "content": "hi"}]);
    let input = json!([{"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}]}]);
    // (row, ingress, request body, members expected)
    let rows = [
        (
            "anthropic client",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16, "messages": hi}),
            false,
        ),
        (
            "chat client",
            Ingress::Chat,
            json!({"model": ALIAS, "messages": hi}),
            false,
        ),
        (
            "anthropic stream client",
            Ingress::Anthropic,
            json!({"model": ALIAS, "max_tokens": 16, "stream": true, "messages": hi}),
            false,
        ),
        (
            "chat stream client",
            Ingress::Chat,
            json!({"model": ALIAS, "stream": true,
                   "stream_options": {"include_usage": true}, "messages": hi}),
            false,
        ),
        (
            "responses client",
            Ingress::Responses,
            json!({"model": ALIAS, "input": input}),
            true,
        ),
    ];
    for (row, ingress, body, expected) in rows {
        // Arrange
        let probe = case(
            row,
            ingress,
            body,
            responses_stream_with_unmodeled_members(),
            responses_provider,
        );

        // Act
        let (_swap, client) = serve(&probe).await;

        // Assert
        let text = String::from_utf8_lossy(&client);
        assert!(text.contains("\"usage\""), "{row}: usage rendered: {text}");
        assert_eq!(
            text.contains("service_tier"),
            expected,
            "{row}: top-level member: {text}"
        );
        assert_eq!(
            text.contains("\"x\""),
            expected,
            "{row}: usage member: {text}"
        );
    }
}
