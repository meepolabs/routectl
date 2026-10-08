//! Cross-lane pin for the canonical `response_format` shape contract.
//!
//! A native Responses client spells a schema directive FLAT
//! (`text.format = {"type":"json_schema","name":X,"schema":{...}}`). Canonical
//! is the NESTED OpenAI Chat-Completions shape, which the Responses ingress
//! produces by rewriting the flat form. Every egress reads (or, on
//! openai-compat, serializes) that one shape.
//!
//! This file drives the REAL ingress and asserts the directive reaches the
//! emitted body on the responses, openai-compat, anthropic-api, gemini and
//! bedrock-invoke egresses (bedrock-converse shares the anthropic-api reader). The
//! translation-drop census cannot catch a producer/consumer shape
//! disagreement -- it welds drop ARMS to counters, and a shape mismatch is
//! not an arm -- so the invariant is pinned per lane here instead.
//!
//! Four lanes drive the whole pipeline (axum server + wiremock upstream) and
//! assert on the body the upstream received. Bedrock has no `base_url` (its
//! endpoint is region-derived and its requests are SigV4-signed), so its lane
//! feeds the same real-ingress canonical request to the body shaper that
//! fires per request inside `BedrockProvider::complete`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::HeaderMap;
use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::openai_responses::ResponsesIngress;
use routectl_core::ChatRequest;
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
use routectl_providers::openai_responses::AuthKind as ResponsesAuthKind;
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

mod helpers {
    use std::sync::Arc;

    use routectl_router::Config;
    use tokio::net::TcpListener;

    pub async fn spawn(config: Arc<Config>) -> String {
        let config = crate::common::isolate_usage_db(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            routectl_cli::server::serve_on_listener(config, listener, None)
                .await
                .expect("server failed");
        });
        // Bounded readiness rather than a fixed sleep: a boot failure panics
        // in the detached task, and reqwest has no default timeout, so an
        // unbounded wait hangs the binary instead of failing it.
        crate::common::readiness::await_health(&base_url).await;
        base_url
    }
}

/// The schema property the directive constrains. Asserted on every lane
/// that carries a schema, so a lane emitting an envelope with the caller's
/// schema hollowed out fails rather than passing on the tag alone.
const SCHEMA_PROPERTY: &str = "marker_structured_field";

const SCHEMA_NAME: &str = "marker_schema_name";

fn base_server() -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: None,
        strict_translation: false,
        allow_disable_fallbacks: true,
        ..Default::default()
    }
}

/// A native Responses request body carrying the FLAT json_schema
/// `text.format` a Codex-style client sends.
fn flat_responses_body(model: &str) -> Value {
    json!({
        "model": model,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "answer as JSON"}]
        }],
        "text": {
            "format": {
                "type": "json_schema",
                "name": SCHEMA_NAME,
                "schema": {
                    "type": "object",
                    "properties": {SCHEMA_PROPERTY: {"type": "string"}},
                    "required": [SCHEMA_PROPERTY]
                },
                "strict": true
            }
        }
    })
}

/// A `text.format` tag no routectl reader recognizes -- the shape a newer
/// Responses format member arrives in -- with sibling keys of its own, so a
/// lane that keeps only the tag is told apart from one that keeps the object.
fn unknown_text_format() -> Value {
    json!({
        "type": "marker_future_format_tag",
        "marker_future_member": {"nested": [1, "two"]},
        "strict": true
    })
}

/// A native Responses request body whose `text.format` is `format`.
fn responses_body_with_format(model: &str, format: Value) -> Value {
    let mut body = flat_responses_body(model);
    body["text"]["format"] = format;
    body
}

/// The canonical request the REAL Responses ingress produces from `body` --
/// the same adapter the `POST /v1/responses` route drives.
fn canonical_from_real_ingress_body(body: &Value) -> ChatRequest {
    ResponsesIngress
        .parse_request(
            &HeaderMap::new(),
            &serde_json::to_vec(body).expect("fixture serializes"),
        )
        .expect("the fixture is a well-formed Responses body")
}

fn canonical_from_real_ingress() -> ChatRequest {
    canonical_from_real_ingress_body(&flat_responses_body("mock-model"))
}

fn openai_response_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "{}"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

fn anthropic_response_body() -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "{}"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 1}
    })
}

fn gemini_response_body() -> Value {
    json!({
        "candidates": [{
            "content": {"parts": [{"text": "{}"}], "role": "model"},
            "finishReason": "STOP",
            "index": 0
        }],
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 1,
            "totalTokenCount": 6
        },
        "modelVersion": "gemini-2.5-pro",
        "responseId": "resp-abc"
    })
}

/// The Responses upstream is drained as SSE (`complete` forces
/// `stream=true`), so the mock returns a single terminal event.
fn responses_sse_body() -> String {
    let completed = json!({
        "id": "resp_01",
        "object": "response",
        "status": "completed",
        "model": "mock-model",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "{}"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed).expect("completed body serializes")
    )
}

fn config_with(providers: BTreeMap<String, ProviderEntry>, model: ModelEntry) -> Arc<Config> {
    let mut models = BTreeMap::new();
    models.insert("target".to_string(), model);
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "mock-model".to_string(),
        AliasValue::Single("target".into()),
    );
    Arc::new(Config {
        server: base_server(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    })
}

/// POST the flat fixture at the real `/v1/responses` route and return the
/// body the upstream received.
async fn upstream_body_for(config: Arc<Config>, upstream: &MockServer) -> Value {
    upstream_body_for_request(config, upstream, &flat_responses_body("mock-model")).await
}

/// POST `request` at the real `/v1/responses` route and return the body the
/// upstream received.
async fn upstream_body_for_request(
    config: Arc<Config>,
    upstream: &MockServer,
    request: &Value,
) -> Value {
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(request)
        .send()
        .await
        .expect("ingress request sends");
    assert_eq!(resp.status(), 200, "ingress returned {}", resp.status());

    let received = upstream
        .received_requests()
        .await
        .expect("requests recorded");
    assert_eq!(received.len(), 1, "expected exactly one upstream call");
    serde_json::from_slice(&received[0].body).expect("upstream body is JSON")
}

// ---------------------------------------------------------------------------
// openai-responses: the same-dialect round trip -- flat in, flat out
// ---------------------------------------------------------------------------

/// A mock Responses upstream plus a config routing `mock-model` to it.
async fn responses_lane() -> (MockServer, Arc<Config>) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses_sse_body()),
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
    let config = config_with(providers, ModelEntry::new("responses-mock", "mock-model"));
    (upstream, config)
}

#[tokio::test]
async fn flat_responses_text_format_round_trips_to_the_responses_egress() {
    let (upstream, config) = responses_lane().await;

    let body = upstream_body_for(config, &upstream).await;

    // The egress flatten is the exact inverse of the ingress nesting, so
    // the directive arrives upstream carrying exactly the keys the client sent.
    assert_eq!(
        body["text"]["format"],
        json!({
            "type": "json_schema",
            "name": SCHEMA_NAME,
            "schema": {
                "type": "object",
                "properties": {SCHEMA_PROPERTY: {"type": "string"}},
                "required": [SCHEMA_PROPERTY]
            },
            "strict": true
        }),
        "Responses egress must round-trip the flat directive: {body}"
    );
}

/// A tag routectl does not recognize is the same-dialect upstream's to judge:
/// the whole format object, sibling keys included, arrives unchanged.
#[tokio::test]
async fn unknown_responses_text_format_tag_reaches_the_responses_egress_verbatim() {
    // Arrange
    let (upstream, config) = responses_lane().await;
    let request = responses_body_with_format("mock-model", unknown_text_format());

    // Act
    let body = upstream_body_for_request(config, &upstream, &request).await;

    // Assert
    assert_eq!(
        body["text"]["format"],
        unknown_text_format(),
        "Responses egress must forward an unknown text.format unchanged: {body}"
    );
}

// ---------------------------------------------------------------------------
// openai-compat: no reader at all -- the slot serializes verbatim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flat_responses_text_format_reaches_openai_compat_as_the_nested_shape() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body()))
        .mount(&upstream)
        .await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "openai-mock".to_string(),
        ProviderEntry::openai_compat(upstream.uri(), common::file_ref("test-key")),
    );
    let config = config_with(providers, ModelEntry::new("openai-mock", "mock-model"));

    let body = upstream_body_for(config, &upstream).await;

    // This lane has no `response_format` reader: the canonical slot is
    // serialized as-is. A flat value here is what a strict host rejects
    // with `unsupported_parameter`, so the nested shape is load-bearing.
    assert_eq!(
        body["response_format"],
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": SCHEMA_NAME,
                "schema": {
                    "type": "object",
                    "properties": {SCHEMA_PROPERTY: {"type": "string"}},
                    "required": [SCHEMA_PROPERTY]
                },
                "strict": true
            }
        }),
        "openai-compat must emit the nested OpenAI shape: {body}"
    );
}

// ---------------------------------------------------------------------------
// anthropic-api: output_config.format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flat_responses_text_format_reaches_the_anthropic_egress_as_output_config_format() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic-mock".to_string(),
        ProviderEntry::anthropic_api(common::file_ref("test-key")).with_base_url(upstream.uri()),
    );
    let config = config_with(
        providers,
        ModelEntry::new("anthropic-mock", "claude-haiku-4-5"),
    );

    let body = upstream_body_for(config, &upstream).await;

    // Anthropic's format member admits only `type` + `schema`; `name` and
    // `strict` are rejected outright, so this lane carries the two it can.
    assert_eq!(
        body["output_config"]["format"]["type"],
        json!("json_schema"),
        "Anthropic egress must carry the json_schema tag: {body}"
    );
    assert_eq!(
        body["output_config"]["format"]["schema"]["properties"][SCHEMA_PROPERTY]["type"],
        json!("string"),
        "Anthropic egress must carry the caller's schema: {body}"
    );
}

// ---------------------------------------------------------------------------
// gemini: responseMimeType + responseSchema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flat_responses_text_format_reaches_the_gemini_egress_as_a_response_schema() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.5-pro:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_response_body()))
        .mount(&upstream)
        .await;

    let toml_src = format!(
        r#"
[providers.gemini-mock]
kind = "gemini"
api_key_ref = "{key}"
base_url = "{base}"

[models.target]
provider = "gemini-mock"
upstream = "gemini-2.5-pro"

[aliases]
mock-model = "target"
"#,
        key = common::file_ref("test-key"),
        base = upstream.uri()
    );
    let mut cfg: Config = toml::from_str(&toml_src).expect("gemini test config parses");
    cfg.server = base_server();
    let body = upstream_body_for(Arc::new(cfg), &upstream).await;

    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        json!("application/json"),
        "Gemini egress must request JSON output: {body}"
    );
    // Gemini's OpenAPI-subset cleaner upper-cases type tokens, so the
    // schema arrives shaped for that dialect rather than structurally identical.
    assert_eq!(
        body["generationConfig"]["responseSchema"]["properties"][SCHEMA_PROPERTY]["type"],
        json!("STRING"),
        "Gemini egress must carry the caller's schema: {body}"
    );
}

// ---------------------------------------------------------------------------
// bedrock-invoke: no base_url, so the body shaper is driven directly
// ---------------------------------------------------------------------------

fn bedrock_invoke_cfg() -> BedrockConfig {
    BedrockConfig {
        id: "bedrock-structured-output-test".into(),
        region: "us-west-2".into(),
        model_id: "global.anthropic.claude-haiku-4-5-v1:0".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        // Empty allowlists are pass-through, so nothing this test asserts
        // can be filtered out by operator policy rather than by the
        // translation under test.
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

#[test]
fn flat_responses_text_format_reaches_the_bedrock_invoke_body() {
    use routectl_providers::bedrock::invoke as bedrock_invoke;

    // Arrange: the canonical request the real ingress produced.
    let req = canonical_from_real_ingress();
    let cfg = bedrock_invoke_cfg();

    // Act: the per-request call site inside BedrockProvider::complete.
    let body = bedrock_invoke::normalize_request(&cfg, &req).expect("body shapes");

    // Assert
    assert_eq!(
        body["output_config"]["format"]["type"],
        json!("json_schema"),
        "Bedrock-Invoke must carry the json_schema tag: {body}"
    );
    assert_eq!(
        body["output_config"]["format"]["schema"]["properties"][SCHEMA_PROPERTY]["type"],
        json!("string"),
        "Bedrock-Invoke must carry the caller's schema: {body}"
    );
}

// ---------------------------------------------------------------------------
// cross-dialect: an unknown tag has no spelling on another wire and drops
// ---------------------------------------------------------------------------

/// Gemini has no member an unknown tag could become, so the directive drops
/// and the lane counts it once. The binary's other Gemini lane carries a
/// representable schema and never reaches this class, so the delta is exact.
#[tokio::test]
async fn unknown_responses_text_format_tag_drops_and_counts_on_the_gemini_egress() {
    use routectl_providers::translation_drop_metrics::translation_drop_snapshot;

    // Arrange
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.5-pro:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_response_body()))
        .mount(&upstream)
        .await;
    let toml_src = format!(
        r#"
[providers.gemini-mock]
kind = "gemini"
api_key_ref = "{key}"
base_url = "{base}"

[models.target]
provider = "gemini-mock"
upstream = "gemini-2.5-pro"

[aliases]
mock-model = "target"
"#,
        key = common::file_ref("test-key"),
        base = upstream.uri()
    );
    let mut cfg: Config = toml::from_str(&toml_src).expect("gemini test config parses");
    cfg.server = base_server();
    let request = responses_body_with_format("mock-model", unknown_text_format());
    let gemini_drops = || {
        translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "gemini" && e.drop_class == "response_format_unrepresentable")
            .map_or(0, |e| e.drop_count)
    };
    let before = gemini_drops();

    // Act
    let body = upstream_body_for_request(Arc::new(cfg), &upstream, &request).await;

    // Assert
    let wire = body.to_string();
    assert!(
        !wire.contains("marker_future_format_tag") && !wire.contains("responseMimeType"),
        "Gemini egress must drop an unknown tag rather than guess a member: {body}"
    );
    assert_eq!(
        gemini_drops() - before,
        1,
        "one counted drop for the request"
    );
}

/// The shared Claude reader has no member an unknown tag could become, so the
/// Anthropic egress emits no `output_config.format` for it.
#[tokio::test]
async fn unknown_responses_text_format_tag_drops_on_the_anthropic_egress() {
    // Arrange
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic-mock".to_string(),
        ProviderEntry::anthropic_api(common::file_ref("test-key")).with_base_url(upstream.uri()),
    );
    let config = config_with(
        providers,
        ModelEntry::new("anthropic-mock", "claude-haiku-4-5"),
    );
    let request = responses_body_with_format("mock-model", unknown_text_format());

    // Act
    let body = upstream_body_for_request(config, &upstream, &request).await;

    // Assert
    assert!(
        body["output_config"].get("format").is_none()
            && !body.to_string().contains("marker_future_format_tag"),
        "Anthropic egress must drop an unknown tag: {body}"
    );
}

/// Same shared reader on the Bedrock-Invoke body shaper.
#[test]
fn unknown_responses_text_format_tag_drops_on_the_bedrock_invoke_body() {
    use routectl_providers::bedrock::invoke as bedrock_invoke;

    // Arrange
    let req = canonical_from_real_ingress_body(&responses_body_with_format(
        "mock-model",
        unknown_text_format(),
    ));
    assert_eq!(
        req.response_format,
        Some(unknown_text_format()),
        "the ingress must hand the unknown tag to the egress unchanged"
    );

    // Act
    let body = bedrock_invoke::normalize_request(&bedrock_invoke_cfg(), &req).expect("body shapes");

    // Assert
    assert!(
        body["output_config"].get("format").is_none()
            && !body.to_string().contains("marker_future_format_tag"),
        "Bedrock-Invoke must drop an unknown tag: {body}"
    );
}
