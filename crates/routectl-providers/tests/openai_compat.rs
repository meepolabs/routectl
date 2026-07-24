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
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

fn user_request(model: &str) -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: model.into(),
        messages: vec![Message {
            refusal: None,
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
    // name, so without the is_auth_header skip path in apply_header_extras
    // this would silently override the auth header and ship the
    // user-supplied value upstream.
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
        #[cfg(feature = "bedrock")]
        mantle: None,
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

#[tokio::test]
async fn complete_429_populates_upstream_type_and_code() {
    // Arrange
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {
                "type": "rate_limit_exceeded",
                "code": "rate_limited",
                "message": "rate limited"
            }
        })))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    // Act
    let err = provider.complete(user_request("gpt-4o")).await.unwrap_err();

    // Assert: the upstream classifier is lifted into upstream_type/code.
    match err {
        routectl_core::Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(upstream_type.as_deref(), Some("rate_limit_exceeded"));
            assert_eq!(upstream_code.as_deref(), Some("rate_limited"));
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

/// Pin: when a RawThinkTag stream ends (`[DONE]`) while the accumulator
/// is still holding back bytes that are a prefix of `<think>` (e.g.
/// `<thi`), those bytes are real visible content and must reach the
/// client. Before the flush they were silently dropped at the `[DONE]`
/// exit. Upstream emits `hello<thi` then `[DONE]` and never completes
/// the tag; the client must receive the full `hello<thi`.
#[tokio::test]
async fn stream_raw_think_tag_flushes_pending_on_done() {
    let server = MockServer::start().await;

    // One chunk whose content ends in a partial `<think>` prefix, then
    // a bare `[DONE]` -- the tag never completes.
    let sse_body = concat!(
        "data: {\"id\":\"t1\",\"model\":\"qwq-32b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello<thi\"},\"finish_reason\":null}]}\n\n",
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
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            if let Some(c) = &choice.delta.content {
                outside.push_str(c);
            }
        }
    }

    // All bytes survive: the held-back `<thi` is flushed at [DONE].
    assert_eq!(outside, "hello<thi");
}

/// Pin: for `n > 1` the streaming stop-sequence heuristic must track
/// per-choice content, not one shared buffer. Choice 0 streams `fooEND`
/// (ends with the configured stop -> matches); choice 1 streams `bar`
/// (no stop -> must NOT match). A single shared accumulator would bleed
/// choice 0's `END` suffix into choice 1's match, producing a false
/// positive on choice 1.
///
/// Two stop sequences are configured so the single-stop fallback in
/// `detect_matched_stop_sequence` cannot fire: this isolates the
/// per-choice suffix-match contract, not the single-fence fallback.
#[tokio::test]
async fn stream_stop_sequence_heuristic_is_per_choice_for_n_gt_1() {
    let server = MockServer::start().await;

    // Two choices interleaved across content chunks, then a terminal
    // chunk carrying finish_reason="stop" for both. Choice 0 -> "fooEND",
    // choice 1 -> "bar".
    let sse_body = concat!(
        "data: {\"id\":\"s1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"foo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"m\",\"choices\":[{\"index\":1,\"delta\":{\"content\":\"bar\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"END\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"},{\"index\":1,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
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
    let mut req = user_request("m");
    req.n = Some(2);
    req.stop = Some(vec!["END".to_string(), "STOP".to_string()]);

    let mut stream = provider.stream(req).await.unwrap();

    // Collect the matched_stop_sequence per choice index from the
    // terminal chunk (the one carrying finish_reason="stop").
    let mut matched: std::collections::HashMap<u32, Option<String>> =
        std::collections::HashMap::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            if choice.finish_reason.as_deref() == Some("stop") {
                matched.insert(choice.index, choice.matched_stop_sequence.clone());
            }
        }
    }

    assert_eq!(
        matched.get(&0).cloned().flatten().as_deref(),
        Some("END"),
        "choice 0 streamed 'fooEND' -> must match the stop sequence",
    );
    assert_eq!(
        matched.get(&1).cloned().flatten(),
        None,
        "choice 1 streamed 'bar' -> must NOT match (no per-choice bleed)",
    );
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
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text("Prior answer".into()),
        reasoning: Some("prior chain of thought".into()),
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    });
    req.messages.push(Message {
        refusal: None,
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
        ChatRequest, KnownContentPart, cache_control::CacheControl, content_part::ContentPart,
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
        #[cfg(feature = "bedrock")]
        mantle: None,
    });
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "hi".into(),
                citations: None,
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
    use routectl_core::{ChatRequest, cache_control::CacheControl};
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
        #[cfg(feature = "bedrock")]
        mantle: None,
    });
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![Message {
            refusal: None,
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
    // The strict-translation rejection is client-facing (HTTP 400), so it
    // must name the offending feature -- never the internal egress/provider
    // id, which would leak routing topology to a remote caller.
    assert!(
        !msg.contains("openai-compat:strict"),
        "strict_translation error must not echo the provider id: {msg}"
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

// ---------------------------------------------------------------------------
// probe(): free reachability against /models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_200_models_list_is_reachable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .expect(1) // AT MOST ONE upstream request: no retry.
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    assert_eq!(
        provider.probe().await,
        routectl_core::ProbeOutcome::Reachable
    );
}

#[tokio::test]
async fn probe_401_is_auth_failed_without_leaking_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    match provider.probe().await {
        routectl_core::ProbeOutcome::AuthFailed(reason) => {
            assert!(!reason.contains("test-key"), "reason leaked the api key");
            assert!(!reason.contains(&server.uri()), "reason leaked the url");
        }
        other => panic!("expected AuthFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_403_is_auth_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    assert!(matches!(
        provider.probe().await,
        routectl_core::ProbeOutcome::AuthFailed(_)
    ));
}

#[tokio::test]
async fn probe_connection_refused_is_unreachable() {
    // A closed loopback port (nothing binds 127.0.0.1:1) deterministically
    // refuses the connect. Stands in for DNS / connect / TLS transport
    // failures, which all fold into Unreachable.
    let provider = make_provider("http://127.0.0.1:1", ReasoningDialect::OpenAi);
    assert!(matches!(
        provider.probe().await,
        routectl_core::ProbeOutcome::Unreachable(_)
    ));
}

// ---------------------------------------------------------------------------
// Response-body cap adoption (the DoS response-body cap). One byte over the 16 MiB ceiling
// exercised end-to-end; wiremock advertises an honest Content-Length, so the
// fast-reject guard trips without transferring the whole body.
// ---------------------------------------------------------------------------

/// One byte past the shared 16 MiB response-body cap.
const OVER_CAP_BODY_LEN: usize = 16 * 1024 * 1024 + 1;

#[tokio::test]
async fn complete_success_body_over_cap_maps_to_502() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; OVER_CAP_BODY_LEN]))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    let (result, events) =
        routectl_testkit::with_capture(provider.complete(user_request("gpt-4o"))).await;
    let err = result.unwrap_err();

    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(status, 502, "an unreadable 2xx body must classify as 502");
            // Exact match proves the body is the bounded fixed message, not
            // an echo of the 16 MiB raw upstream body.
            assert_eq!(body, "response body exceeded 16777216-byte cap");
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }

    let cap_warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.field("path").is_some())
        .collect();
    assert_eq!(cap_warns.len(), 1, "exactly one cap-trip WARN per trip");
    let w = cap_warns[0];
    assert_eq!(w.field("provider"), Some("test-provider"));
    assert_eq!(w.field("status"), Some("200"));
    assert_eq!(
        w.field("body_cap_bytes"),
        Some((16 * 1024 * 1024).to_string().as_str())
    );
    assert_eq!(
        w.field("content_length"),
        Some(format!("Some({OVER_CAP_BODY_LEN})").as_str())
    );
    assert_eq!(w.field("body_truncated"), Some("true"));
    assert_eq!(w.field("path"), Some("complete_success_body"));
}

#[tokio::test]
async fn complete_error_body_over_cap_preserves_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_bytes(vec![b'a'; OVER_CAP_BODY_LEN]))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    let (result, events) =
        routectl_testkit::with_capture(provider.complete(user_request("gpt-4o"))).await;
    let err = result.unwrap_err();

    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(
                status, 429,
                "the original upstream status must be preserved"
            );
            // Exact match proves the body is the bounded fixed message, not
            // an echo of the 16 MiB raw upstream body.
            assert_eq!(body, "response body exceeded 16777216-byte cap");
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }

    let cap_warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.field("path").is_some())
        .collect();
    assert_eq!(cap_warns.len(), 1, "exactly one cap-trip WARN per trip");
    let w = cap_warns[0];
    assert_eq!(w.field("provider"), Some("test-provider"));
    assert_eq!(w.field("status"), Some("429"));
    assert_eq!(w.field("body_truncated"), Some("true"));
    assert_eq!(w.field("path"), Some("error_body"));
}

#[tokio::test]
async fn stream_error_body_over_cap_preserves_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_bytes(vec![b'a'; OVER_CAP_BODY_LEN]))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    // `stream()` yields a `BoxStream` that is not `Debug`, so `unwrap_err`
    // is unavailable; match the `Result` directly.
    let err = match provider.stream(user_request("gpt-4o")).await {
        Ok(_) => panic!("expected an upstream error, got a stream"),
        Err(e) => e,
    };

    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(
                status, 503,
                "the original upstream status must be preserved"
            );
            // Exact match proves the body is the bounded fixed message, not
            // an echo of the 16 MiB raw upstream body.
            assert_eq!(body, "response body exceeded 16777216-byte cap");
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Bedrock mantle lane wire behavior: SigV4/bearer-signed egress, no
// first-party Bearer, a no-redirect client, and the deterministic 501
// count_tokens capability signal. These pin the runtime lane against a mock
// upstream; the credential-scope and URL-builder units live in
// `routectl-providers/src/mantle.rs`.
// ---------------------------------------------------------------------------

#[cfg(feature = "bedrock")]
mod mantle {
    use super::*;
    use routectl_core::Error;
    use routectl_providers::anthropic_api::MantleAuth;
    use routectl_providers::bedrock::BedrockCreds;
    use routectl_providers::bedrock::auth::resolve;
    use routectl_providers::openai_compat::HistoryReasoning;
    use serde_json::Value;

    /// A mantle-lane compat provider posting to `base_url` with a resolved
    /// credential. `base_url` points at wiremock (the factory derives the
    /// real host from the region; here we still sign under the region
    /// scope). The `api_key` is empty, mirroring the config-validation
    /// invariant on the lane.
    async fn mantle_provider(base_url: &str, creds: BedrockCreds) -> OpenAiCompatProvider {
        let resolved = resolve(&creds, "us-west-2").await.unwrap();
        OpenAiCompatProvider::new(OpenAiCompatConfig {
            id: "mantle-compat".into(),
            base_url: base_url.to_string(),
            api_key: String::new(),
            header_extras: vec![],
            payload_extras: None,
            reasoning_dialect: ReasoningDialect::OpenAi,
            history_reasoning: HistoryReasoning::Auto,
            user_agent: None,
            strict_translation: false,
            disable_stream_include_usage: false,
            mantle: Some(MantleAuth {
                region: "us-west-2".into(),
                creds: resolved,
            }),
        })
    }

    fn bearer_creds() -> BedrockCreds {
        BedrockCreds::BearerKey {
            key: "mantle-bearer-key".into(),
        }
    }

    fn sigv4_creds() -> BedrockCreds {
        BedrockCreds::Static {
            access_key: "AKIAmantlewire000000".into(),
            secret_key: "mantle-wire-secret-key".into(),
            session_token: None,
        }
    }

    fn ok_completion() -> serde_json::Value {
        json!({
            "id": "chatcmpl-mantle",
            "model": "anthropic.claude-haiku-4-5",
            "created": 1700000000_i64,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
    }

    /// complete() on the mantle lane with bearer creds signs the request as
    /// `Authorization: Bearer <mantle-key>` and never attaches a stray
    /// first-party Bearer (the empty `api_key` is not stamped).
    #[tokio::test]
    async fn complete_bearer_lane_is_signed_with_no_first_party_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let auth = received[0]
            .headers
            .get("authorization")
            .expect("mantle lane must attach Authorization")
            .to_str()
            .unwrap();
        assert_eq!(
            auth, "Bearer mantle-bearer-key",
            "bearer creds must sign as the mantle key, never an empty first-party Bearer"
        );
        // The signed body is real JSON bytes (SigV4 requires a hashable body).
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(body.get("model").is_some(), "signed body reached the wire");
    }

    /// stream() on the mantle lane with bearer creds is signed and carries
    /// no first-party Bearer. The request is sent (and thus signed) by the
    /// time `stream()` returns, so the wire assertion holds without draining.
    #[tokio::test]
    async fn stream_bearer_lane_is_signed_with_no_first_party_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let _stream = provider
            .stream(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(
            received[0]
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer mantle-bearer-key"),
            "mantle stream() must be signed with the mantle key"
        );
    }

    /// complete() on the SigV4 lane signs with an `AWS4-HMAC-SHA256`
    /// Authorization scoped to `.../us-west-2/bedrock-mantle/aws4_request`,
    /// stamps `x-amz-date`, and carries the bare model id on the wire.
    #[tokio::test]
    async fn sigv4_lane_signs_wire_with_mantle_service_scope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), sigv4_creds()).await;
        provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let auth = received[0]
            .headers
            .get("authorization")
            .expect("SigV4 lane must attach Authorization")
            .to_str()
            .unwrap();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 "),
            "SigV4 lane must sign with AWS4-HMAC-SHA256; got {auth}"
        );
        assert!(
            auth.contains("/us-west-2/bedrock-mantle/aws4_request"),
            "credential scope must name the mantle service under the lane region; got {auth}"
        );
        assert!(
            received[0].headers.get("x-amz-date").is_some(),
            "SigV4 lane must stamp x-amz-date"
        );
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(
            body.get("model").and_then(Value::as_str),
            Some("anthropic.claude-haiku-4-5"),
            "the bare model id must reach the wire body verbatim"
        );
    }

    /// The mantle lane uses a no-redirect client: a 3xx is surfaced as an
    /// upstream failure and NEVER followed to its `Location` target.
    #[tokio::test]
    async fn mantle_lane_does_not_follow_redirects() {
        let server = MockServer::start().await;
        let redirect_target = format!("{}/redirected", server.uri());
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", redirect_target.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/redirected"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let err = provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap_err();
        // A 302 is surfaced as an upstream error, not chased cross-host.
        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 302),
            other => panic!("expected Upstream, got: {other:?}"),
        }

        let received = server.received_requests().await.unwrap();
        let followed = received
            .iter()
            .filter(|r| r.url.path() == "/redirected")
            .count();
        assert_eq!(
            followed, 0,
            "no-redirect client must not follow the 302 to its Location target"
        );
    }

    /// count_tokens on a mantle-configured compat provider stays the
    /// deterministic trait-default 501 (`Error::NotImplemented`): the router
    /// never walks compat for token counting, so the lane must not dial the
    /// signed endpoint for it.
    #[tokio::test]
    async fn count_tokens_is_not_implemented_on_mantle_lane() {
        let provider = mantle_provider("https://unused.invalid", bearer_creds()).await;
        let err = provider
            .count_tokens(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::NotImplemented(_, _)),
            "count_tokens on the mantle compat lane must stay a deterministic 501; got {err:?}"
        );
    }

    /// A bearer mantle credential is a static secret, so the credential
    /// probe reports `Reachable` without dialing the inference host.
    #[tokio::test]
    async fn probe_resolves_credential_without_dialing() {
        let provider = mantle_provider("https://unused.invalid", bearer_creds()).await;
        assert!(
            matches!(
                provider.probe().await,
                routectl_core::ProbeOutcome::Reachable
            ),
            "a bearer mantle credential must probe Reachable without a network dial"
        );
    }

    /// End-to-end AWS 403 on the wire: the ARN-laden AccessDenied body lifts
    /// the AWS exception token, scrubs the client body to the IAM action only
    /// (no principal ARN / account id), and classifies as `FailureClass::Auth`.
    /// The reader units live in `openai_compat/mod.rs`; this pins the full
    /// runtime lane against a mock upstream.
    #[tokio::test]
    async fn aws_403_lifts_token_scrubs_body_and_classifies_auth() {
        let server = MockServer::start().await;
        let body = r#"{"__type":"com.amazonaws.bedrock#AccessDeniedException","message":"User: arn:aws:iam::123456789012:role/App is not authorized to perform: bedrock-runtime:InvokeModel on resource: arn:aws:bedrock:us-west-2::foundation-model/anthropic.claude-haiku-4-5"}"#;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(403).set_body_string(body))
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let err = provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap_err();

        match &err {
            Error::Upstream {
                status,
                upstream_type,
                body,
                ..
            } => {
                assert_eq!(*status, 403);
                assert_eq!(upstream_type.as_deref(), Some("AccessDeniedException"));
                assert!(!body.contains("arn:aws:"), "client body leaked ARN: {body}");
                assert!(
                    !body.contains("123456789012"),
                    "client body leaked account id: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
        assert_eq!(
            routectl_core::failure_class::classify(&err, Some("openai-compat")).class,
            routectl_core::failure_class::FailureClass::Auth,
            "a mantle 403 must classify as Auth"
        );
    }

    /// End-to-end AWS 429 on the wire: the `Retry-After` reset hint is
    /// preserved on the canonical error and the bare AWS throttling `code`
    /// token is lifted.
    #[tokio::test]
    async fn aws_429_preserves_retry_after_and_lifts_code() {
        let server = MockServer::start().await;
        let body = r#"{"code":"ThrottlingException","Message":"Too many requests"}"#;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let err = provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap_err();

        match err {
            Error::Upstream {
                status,
                retry_after,
                upstream_code,
                ..
            } => {
                assert_eq!(status, 429);
                assert_eq!(
                    retry_after,
                    Some(std::time::Duration::from_secs(30)),
                    "the Retry-After reset hint must be preserved"
                );
                assert_eq!(upstream_code.as_deref(), Some("ThrottlingException"));
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }
}
