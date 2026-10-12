//! Zero-detection harness: every committed driver fixture, plus the
//! representative request shapes of each FIDELITY lane, through the real
//! ingress and a real egress against a mock upstream, with every request
//! checked on both legs. Any finding is either an intended routectl edit,
//! which must be declared at the site that makes it, or a known loss listed
//! in [`KNOWN_UNDECLARED`] by lane, leg and path with its reason.
//!
//! The harness prints what it walked per lane and leg, and fails when it
//! walks nothing: a green run over an empty corpus proves nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use routectl_core::wire_check::Leg;
use routectl_router::wire_check_state::{CheckOutcome, WireCheckSnapshot};
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, Router};
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
const RESPONSES: &str = "responses->openai-responses";

/// Findings the check correctly reports that no declaration explains.
const KNOWN_UNDECLARED: &[KnownUndeclared] = &[
    KnownUndeclared {
        lane: ANTHROPIC,
        leg: Leg::Response,
        path: "/usage/server_tool_use",
        reason: "real loss: the closing message_delta render drops server_tool_use",
    },
    KnownUndeclared {
        lane: ANTHROPIC,
        leg: Leg::Response,
        path: "/usage/output_tokens_details",
        reason: "real loss: the closing message_delta render drops output_tokens_details",
    },
    KnownUndeclared {
        lane: CHAT,
        leg: Leg::Response,
        path: "/system_fingerprint",
        reason: "real loss: the stream render stamps a null system_fingerprint",
    },
    KnownUndeclared {
        lane: CHAT,
        leg: Leg::Response,
        path: "/usage/prompt_tokens_details",
        reason: "real loss: the stream render never mirrors cached_tokens back",
    },
    KnownUndeclared {
        lane: RESPONSES,
        leg: Leg::Request,
        path: "/temperature",
        reason: "real loss: the Responses egress has no temperature field and drops it",
    },
    KnownUndeclared {
        lane: RESPONSES,
        leg: Leg::Request,
        path: "/top_p",
        reason: "real loss: the Responses egress has no top_p field and drops it",
    },
    KnownUndeclared {
        lane: RESPONSES,
        leg: Leg::Response,
        path: "/created_at",
        reason: "real loss: the stream render has no upstream created_at to echo",
    },
    KnownUndeclared {
        lane: RESPONSES,
        leg: Leg::Response,
        path: "/output/0/id",
        reason: "real loss: the egress drops the upstream message item id, so no render can echo it",
    },
];

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

/// One request through the harness: what the client sends, how the
/// upstream answers, and the provider config the lane uses.
struct Case {
    name: String,
    ingress: Ingress,
    headers: HeaderMap,
    body: Value,
    upstream: ResponseTemplate,
    provider: fn(&str) -> ProviderEntry,
    model: fn() -> ModelEntry,
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

fn responses_document(created_at: i64) -> Value {
    json!({
        "id": "resp_1", "object": "response", "created_at": created_at,
        "status": "completed", "model": UPSTREAM,
        "output": [{"type": "message", "id": "m1", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "ok", "annotations": []}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    })
}

/// The Responses egress reads every answer as a stream.
fn responses_stream(created_at: i64) -> ResponseTemplate {
    let done = responses_document(created_at);
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
                provider: anthropic_provider,
                model: plain_model,
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
        provider,
        model: plain_model,
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
            responses_stream(0),
            responses_provider,
        ),
        case(
            "responses/stream",
            Ingress::Responses,
            json!({"model": ALIAS, "stream": true, "input": input}),
            responses_stream(0),
            responses_provider,
        ),
        case(
            "responses/sampling",
            Ingress::Responses,
            json!({"model": ALIAS, "temperature": 0.3, "top_p": 0.9,
                   "max_output_tokens": 32, "input": input}),
            responses_stream(0),
            responses_provider,
        ),
        case(
            "responses/instructions",
            Ingress::Responses,
            json!({"model": ALIAS, "instructions": "be brief", "input": input}),
            responses_stream(0),
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
            responses_stream(0),
            responses_provider,
        ),
        case(
            "responses/upstream-created-at",
            Ingress::Responses,
            json!({"model": ALIAS, "stream": true, "input": input}),
            responses_stream(1_700_000_000),
            responses_provider,
        ),
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
// The walk
// ---------------------------------------------------------------------------

/// Checks every request on both legs.
async fn router_for(case: &Case, base: &str) -> Router {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut config = Config {
        providers: BTreeMap::from([(PROVIDER.to_string(), (case.provider)(base))]),
        models: BTreeMap::from([(NICKNAME.to_string(), (case.model)())]),
        aliases: BTreeMap::from([(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()))]),
        retry,
        ..Default::default()
    };
    config.wire_check.first_n = u32::MAX;
    config.wire_check.sample_one_in = std::num::NonZeroU32::MIN;
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("build router")
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
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(case.upstream.clone())
        .mount(&server)
        .await;
    let router = router_for(&case, &server.uri()).await;
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
    let snapshot = settled_snapshot(&swap).await;
    (case, snapshot)
}

/// The wire-check state once both legs have recorded: a stream's response
/// leg records when its render task finishes, after the client body ends.
async fn settled_snapshot(swap: &Arc<arc_swap::ArcSwap<Router>>) -> WireCheckSnapshot {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let snapshot = swap.load().wire_check_snapshot();
        let checks: u64 = snapshot.checks.iter().map(|c| c.reading.count).sum();
        if checks >= 2 || std::time::Instant::now() >= deadline {
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
/// requests that did not compare both legs, and every finding.
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
        let lane = case.ingress.lane();
        let (case, snapshot) = walk(case).await;
        if compared_checks(&snapshot) < 2 {
            let checks: Vec<(&str, u64)> = snapshot
                .checks
                .iter()
                .map(|c| (c.outcome.label(), c.reading.count))
                .collect();
            walked.incomplete.push(format!("{}: {checks:?}", case.name));
        }
        for leg in [Leg::Request, Leg::Response] {
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
        .collect();

    let walked = walk_all(cases).await;

    for ((lane, leg), count) in &walked.counts {
        println!("zero-detection walked lane={lane} leg={leg} requests={count}");
    }
    assert!(walked.total() > 0, "the harness walked nothing");
    assert!(
        walked.incomplete.is_empty(),
        "every walked request must compare both legs: {:#?}",
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

    let walked = walk_all(vec![probe]).await;

    assert_eq!(walked.total(), 2, "one request, both legs");
    assert_eq!(
        walked.unexplained(),
        [format!(
            "{CHAT} response /unmodelled_member missing (control/unmodelled-upstream-member)"
        )]
    );
}
