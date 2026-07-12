//! Contract tests for the streaming-side egress layer.
//!
//! Each scenario serves a canned SSE body from a `wiremock::MockServer`,
//! drives the provider's `stream()` method, and asserts the canonical
//! `ChatChunk` sequence with explicit field checks (NOT snapshots --
//! SSE field ordering inside the JSON Map is non-deterministic and a
//! snapshot would be flaky).
//!
//! This file pairs with `contract_stream_ingress.rs` in the
//! `routectl-cli` crate (the canonical-to-wire half). It also pairs
//! with the existing per-provider stream tests in `anthropic_api.rs`
//! and `openai_compat.rs`: those exercise the full stream surface;
//! this contract file pins ONLY the two bug classes the v0.6 contract
//! suite must guard against:
//!
//!   - Bug B class: `message_delta`/`message_stop` ordering on the
//!     Anthropic egress (canonical chunks have a usage trailer; the
//!     egress must emit `message_delta + message_stop` exactly once at
//!     the end, not duplicate them).
//!   - Bug G class: post-`[DONE]` SSE trailer on openai-compat hosts
//!     (some hosts emit a bookkeeping chunk after `[DONE]`; the
//!     parser must ignore it, not try to JSON-decode `[DONE]` or the
//!     trailer).
//!
//! Scope: only `anthropic_api` and `openai_compat` providers are
//! covered. Bedrock (Invoke + Converse) and `openai_responses`
//! streaming coverage land in a follow-up PR.

#![cfg(all(feature = "anthropic-api", feature = "openai-compat"))]

use futures::StreamExt;
use routectl_core::{ChatChunk, ChatRequest, Message, MessageContent, Provider, Role};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------
// Provider + request builders
// ---------------------------------------------------------------------
//
// Duplicated from `contract_egress.rs` and `contract_response_egress.rs`:
// the shared `common/mod.rs` only carries canonical-shape scenario
// builders, not provider constructors or wire-body SSE strings.
// Keeping the two-line constructors local avoids a `pub mod providers`
// carve-out for one more caller.

fn anthropic_api_provider(base_url: &str) -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "anthropic-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: base_url.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
    })
}

fn openai_compat_provider(base_url: &str) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat-test".into(),
        base_url: base_url.into(),
        api_key: "test-key".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
    })
}

fn stream_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("Hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: Some(64),
        stream: Some(true),
        ..Default::default()
    }
}

/// Drain a provider stream into a `Vec<ChatChunk>`. Surfaces any
/// per-chunk `Err` as a test panic so the assertions below operate on
/// only the successful canonical chunks (any error is itself a
/// contract violation).
async fn collect_chunks<P: Provider + ?Sized>(provider: &P, req: ChatRequest) -> Vec<ChatChunk> {
    let mut stream = provider.stream(req).await.expect("stream open");
    let mut out: Vec<ChatChunk> = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.expect("stream chunk decoded without error"));
    }
    out
}

/// Collect every `delta.content` text fragment across all chunks, in
/// arrival order. Used by scenarios 7 + 8 to assert content
/// reassembly.
fn join_delta_text(chunks: &[ChatChunk]) -> String {
    let mut s = String::new();
    for c in chunks {
        for ch in &c.choices {
            if let Some(t) = ch.delta.content.as_deref() {
                s.push_str(t);
            }
        }
    }
    s
}

/// First non-empty `finish_reason` observed across chunks, in arrival
/// order. The egress must surface a terminal `finish_reason` (either
/// inline with the last content chunk or on a trailing usage-only
/// chunk).
fn first_finish_reason(chunks: &[ChatChunk]) -> Option<String> {
    for c in chunks {
        for ch in &c.choices {
            if let Some(fr) = ch.finish_reason.as_deref() {
                return Some(fr.to_string());
            }
        }
    }
    None
}

// =====================================================================
// Scenario 7: basic_stream_sequence
// =====================================================================
//
// Bug B class guard. Both egresses must consume their native canned
// SSE body and produce a canonical chunk sequence with:
//   - text deltas in arrival order ("Hello", " world" -> "Hello world")
//   - terminal `finish_reason: "stop"`
//   - usage surfaced on at least one chunk (Anthropic emits usage on
//     the `message_delta` event; the egress lifts it to the
//     terminal canonical chunk)

mod scenario_7_basic_stream_sequence {
    use super::*;

    /// Anthropic-shape SSE body covering the full happy-path event
    /// sequence: message_start, content_block_start, two
    /// content_block_delta frames, content_block_stop, message_delta
    /// (carrying stop_reason + usage), message_stop. Mirrors what
    /// real api.anthropic.com emits for a two-token completion.
    const fn anthropic_sse_body() -> &'static str {
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s7\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-opus\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
    }

    /// OpenAI-shape SSE body covering a minimal stream: two content
    /// deltas + a terminal finish_reason chunk + `[DONE]` sentinel.
    /// Mirrors the openai.com / OpenRouter chat-completions stream
    /// shape used by all openai-compat hosts.
    const fn openai_sse_body() -> &'static str {
        concat!(
            "data: {\"id\":\"chunk-s7\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chunk-s7\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
    }

    #[tokio::test]
    async fn anthropic_api_egress() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_sse_body()),
            )
            .mount(&server)
            .await;

        let provider = anthropic_api_provider(&server.uri());
        let chunks = collect_chunks(&provider, stream_request("claude-3-opus")).await;

        // Exact chunk-count pin. Bug B class is famous for emitting
        // duplicate message_delta / message_stop pairs; without this
        // assertion, a regression that doubled the terminal chunk
        // would still produce "Hello world" + finish_reason "stop"
        // and slip past the field-shape assertions below.
        // Expected mapping from the canned SSE body:
        //   message_start         -> 0 (id/model captured for later chunks)
        //   content_block_start   -> 0 (block opens)
        //   content_block_delta x2 -> 2 (one per text fragment)
        //   content_block_stop    -> 0 (block closes)
        //   message_delta         -> 1 (carries finish_reason + usage)
        //   message_stop          -> 0 (end-of-stream marker)
        assert_eq!(
            chunks.len(),
            3,
            "anthropic egress must emit exactly 3 chunks for this fixture (2 deltas + terminal); chunks: {chunks:?}"
        );
        // Text deltas reassemble in order.
        assert_eq!(
            join_delta_text(&chunks),
            "Hello world",
            "anthropic egress must surface content_block_delta text in order; chunks: {chunks:?}"
        );
        // The terminal chunk MUST carry BOTH finish_reason AND usage
        // -- the Anthropic wire emits both on a single `message_delta`
        // event, and they must arrive coupled on the canonical side
        // so the ingress's render_chunk can synthesize one terminal
        // event downstream. A regression that splits them across
        // separate canonical chunks would pass `first_finish_reason`
        // + `any(usage)` style checks, so the assertion is tight on
        // a single chunk index (the last).
        let terminal = chunks.last().expect("at least one chunk");
        assert_eq!(
            terminal.choices[0].finish_reason.as_deref(),
            Some("stop"),
            "anthropic egress terminal chunk must carry finish_reason `stop` (from wire end_turn)"
        );
        assert!(
            terminal.usage.is_some(),
            "anthropic egress terminal chunk must carry usage (from message_delta); terminal: {terminal:?}"
        );
        // Non-terminal chunks must NOT carry finish_reason or usage
        // -- those belong only on the terminal. Catches regressions
        // that bleed terminal fields onto earlier deltas.
        for (i, c) in chunks.iter().enumerate().take(chunks.len() - 1) {
            assert!(
                c.choices[0].finish_reason.is_none(),
                "chunk {i} is non-terminal but carries finish_reason: {c:?}"
            );
            assert!(
                c.usage.is_none(),
                "chunk {i} is non-terminal but carries usage: {c:?}"
            );
        }
    }

    #[tokio::test]
    async fn openai_compat_egress() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(openai_sse_body()),
            )
            .mount(&server)
            .await;

        let provider = openai_compat_provider(&server.uri());
        let chunks = collect_chunks(&provider, stream_request("gpt-4o")).await;

        // Exact chunk-count pin: the canned SSE body has 2 data
        // frames before [DONE], so exactly 2 canonical chunks.
        assert_eq!(
            chunks.len(),
            2,
            "openai-compat egress must emit exactly 2 chunks for this fixture; chunks: {chunks:?}"
        );
        assert_eq!(
            join_delta_text(&chunks),
            "Hello world",
            "openai-compat egress must surface delta.content text in order; chunks: {chunks:?}"
        );
        // OpenAI-compat is canonical-shape passthrough on
        // finish_reason; `stop` arrives on the final pre-DONE chunk.
        assert_eq!(
            first_finish_reason(&chunks).as_deref(),
            Some("stop"),
            "openai-compat egress must surface terminal finish_reason `stop`"
        );
        // The egress now auto-injects `stream_options.include_usage = true`
        // on streaming requests so the upstream emits a terminal usage
        // chunk + finish_reason. Pin the wire-body by reading the
        // captured request body off the mock server.
        let received = server.received_requests().await.expect("received requests");
        assert_eq!(received.len(), 1, "exactly one upstream request expected");
        let body: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("body parses as JSON");
        assert_eq!(
            body.pointer("/stream_options/include_usage"),
            Some(&serde_json::Value::Bool(true)),
            "egress must auto-inject stream_options.include_usage = true on streaming requests; body: {body}"
        );
    }
}

// =====================================================================
// Scenario 8: post_done_trailer (Bug G class)
// =====================================================================
//
// Some openai-compat hosts (DeepSeek + various proxies) emit a
// bookkeeping `data: {...}` chunk AFTER the `[DONE]` sentinel.
// Pre-fix, the SSE parser would try to JSON-decode `[DONE]` or the
// trailer and yield an error. Post-fix, `[DONE]` is a hard stop --
// the trailer is silently ignored. Pin that behavior here so a
// regression surfaces immediately. Anthropic egress is skipped: the
// Anthropic wire shape has no `[DONE]` sentinel; its post-stream
// trailer guard lives in
// `anthropic_api.rs::integration_stream_handles_trailing_done_sentinel`.

mod scenario_8_post_done_trailer {
    use super::*;

    /// OpenAI-shape SSE body with a `cost:"0"` bookkeeping chunk
    /// AFTER `[DONE]`. The chunk has an empty `choices` array (no
    /// content), which would normally still parse fine, but the
    /// parser must stop at `[DONE]` regardless and not look at it.
    const fn openai_sse_body_with_post_done_trailer() -> &'static str {
        concat!(
            "data: {\"id\":\"chunk-s8\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
            "data: {\"choices\":[],\"cost\":\"0\"}\n\n",
        )
    }

    #[tokio::test]
    async fn openai_compat_egress() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(openai_sse_body_with_post_done_trailer()),
            )
            .mount(&server)
            .await;

        let provider = openai_compat_provider(&server.uri());
        let chunks = collect_chunks(&provider, stream_request("gpt-4o")).await;

        // The pre-DONE chunk carries `"hi"`. The post-DONE trailer
        // has an empty `choices` array -- if the parser kept reading
        // past `[DONE]`, it would yield an additional chunk here.
        // Pin: exactly one canonical chunk surfaces, carrying `"hi"`.
        assert_eq!(
            join_delta_text(&chunks),
            "hi",
            "post-DONE trailer must NOT surface as a canonical chunk; chunks: {chunks:?}"
        );
        assert_eq!(
            chunks.len(),
            1,
            "exactly one chunk expected (the pre-DONE content); post-DONE trailer ignored; got: {chunks:?}"
        );
        assert_eq!(
            first_finish_reason(&chunks).as_deref(),
            Some("stop"),
            "openai-compat egress must surface terminal finish_reason on pre-DONE chunk"
        );
    }
}

// =====================================================================
// stream_options auto-inject precedence (issue #4)
// =====================================================================
//
// The openai-compat egress defaults to injecting `stream_options.include_usage = true`
// on streaming requests so most upstreams emit a terminal usage chunk +
// finish_reason. The precedence rules:
//
//   - operator opt-out via `disable_stream_include_usage = true` -> no inject
//   - operator-supplied `stream_options` in `default_extras` /
//     `provider_extras` wins -- including an explicit `include_usage = false`.

#[tokio::test]
async fn openai_compat_egress_opt_out_suppresses_stream_options() {
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

    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat-test".into(),
        base_url: server.uri(),
        api_key: "test-key".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: true,
    });
    let _ = collect_chunks(&provider, stream_request("gpt-4o")).await;

    let received = server.received_requests().await.expect("received requests");
    assert_eq!(received.len(), 1, "exactly one upstream request expected");
    let body: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("body parses as JSON");
    assert!(
        body.get("stream_options").is_none(),
        "opt-out must suppress the auto-injected stream_options entirely; body: {body}"
    );
}

// =====================================================================
// Per-model header_extras reach the wire
// =====================================================================
//
// v0.6.0 promoted `header_extras` to both provider AND model levels.
// The router's dispatch layer composes the merged map into
// `ChatRequest.routectl_internal.header_extras`; the openai-compat
// egress's `build_headers` reads from there (with `self.cfg.header_extras`
// as a library-consumer fallback). This contract test pins the wire-side
// outcome: a model-level header set ONLY via the router-side carrier
// must appear on the upstream HTTP request, AND model values must win
// over provider values on key collision.
//
// Bug class caught: round 1 review of 8fb4699 surfaced that the original
// commit composed the merged map at dispatch but never published it to
// the egress -- only `anthropic-beta` reached the wire (via the
// canonical `req.anthropic_beta` lift). All other model-level headers
// were silently discarded.

#[tokio::test]
async fn openai_compat_per_model_header_extras_reach_wire() {
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

    // Provider sets one base header; the router-composed map (which
    // mirrors what `Router::merge_header_extras` would land on the
    // request) sets the SAME key with a different value plus an
    // additional model-only key.
    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat-test".into(),
        base_url: server.uri(),
        api_key: "test-key".into(),
        header_extras: vec![("x-shared".into(), "provider-only".into())],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
    });

    // Simulate what the router would publish onto the request before
    // calling provider.stream(). Model wins on the shared key; the
    // model-only key joins the map.
    let mut req = stream_request("gpt-4o");
    let mut merged = std::collections::BTreeMap::new();
    merged.insert("x-shared".into(), "model-wins".into());
    merged.insert("x-model-only".into(), "from-model".into());
    req.routectl_internal.header_extras = Some(merged);

    let _ = collect_chunks(&provider, req).await;

    let received = server.received_requests().await.expect("received requests");
    assert_eq!(received.len(), 1, "exactly one upstream request expected");
    let hdrs = &received[0].headers;
    assert_eq!(
        hdrs.get("x-shared").map(|h| h.to_str().unwrap_or_default()),
        Some("model-wins"),
        "model-level header_extras must win over provider-level on key collision",
    );
    assert_eq!(
        hdrs.get("x-model-only")
            .map(|h| h.to_str().unwrap_or_default()),
        Some("from-model"),
        "model-only header_extras keys must reach the wire",
    );
}

#[tokio::test]
async fn openai_compat_egress_preserves_operator_supplied_stream_options() {
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

    // Operator explicitly opted out via default_extras with
    // `include_usage = false`. The auto-inject must not flip it to
    // true -- explicit operator config wins.
    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat-test".into(),
        base_url: server.uri(),
        api_key: "test-key".into(),
        header_extras: vec![],
        payload_extras: Some(serde_json::json!({
            "stream_options": {"include_usage": false}
        })),
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
    });
    let _ = collect_chunks(&provider, stream_request("gpt-4o")).await;

    let received = server.received_requests().await.expect("received requests");
    assert_eq!(received.len(), 1, "exactly one upstream request expected");
    let body: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("body parses as JSON");
    assert_eq!(
        body.pointer("/stream_options/include_usage"),
        Some(&serde_json::Value::Bool(false)),
        "operator-supplied include_usage=false must win over auto-inject; body: {body}"
    );
}
