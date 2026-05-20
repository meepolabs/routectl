//! Integration tests for the per-model `[models.X] thinking` /
//! `enabled` operator-side reasoning defaults. End-to-end through the
//! axum server + a wiremock upstream that captures the routed request
//! body so we can verify what reaches each provider's wire shape.
//!
//! What's covered:
//!   - Anthropic legacy thinking shape: `thinking = "high"` + the
//!     caller's `max_tokens` produce a proportional `budget_tokens`
//!     on the upstream body.
//!   - Anthropic adaptive thinking shape: `thinking = "xhigh"` +
//!     `adaptive_thinking = true` produces `thinking.type =
//!     "adaptive"` and lifts the effort string into top-level
//!     `output_config.effort`.
//!   - OpenAI Responses: `thinking = "high"` produces `reasoning =
//!     {effort: "high", summary: "auto"}` on the body the chatgpt-oauth
//!     endpoint receives.
//!   - vLLM dialect: `enabled = true` injects
//!     `chat_template_kwargs.enable_thinking = true`.
//!   - Caller precedence: a caller-supplied `thinking.budget_tokens`
//!     wins over the operator's `thinking = "high"` default.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{
    AliasValue, Config, EffortLevel, ModelEntry, ProviderEntry, ReasoningDialect, RetryPolicy,
    ServerConfig, ThinkingChoice,
};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod helpers {
    use std::sync::Arc;

    use routectl_router::Config;
    use tokio::net::TcpListener;

    pub async fn spawn(config: Arc<Config>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            routectl_cli::server::serve_on_listener(config, listener)
                .await
                .expect("server failed");
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        base_url
    }
}

// ---------------------------------------------------------------------------
// Test fixtures shared across cases
// ---------------------------------------------------------------------------

fn anthropic_response_body() -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 1}
    })
}

fn openai_compat_response_body() -> Value {
    json!({
        "id": "chatcmpl-01",
        "object": "chat.completion",
        "created": 0,
        "model": "qwen3-30b",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
}

fn empty_server() -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: None,
        strict_translation: false,
        allow_disable_fallbacks: true,
    }
}

fn parse_effort(s: &str) -> EffortLevel {
    match s {
        "minimal" => EffortLevel::Minimal,
        "low" => EffortLevel::Low,
        "medium" => EffortLevel::Medium,
        "high" => EffortLevel::High,
        "xhigh" => EffortLevel::Xhigh,
        "max" => EffortLevel::Max,
        _ => panic!("unsupported effort token in test fixture: {s}"),
    }
}

/// Build a config with one Anthropic-API provider and one model whose
/// `[models.X]` carries the operator-side reasoning defaults.
/// `adaptive` toggles the Opus 4.7+ wire shape on the model.
fn anthropic_config_with_defaults(
    upstream_base: &str,
    thinking: &str,
    adaptive: bool,
) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    let entry =
        ProviderEntry::anthropic_api("literal:test-key").with_base_url(upstream_base.to_string());
    providers.insert("anthropic-mock".to_string(), entry);

    let mut model =
        ModelEntry::new("anthropic-mock", "claude-haiku-4-5").with_effort(parse_effort(thinking));
    model = if adaptive {
        model.with_thinking(ThinkingChoice::Adaptive)
    } else {
        model.with_thinking(ThinkingChoice::Bool(true))
    };
    let mut models = BTreeMap::new();
    models.insert("haiku".to_string(), model);

    let mut aliases = BTreeMap::new();
    aliases.insert("heavy".to_string(), AliasValue::Single("haiku".into()));

    Arc::new(Config {
        server: empty_server(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
    })
}

/// Build a config with one openai-compat provider on the vllm dialect
/// and one model whose `[models.X] enabled` (reasoning) is set.
/// Reasoning's `enabled` is now reachable from TOML in rc.2+ since
/// the outer ModelEntry selectability flag was renamed to
/// `selectable` to free the key. We still go through the builder
/// API here for parity with the rest of the test fixture surface.
fn vllm_config_with_enabled(upstream_base: &str, enabled: bool) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    let entry = ProviderEntry::openai_compat(upstream_base.to_string(), "literal:test-key");
    providers.insert("vllm-mock".to_string(), entry);

    // v0.6.0: `thinking = true` -> ReasoningDefaults::enabled = Some(true).
    // The model's reasoning_dialect must also be vllm so the egress
    // injects chat_template_kwargs.enable_thinking on the wire.
    let model = ModelEntry::new("vllm-mock", "qwen3-30b")
        .with_thinking(ThinkingChoice::Bool(enabled))
        .with_reasoning_dialect(ReasoningDialect::Vllm);
    let mut models = BTreeMap::new();
    models.insert("qwen".to_string(), model);

    let mut aliases = BTreeMap::new();
    aliases.insert("heavy".to_string(), AliasValue::Single("qwen".into()));

    Arc::new(Config {
        server: empty_server(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Case (a): Anthropic legacy thinking shape derives budget from default
// ---------------------------------------------------------------------------

/// Pin: `[models.X] thinking = "high"` on a non-adaptive Anthropic
/// model produces the legacy wire shape `thinking.type = "enabled"`
/// with `budget_tokens = floor(max_tokens * 0.80)`. Caller sent no
/// reasoning fields at all.
#[tokio::test]
async fn anthropic_default_thinking_high_reaches_upstream_legacy_shape() {
    // Arrange
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_config_with_defaults(&upstream.uri(), "high", false);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        // Sized above the Anthropic legacy-thinking floor
        // (`max_tokens > 1024`) so the gate in `build_thinking` keeps
        // the Enabled shape on a probe-sized request. See
        // `small_max_tokens_drops_legacy_thinking` for the dropped
        // case.
        "max_tokens": 2048,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }],
    });

    // Act
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Assert
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["thinking"]["type"], "enabled",
        "expected legacy thinking shape; got body: {upstream_body}"
    );
    // 2048 * 0.80 = 1638.4 -> u32 truncation -> 1638. Above the
    // Anthropic floor (1024) so no clamp fires here.
    assert_eq!(
        upstream_body["thinking"]["budget_tokens"], 1638,
        "operator default thinking=high must derive budget from caller max_tokens"
    );
}

// ---------------------------------------------------------------------------
// Case (b): Anthropic adaptive thinking shape lifts effort to output_config
// ---------------------------------------------------------------------------

/// Pin: `[models.X] thinking = "xhigh"` on an adaptive model
/// (Opus 4.7+) produces the adaptive wire shape (`thinking.type =
/// "adaptive"` with no budget field) AND lifts the effort string into
/// top-level `output_config.effort`.
#[tokio::test]
async fn anthropic_adaptive_default_thinking_xhigh_emits_output_config() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_config_with_defaults(&upstream.uri(), "xhigh", true);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 4096,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }],
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["thinking"]["type"], "adaptive",
        "expected adaptive thinking wire shape; got body: {upstream_body}"
    );
    assert!(
        upstream_body["thinking"].get("budget_tokens").is_none(),
        "adaptive shape must NOT carry budget_tokens; got: {}",
        upstream_body["thinking"]
    );
    assert_eq!(
        upstream_body["output_config"]["effort"], "xhigh",
        "operator default thinking=xhigh must lift to output_config.effort on adaptive"
    );
}

// ---------------------------------------------------------------------------
// Case (c): OpenAI Responses default thinking emits reasoning block
// ---------------------------------------------------------------------------

/// Pin: `[models.X] thinking = "high"` on a model bound to an OpenAI
/// Responses provider emits `reasoning = {effort: "high", summary:
/// "auto"}` on the upstream body. The chatgpt-oauth endpoint is
/// stream-only so the mock returns an SSE `response.completed` event.
#[tokio::test]
async fn openai_responses_default_thinking_high_emits_reasoning_block() {
    let upstream = MockServer::start().await;
    let completed_body = json!({
        "id": "resp_01",
        "object": "response",
        "status": "completed",
        "model": "gpt-5.3-codex",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "ok"}],
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
    });
    let event_body = format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed_body).unwrap()
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event_body),
        )
        .mount(&upstream)
        .await;

    // Build the OpenaiResponses provider via TOML deserialization
    // because the variant is `#[non_exhaustive]` and the routectl-cli
    // test crate (external to routectl-router) cannot use struct-
    // literal syntax. The factory rejects an absent `account_id_ref`
    // for `auth_kind = "chatgpt-oauth"`, so supply a placeholder UUID.
    let toml_text = format!(
        r#"
kind = "openai-responses"
api_key_ref = "literal:test-jwt"
account_id_ref = "literal:00000000-0000-0000-0000-000000000000"
base_url = "{upstream_uri}"
auth_kind = "chatgpt-oauth"
"#,
        upstream_uri = upstream.uri()
    );
    let entry: ProviderEntry =
        toml::from_str(&toml_text).expect("openai-responses entry should parse");

    let mut providers = BTreeMap::new();
    providers.insert("gpt-mock".to_string(), entry);

    let mut model = ModelEntry::new("gpt-mock", "gpt-5.3-codex")
        .with_thinking(ThinkingChoice::Bool(true))
        .with_effort(EffortLevel::High);
    let mut models = BTreeMap::new();
    models.insert("codex".to_string(), model.clone());
    let _ = &mut model;

    let mut aliases = BTreeMap::new();
    aliases.insert("heavy".to_string(), AliasValue::Single("codex".into()));

    let config = Arc::new(Config {
        server: empty_server(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
    });

    let base = helpers::spawn(config).await;

    // Use the OpenAI Chat Completions ingress so we can drive the
    // openai-responses egress without the Anthropic ingress in the
    // path. The body is a plain chat completions request -- routectl
    // translates internally to the Responses wire shape.
    let body = json!({
        "model": "heavy",
        "messages": [{"role": "user", "content": "hello"}],
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "ingress rejected: {} body: {}",
        resp.status(),
        resp.text().await.unwrap_or_default(),
    );

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["reasoning"]["effort"], "high",
        "operator default thinking=high must emit reasoning.effort=high; got body: {upstream_body}"
    );
    assert_eq!(
        upstream_body["reasoning"]["summary"], "auto",
        "openai-responses egress always pairs effort with summary=auto"
    );
}

// ---------------------------------------------------------------------------
// Case (d): vLLM default enabled=true emits chat_template_kwargs
// ---------------------------------------------------------------------------

/// Pin: `[models.X] enabled = true` on a vLLM-dialect openai-compat
/// model injects `chat_template_kwargs.enable_thinking = true` on
/// the upstream body. Caller sent no reasoning fields.
#[tokio::test]
async fn vllm_default_enabled_true_emits_chat_template_kwargs() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_compat_response_body()))
        .mount(&upstream)
        .await;

    let config = vllm_config_with_enabled(&upstream.uri(), true);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "messages": [{"role": "user", "content": "hello"}],
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["chat_template_kwargs"]["enable_thinking"],
        json!(true),
        "operator default enabled=true must inject enable_thinking; got body: {upstream_body}"
    );
}

// ---------------------------------------------------------------------------
// Case (e): caller reasoning overrides operator default
// ---------------------------------------------------------------------------

/// Pin: caller-supplied `thinking.budget_tokens` on the wire wins over
/// the operator's `thinking = "high"` default. The egress must emit
/// the caller's exact budget (2048), NOT a budget derived from the
/// operator default (which would be ~3276 for `high` at max_tokens=
/// 4096).
#[tokio::test]
async fn caller_reasoning_overrides_provider_default_high() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_config_with_defaults(&upstream.uri(), "high", false);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 4096,
        // Caller's budget must sit above Anthropic's 1024 floor so
        // the per-arm clamp does not mask the "caller wins"
        // assertion. The sub-floor variant is covered by the unit
        // test `small_max_tokens_drops_thinking_with_explicit_budget`.
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }],
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["thinking"]["type"], "enabled",
        "caller asked for legacy enabled shape; got body: {upstream_body}"
    );
    assert_eq!(
        upstream_body["thinking"]["budget_tokens"], 2048,
        "caller's budget_tokens=2048 must win over operator default thinking=high"
    );
}

// ---------------------------------------------------------------------------
// Case (f): fallback chain applies per-hop reasoning defaults
// ---------------------------------------------------------------------------

/// Pin: a fallback chain with two models carrying different
/// `[models.X] thinking` defaults applies the SECOND model's default
/// on the second hop. The merge happens per-attempt inside the
/// router, AFTER `attempt_req = req.clone()`, so the first model's
/// mutation cannot bleed into the second model's body.
///
/// Setup: model-a returns 500 with `thinking = "low"`, model-b
/// returns 200 with `thinking = "high"`. The client sends one request
/// with `max_tokens = 8000` (sized so both effort levels produce
/// budgets above the 1024 floor; using 1024 would cause both hops to
/// clamp to the same value and erase the per-hop distinction the
/// test exists to pin). Assert:
///   - Upstream A received `budget_tokens = 1600` (8000 * 0.20).
///   - Upstream B received `budget_tokens = 6400` (8000 * 0.80).
#[tokio::test]
async fn fallback_chain_applies_per_hop_reasoning_defaults() {
    // Arrange: two upstream wiremocks, A 500 / B 200.
    let upstream_a = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream A failure"))
        .mount(&upstream_a)
        .await;

    let upstream_b = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream_b)
        .await;

    // Build two anthropic-api providers + two models with different
    // reasoning defaults. model-a -> "low", model-b -> "high".
    let mut providers = BTreeMap::new();
    providers.insert(
        "provider-a".to_string(),
        ProviderEntry::anthropic_api("literal:test-key").with_base_url(upstream_a.uri()),
    );
    providers.insert(
        "provider-b".to_string(),
        ProviderEntry::anthropic_api("literal:test-key").with_base_url(upstream_b.uri()),
    );

    let mut models = BTreeMap::new();
    models.insert(
        "model-a".to_string(),
        ModelEntry::new("provider-a", "claude-haiku-4-5")
            .with_thinking(ThinkingChoice::Bool(true))
            .with_effort(EffortLevel::Low),
    );
    models.insert(
        "model-b".to_string(),
        ModelEntry::new("provider-b", "claude-haiku-4-5")
            .with_thinking(ThinkingChoice::Bool(true))
            .with_effort(EffortLevel::High),
    );

    // Alias chain: model-a first (returns 500 -> fallback), then model-b (200).
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "heavy".to_string(),
        AliasValue::Chain(vec!["model-a".into(), "model-b".into()]),
    );

    // Tighten the retry policy so A only sees one attempt before the
    // router walks to B. Default `max_attempts = 2` would otherwise
    // make A receive two requests before fallback, doubling the
    // received-request count without changing the assertion.
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;

    let config = Arc::new(Config {
        server: empty_server(),
        providers,
        aliases,
        retry,
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
    });
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 8000,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }],
    });

    // Act
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "fallback should succeed via provider-b");

    // Assert: A received the "low"-derived budget, B received the
    // "high"-derived budget. If the merge were aliasing across hops,
    // both upstreams would see the same value.
    let received_a = upstream_a.received_requests().await.unwrap();
    assert_eq!(
        received_a.len(),
        1,
        "model-a should receive exactly one attempt before fallback"
    );
    let body_a: Value = serde_json::from_slice(&received_a[0].body).unwrap();
    assert_eq!(
        body_a["thinking"]["budget_tokens"], 1600,
        "model-a's `thinking = low` must derive budget_tokens=1600 (8000*0.20); got body: {body_a}"
    );

    let received_b = upstream_b.received_requests().await.unwrap();
    assert_eq!(
        received_b.len(),
        1,
        "model-b should receive the fallback attempt"
    );
    let body_b: Value = serde_json::from_slice(&received_b[0].body).unwrap();
    assert_eq!(
        body_b["thinking"]["budget_tokens"], 6400,
        "model-b's `thinking = high` must derive budget_tokens=6400 (8000*0.80) \
         (no bleed-through from model-a); got body: {body_b}"
    );
}

// ---------------------------------------------------------------------------
// Case (g): streaming path applies operator reasoning defaults
// ---------------------------------------------------------------------------

/// Pin: the merge fires on the streaming path too, not just the
/// non-streaming complete path. Same shape as case (a) but with
/// `stream: true` on the request and an SSE response body. The
/// reasoning-defaults merge happens in `stream_with_options` after
/// `attempt_req = req.clone()`, so the upstream-captured body must
/// carry the operator's `thinking = "high"` (legacy
/// budget_tokens=1638 for max_tokens=2048).
#[tokio::test]
async fn streaming_default_thinking_high_reaches_upstream() {
    // Arrange: Anthropic-shape SSE response. message_start +
    // content_block_start/delta/stop + message_delta + message_stop is
    // the minimum frame sequence the SseState machine needs to drive
    // a clean stream end-to-end. Each `data:` line is its own JSON
    // payload (no `event:` line needed; the eventsource decoder pulls
    // event_type out of the JSON body).
    let upstream = MockServer::start().await;
    let sse_body = "\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-haiku-4-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&upstream)
        .await;

    let config = anthropic_config_with_defaults(&upstream.uri(), "high", false);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 2048,
        "stream": true,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }],
    });

    // Act
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "streaming ingress rejected; body: {}",
        resp.text().await.unwrap_or_default(),
    );
    // Drain the SSE response so the stream completes cleanly before
    // the wiremock assertion below. Without this, the request future
    // can be dropped before the upstream sees its body in some racy
    // builds.
    let _ = resp.bytes().await.unwrap();

    // Assert: upstream-captured body has the operator's thinking
    // applied via the streaming path. Same numeric pin as case (a):
    // 2048 * 0.80 -> 1638.
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["stream"],
        json!(true),
        "egress must propagate stream=true to the upstream; got body: {upstream_body}"
    );
    assert_eq!(
        upstream_body["thinking"]["type"], "enabled",
        "expected legacy thinking shape on streaming path; got body: {upstream_body}"
    );
    assert_eq!(
        upstream_body["thinking"]["budget_tokens"], 1638,
        "operator default thinking=high must reach upstream on streaming path \
         (1024 * 0.80 -> 819); got body: {upstream_body}"
    );
}
