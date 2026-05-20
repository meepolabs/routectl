//! Integration tests for the openai-compat provider.
//!
//! Unit tests for request/response/SSE normalization live inside each
//! submodule's own #[cfg(test)] blocks. Here we exercise:
//!   - wiremock-based complete() and stream() end-to-end paths.
//!   - DeepSeek multi-turn: reasoning_content stripped from outgoing history.
//!   - Cross-crate smoke: provider id is returned on ChatResponse.

#![cfg(feature = "openai-compat")]

use futures::StreamExt;
use routectl_core::{Message, MessageContent, Provider, ReasoningDetailKind, Role};
use routectl_providers::openai_compat::{
    OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_provider(base_url: &str, dialect: ReasoningDialect) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "test-provider".into(),
        base_url: base_url.into(),
        api_key: "test-key".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: dialect,
        history_reasoning: routectl_providers::openai_compat::HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
    })
}

fn user_request(model: &str) -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: model.into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("What is 2+2?".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        temperature: Some(0.5),
        max_tokens: Some(128),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// extra_headers reserved-name guard (parity with anthropic_api / bedrock)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extra_headers_reserved_name_does_not_override_authorization() {
    // TOML-supplied `extra_headers = { "authorization" = "..." }` must not
    // bypass the provider's Bearer auth. HeaderMap::insert replaces by
    // name, so without the is_reserved_extra_header guard this would
    // silently override the auth header and ship the user-supplied value
    // upstream.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer real-token",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "ok",
            "model": "test",
            "created": 1,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&server)
        .await;

    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "test-provider".into(),
        base_url: server.uri(),
        api_key: "real-token".into(),
        // Attempt to override with a different value -- must be ignored.
        header_extras: vec![("authorization".into(), "Bearer attacker-token".into())],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: routectl_providers::openai_compat::HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
    });

    // If the guard is missing, the wiremock matcher above won't find
    // "Bearer real-token" (the override would have replaced it) and the
    // mock server returns 404, surfacing here as an upstream error.
    let resp = provider.complete(user_request("test")).await;
    assert!(
        resp.is_ok(),
        "guard must keep the real Bearer token: {resp:?}"
    );
}

// ---------------------------------------------------------------------------
// complete() integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_openai_dialect_returns_normalized_response() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-001",
        "model": "gpt-4o",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Four."
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 2,
            "total_tokens": 12
        }
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    let req = user_request("gpt-4o");
    let resp = provider.complete(req).await.unwrap();

    assert_eq!(resp.id, "chatcmpl-001");
    assert_eq!(resp.routectl_provider.as_deref(), Some("test-provider"));
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t, "Four."),
        _ => panic!("expected text content"),
    }
}

#[tokio::test]
async fn complete_deepseek_lifts_reasoning_content() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-ds-001",
        "model": "deepseek-reasoner",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "The answer is 4.",
                "reasoning": "2 plus 2 equals 4."
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 30,
            "total_tokens": 40
        }
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::DeepSeek);
    let req = user_request("deepseek-reasoner");
    let resp = provider.complete(req).await.unwrap();

    let details = &resp.choices[0].message.reasoning_details;
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].format.as_deref(), Some("deepseek-v1"));
    assert!(matches!(details[0].kind, ReasoningDetailKind::Text));
    assert_eq!(details[0].payload["text"], "2 plus 2 equals 4.");
}

#[tokio::test]
async fn complete_raw_think_tag_strips_and_lifts() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-think-001",
        "model": "qwq-32b",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "<think>inner reasoning</think>Outer answer."
            },
            "finish_reason": "stop"
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::RawThinkTag);
    let req = user_request("qwq-32b");
    let resp = provider.complete(req).await.unwrap();

    let details = &resp.choices[0].message.reasoning_details;
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].format.as_deref(), Some("raw-think-tag-v1"));
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t.trim(), "Outer answer."),
        _ => panic!("expected text"),
    }
}

#[tokio::test]
async fn complete_upstream_error_surfaces_as_error_upstream() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    let err = provider.complete(user_request("gpt-4o")).await.unwrap_err();

    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(status, 429);
            assert!(body.contains("rate limited"));
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// stream() integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_openai_dialect_collects_chunks() {
    let server = MockServer::start().await;

    let sse_body = concat!(
        "data: {\"id\":\"chunk-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"He\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chunk-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chunk-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    let mut stream = provider.stream(user_request("gpt-4o")).await.unwrap();

    let mut content_parts = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            if let Some(c) = &choice.delta.content {
                content_parts.push(c.clone());
            }
        }
    }

    assert_eq!(content_parts.join(""), "Hello");
}

#[tokio::test]
async fn stream_deepseek_lifts_reasoning_content_in_chunks() {
    let server = MockServer::start().await;

    let sse_body = concat!(
        "data: {\"id\":\"ds-1\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"chain of\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"ds-1\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\" thought\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"ds-1\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::DeepSeek);
    let mut stream = provider
        .stream(user_request("deepseek-reasoner"))
        .await
        .unwrap();

    let mut reasoning_chunks: Vec<String> = Vec::new();
    let mut content_chunks: Vec<String> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            for detail in &choice.delta.reasoning_details {
                if let Some(t) = detail.payload["text"].as_str() {
                    reasoning_chunks.push(t.into());
                }
            }
            if let Some(c) = &choice.delta.content {
                content_chunks.push(c.clone());
            }
        }
    }

    assert_eq!(reasoning_chunks.join(""), "chain of thought");
    assert_eq!(content_chunks.join(""), "answer");
}

#[tokio::test]
async fn stream_raw_think_tag_state_machine_across_chunks() {
    let server = MockServer::start().await;

    // The <think> tag opens in chunk 1 and closes in chunk 2 -- classic split.
    let sse_body = concat!(
        "data: {\"id\":\"t1\",\"model\":\"qwq-32b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"before<think>partial\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"t1\",\"model\":\"qwq-32b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" thought</think>after\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::RawThinkTag);
    let mut stream = provider.stream(user_request("qwq-32b")).await.unwrap();

    let mut outside = String::new();
    let mut reasoning = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            if let Some(c) = &choice.delta.content {
                outside.push_str(c);
            }
            if let Some(r) = &choice.delta.reasoning {
                reasoning.push_str(r);
            }
        }
    }

    assert_eq!(outside, "beforeafter");
    assert_eq!(reasoning, "partial thought");
}

// ---------------------------------------------------------------------------
// DeepSeek multi-turn: outgoing body must not contain reasoning_content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deepseek_multiturn_strips_reasoning_from_outgoing_body() {
    let server = MockServer::start().await;

    // We capture the request body to inspect it.
    let response_body = json!({
        "id": "chatcmpl-mt-001",
        "model": "deepseek-reasoner",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    // Build a request whose history contains an assistant message with
    // reasoning_details and reasoning fields populated.
    let mut req = user_request("deepseek-reasoner");
    req.messages.push(Message {
        role: Role::Assistant,
        content: MessageContent::Text("Prior answer".into()),
        reasoning: Some("prior chain of thought".into()),
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    });
    req.messages.push(Message {
        role: Role::User,
        content: MessageContent::Text("Follow-up question".into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    });

    let provider = make_provider(&server.uri(), ReasoningDialect::DeepSeek);

    // Inspect the normalized request before it goes out.
    let normalized = provider.normalize_request(&req).unwrap();
    let messages = normalized["messages"].as_array().unwrap();
    for msg in messages {
        assert!(
            msg.get("reasoning_content").is_none(),
            "reasoning_content must not be in outgoing body: {msg}"
        );
        assert!(
            msg.get("reasoning").is_none(),
            "reasoning must not be in outgoing body: {msg}"
        );
        assert!(
            msg.get("reasoning_details").is_none(),
            "reasoning_details must not be in outgoing body: {msg}"
        );
    }

    // Also verify the round-trip through complete() succeeds.
    let resp = provider.complete(req).await.unwrap();
    assert_eq!(resp.id, "chatcmpl-mt-001");
}

// ---------------------------------------------------------------------------
// strict_translation: lossy seam policy
// ---------------------------------------------------------------------------

#[test]
fn strict_translation_off_warns_and_allows_request() {
    // Default mode: cache_control on a user text block is silently dropped
    // (warn-only), and the request body still serializes. Pin that the
    // request reaches the upstream wire shape without erroring.
    use routectl_core::{
        cache_control::CacheControl, content_part::ContentPart, ChatRequest, KnownContentPart,
    };
    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat:test".into(),
        base_url: "http://localhost".into(),
        api_key: "k".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: routectl_providers::openai_compat::HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
    });
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "hi".into(),
                cache_control: Some(CacheControl::ephemeral_5m()),
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        cache_control: Some(CacheControl::ephemeral_5m()),
        ..Default::default()
    };
    let body = provider
        .normalize_request(&req)
        .expect("default warns, returns Ok");
    assert!(
        body.get("cache_control").is_none(),
        "wire body must not carry cache_control on the openai-compat seam"
    );
}

#[test]
fn strict_translation_on_rejects_canonical_only_fields() {
    // Strict mode: same lossy seam returns an Error::Validation that
    // names the offending fields. Wired through OpenAiCompatConfig from
    // [server] strict_translation at provider build time.
    use routectl_core::{cache_control::CacheControl, ChatRequest};
    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat:strict".into(),
        base_url: "http://localhost".into(),
        api_key: "k".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: routectl_providers::openai_compat::HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: true,
        disable_stream_include_usage: false,
    });
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        cache_control: Some(CacheControl::ephemeral_5m()),
        anthropic_beta: vec!["context-1m-2025-08-07".into()],
        ..Default::default()
    };
    let err = provider
        .normalize_request(&req)
        .expect_err("strict_translation must reject canonical-only fields");
    let msg = format!("{err}");
    assert!(
        msg.contains("strict_translation"),
        "expected strict_translation marker in error: {msg}"
    );
    assert!(
        msg.contains("cache_control") && msg.contains("anthropic_beta"),
        "expected both dropped fields named in error: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Dialect format tag correctness
// ---------------------------------------------------------------------------

#[test]
fn dialect_format_tags_are_correct() {
    use routectl_providers::openai_compat::ReasoningDialect;
    assert_eq!(ReasoningDialect::OpenAi.format_tag(), "openai-responses-v1");
    assert_eq!(ReasoningDialect::DeepSeek.format_tag(), "deepseek-v1");
    assert_eq!(ReasoningDialect::Vllm.format_tag(), "vllm-reasoning-v1");
    assert_eq!(
        ReasoningDialect::RawThinkTag.format_tag(),
        "raw-think-tag-v1"
    );
    assert_eq!(
        ReasoningDialect::OpenRouter.format_tag(),
        "openrouter-passthrough-v1"
    );
    assert_eq!(ReasoningDialect::Passthrough.format_tag(), "passthrough-v1");
}

// ---------------------------------------------------------------------------
// Passthrough and OpenRouter: no mutation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_passthrough_no_mutation() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-pt-001",
        "model": "some-model",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "pass"},
            "finish_reason": "stop"
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::Passthrough);
    let resp = provider.complete(user_request("some-model")).await.unwrap();
    assert_eq!(resp.choices[0].message.reasoning_details.len(), 0);
}

#[tokio::test]
async fn complete_vllm_lifts_reasoning_content() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-vllm-001",
        "model": "qwen3-30b",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "result",
                "reasoning": "vllm trace"
            },
            "finish_reason": "stop"
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::Vllm);
    let req = user_request("qwen3-30b");
    let resp = provider.complete(req).await.unwrap();

    let details = &resp.choices[0].message.reasoning_details;
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].format.as_deref(), Some("vllm-reasoning-v1"));
    assert_eq!(details[0].payload["text"], "vllm trace");
}
