//! End-to-end pin for the per-egress-allowlist contract: a foreign
//! upstream (openai-compat with the DeepSeek dialect) flowing through
//! the canonical normalize seam and the Anthropic ingress render must
//! NOT leak openai-compat envelope keys, vendor usage sub-bags, or a
//! `signature: null` thinking key into the Anthropic-shape body served
//! back to a claude-code style caller.
//!
//! See docs/WIRE-GOTCHAS.md for background on the signature seam this
//! test pins -- Anthropic 400s on a `signature: null` thinking block
//! mid-conversation when a provider switch lands on a non-signing
//! upstream.
//!
//! A second section pins that a zero-argument tool call from a Responses
//! upstream renders as an empty object on every ingress.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_providers::openai_compat::dialect::ReasoningDialect;
use routectl_providers::openai_compat::response::{
    OPENAI_COMPAT_ENVELOPE_KEYS, OPENAI_COMPAT_USAGE_SUBKEYS, normalize,
};
use routectl_providers::openai_responses::AuthKind as ResponsesAuthKind;
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

/// Build a raw openai-compat response (DeepSeek shape) carrying every
/// field on the openai-compat strip allow-lists
/// (`OPENAI_COMPAT_ENVELOPE_KEYS`, `OPENAI_COMPAT_USAGE_SUBKEYS`) plus
/// a DeepSeek-style unsigned `reasoning_content` so the render path
/// also exercises the no-null-signature seam.
fn deepseek_raw_with_envelope_and_thinking() -> Value {
    json!({
        "id": "chatcmpl-cross-dialect",
        "model": "deepseek-v4-pro",
        "created": 1_700_000_000_i64,
        // openai-compat envelope tells.
        "object": "chat.completion",
        "system_fingerprint": "fp_test",
        "cost": "0",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "the answer is 42",
                // DeepSeek dialect carries reasoning under reasoning_content;
                // the dialect lift hoists it into ReasoningDetail with no
                // signature (deepseek-v4-pro does not sign).
                "reasoning_content": "thinking trace"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            // DeepSeek vendor cache stat -- must lift to canonical
            // cache_read_input_tokens.
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20,
            // OpenAI vendor sub-bags -- must be stripped after lift.
            "prompt_tokens_details": {"cached_tokens": 64},
            "completion_tokens_details": {"reasoning_tokens": 7}
        }
    })
}

/// Drive the full pipeline (openai-compat normalize -> Anthropic
/// ingress render) and assert the Anthropic-shape body served back is
/// clean.
#[test]
fn openai_compat_to_anthropic_render_strips_vendor_keys_and_omits_null_signature() {
    // Arrange
    let raw = deepseek_raw_with_envelope_and_thinking();

    // Act: openai-compat egress normalize -> canonical ChatResponse.
    let canonical = normalize("p-deepseek", raw, ReasoningDialect::DeepSeek)
        .expect("normalize must succeed on a well-formed openai-compat body");

    // Sanity: the dialect lifted reasoning_content into a
    // ReasoningDetail without a signature, so the Anthropic render
    // exercises the no-null-signature path.
    let details = &canonical
        .choices
        .first()
        .expect("choice present")
        .message
        .reasoning_details;
    assert_eq!(details.len(), 1, "DeepSeek dialect must lift one detail");
    assert!(
        details[0].payload.get("signature").is_none(),
        "DeepSeek lift must not synthesize a signature; got payload: {:?}",
        details[0].payload
    );

    // Anthropic ingress render -> wire body served back to caller.
    let body: Value = serde_json::from_slice(
        &AnthropicIngress
            .render_response(canonical)
            .expect("render_response must succeed"),
    )
    .expect("rendered body is valid JSON");

    // Assert (1): top-level envelope keys do not leak. Iterate the
    // shared allow-list so a future addition to
    // `OPENAI_COMPAT_ENVELOPE_KEYS` is automatically pinned by this
    // test instead of needing a mirrored literal kept in sync.
    let obj = body.as_object().expect("rendered body is a JSON object");
    for k in OPENAI_COMPAT_ENVELOPE_KEYS {
        assert!(
            !obj.contains_key(*k),
            "openai-compat envelope key {k} must not appear at top level; got body: {body}"
        );
    }

    // Assert (2): usage sub-bags do not leak; canonical lift surfaces
    // the values via Anthropic-shape fields. Same allow-list pivot:
    // iterate `OPENAI_COMPAT_USAGE_SUBKEYS` rather than a copy.
    let usage = body.get("usage").expect("usage rendered");
    let usage_obj = usage.as_object().expect("usage is a JSON object");
    for k in OPENAI_COMPAT_USAGE_SUBKEYS {
        assert!(
            !usage_obj.contains_key(*k),
            "openai-compat usage sub-bag {k} must not appear in rendered usage; got: {usage}"
        );
    }

    // Assert (3): cache_read_input_tokens lifted to the Anthropic-shape
    // canonical surface.
    assert_eq!(
        usage_obj.get("cache_read_input_tokens"),
        Some(&json!(80)),
        "cache_read_input_tokens must be lifted from prompt_cache_hit_tokens; got: {usage}"
    );

    // Assert (4): the rendered thinking block carries NO `signature`
    // key when the source detail had no signature. Real Anthropic
    // 400s on signature:null mid-conversation provider switch.
    let content = body
        .get("content")
        .and_then(|v| v.as_array())
        .expect("content is array");
    let thinking = content
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
        .expect("thinking block surfaced from DeepSeek reasoning_content");
    let thinking_obj = thinking.as_object().expect("thinking block is an object");
    assert!(
        !thinking_obj.contains_key("signature"),
        "thinking block must not carry signature: null when source has no signature; got: {thinking}"
    );
    assert_eq!(thinking["thinking"], "thinking trace");
}

/// Drive a canonical ChatResponse that has a tool_use block in BOTH
/// `tool_calls` and `ContentPart::ToolUse` through the OpenAI ingress
/// render path and assert:
/// (a) `tool_calls` is populated with exactly one entry, and
/// (b) the `content` field contains NO `tool_use` blocks (the
///     `strip_tool_use_parts_when_tool_calls_present` dedup fires).
///
/// The text part survives the strip and collapses to a string.
#[test]
fn openai_render_dedupes_tool_use_when_present_in_both_tool_calls_and_parts() {
    use routectl_cli::ingress::openai::OpenAiIngress;
    use routectl_core::{
        ChatResponse, Choice, ContentPart, KnownContentPart, Message, MessageContent, Role, Usage,
    };

    let resp = ChatResponse {
        id: "chatcmpl-dup".into(),
        model: "claude-haiku-4-5".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "I will call the tool.".into(),
                        citations: None,
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::ToolUse {
                        id: "call_dedup".into(),
                        name: "calculator".into(),
                        input: json!({"a": 1, "b": 2}),
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_dedup",
                    "type": "function",
                    "function": {"name": "calculator", "arguments": "{\"a\":1,\"b\":2}"}
                })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };

    let v: Value = serde_json::from_slice(
        &OpenAiIngress
            .render_response(resp)
            .expect("render_response must succeed"),
    )
    .expect("rendered body is valid JSON");

    // Assert (1): tool_calls populated with exactly one entry.
    let tool_calls = v["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("tool_calls must be an array");
    assert_eq!(
        tool_calls.len(),
        1,
        "expected exactly one tool call; got {tool_calls:?}"
    );
    assert_eq!(tool_calls[0]["id"], "call_dedup");

    // Assert (2): content must NOT contain any tool_use blocks after dedup.
    // After strip, the text-only parts collapse to a string; the content
    // field will be a string, not an array with tool_use blocks.
    let content = &v["choices"][0]["message"]["content"];
    if let Some(arr) = content.as_array() {
        let tool_use_blocks: Vec<_> = arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            .collect();
        assert!(
            tool_use_blocks.is_empty(),
            "content must not contain tool_use blocks after dedup; got: {arr:?}"
        );
    }
    // The text part survives the strip and collapses to a string.
    if let Some(s) = content.as_str() {
        assert!(
            s.contains("I will call the tool"),
            "collapsed text content must be preserved; got: {s}"
        );
    }
}

// ---------------------------------------------------------------------------
// A zero-argument `function_call` from a Responses upstream, taken on the
// non-streaming path, reaches every ingress as an EMPTY OBJECT. The Responses
// wire ships `arguments: ""` for a call with no parameters. Each test drives
// the real axum server against a wiremock Responses upstream (whose
// `complete` drains SSE until `response.completed`).
// ---------------------------------------------------------------------------

const CALL_ID: &str = "call_zero_arg";
const TOOL_NAME: &str = "current_time";

async fn spawn(config: Arc<Config>) -> String {
    let config = common::isolate_usage_db(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });
    common::readiness::await_health(&base_url).await;
    base_url
}

/// A terminal `response.completed` whose only output item is a
/// `function_call` with empty `arguments`.
fn zero_arg_sse_body() -> String {
    let completed = json!({
        "id": "resp_zero_arg",
        "object": "response",
        "status": "completed",
        "model": "mock-model",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": CALL_ID,
            "name": TOOL_NAME,
            "arguments": "",
            "status": "completed"
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed).expect("completed body serializes")
    )
}

async fn zero_arg_responses_lane() -> (MockServer, Arc<Config>) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(zero_arg_sse_body()),
        )
        .mount(&upstream)
        .await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "responses-mock".to_string(),
        ProviderEntry::openai_responses(common::file_ref("test-key"))
            .with_openai_responses_base_url(upstream.uri())
            .with_openai_responses_auth_kind(ResponsesAuthKind::ApiKey),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "target".to_string(),
        ModelEntry::new("responses-mock", "mock-model"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "mock-model".to_string(),
        AliasValue::Single("target".into()),
    );
    let config = Arc::new(Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            auth: None,
            strict_translation: false,
            allow_disable_fallbacks: true,
            ..Default::default()
        },
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    });
    (upstream, config)
}

/// POST `body` to `route` on a server fronting the zero-arg upstream and
/// return the JSON the client is served.
async fn served_body(route: &str, body: Value) -> Value {
    let (upstream, config) = zero_arg_responses_lane().await;
    let base = spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}{route}"))
        .json(&body)
        .send()
        .await
        .expect("ingress request sends");
    let status = resp.status();
    let served: Value = resp.json().await.expect("served body is JSON");
    assert_eq!(status, 200, "ingress returned {status}: {served}");

    let received = upstream
        .received_requests()
        .await
        .expect("requests recorded");
    assert_eq!(received.len(), 1, "expected exactly one upstream call");
    served
}

fn assert_empty_object_arguments(arguments: &Value, served: &Value) {
    let text = arguments
        .as_str()
        .unwrap_or_else(|| panic!("arguments must be a JSON string; served {served}"));
    let parsed: Value = serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("arguments {text:?} must parse as JSON ({e}); served {served}"));
    assert_eq!(parsed, json!({}), "served {served}");
}

#[tokio::test]
async fn zero_arg_function_call_reaches_chat_completions_as_an_empty_object() {
    // Arrange
    let request = json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "what time is it"}]
    });

    // Act
    let served = served_body("/v1/chat/completions", request).await;

    // Assert
    let tool_calls = served["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap_or_else(|| panic!("tool_calls must be an array; served {served}"));
    assert_eq!(tool_calls.len(), 1, "served {served}");
    assert_eq!(tool_calls[0]["id"], CALL_ID);
    assert_empty_object_arguments(&tool_calls[0]["function"]["arguments"], &served);
}

#[tokio::test]
async fn zero_arg_function_call_reaches_anthropic_messages_as_an_empty_object() {
    // Arrange
    let request = json!({
        "model": "mock-model",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "what time is it"}]
    });

    // Act
    let served = served_body("/v1/messages", request).await;

    // Assert
    let tool_uses: Vec<&Value> = served["content"]
        .as_array()
        .unwrap_or_else(|| panic!("content must be an array; served {served}"))
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .collect();
    assert_eq!(tool_uses.len(), 1, "served {served}");
    assert_eq!(tool_uses[0]["id"], CALL_ID);
    let input = &tool_uses[0]["input"];
    assert!(
        input.as_object().is_some_and(serde_json::Map::is_empty),
        "tool_use.input must be an empty object; served {served}"
    );
}

#[tokio::test]
async fn zero_arg_function_call_reaches_responses_as_an_empty_object() {
    // Arrange
    let request = json!({
        "model": "mock-model",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "what time is it"}]
        }]
    });

    // Act
    let served = served_body("/v1/responses", request).await;

    // Assert
    let calls: Vec<&Value> = served["output"]
        .as_array()
        .unwrap_or_else(|| panic!("output must be an array; served {served}"))
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect();
    assert_eq!(calls.len(), 1, "served {served}");
    assert_eq!(calls[0]["call_id"], CALL_ID);
    assert_empty_object_arguments(&calls[0]["arguments"], &served);
}
