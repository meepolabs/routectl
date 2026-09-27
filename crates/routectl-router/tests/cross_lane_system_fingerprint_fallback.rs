//! Canonical system content across a cross-lane fallback: Gemini first, then
//! OpenAI Responses.
//!
//! The billing/attribution strip is attempt-local. Each lane filters its own
//! projection of the caller's ORIGINAL canonical request, so a first hop that
//! withholds the fingerprint and then fails cannot hand a filtered (or an
//! unfiltered) copy to the next hop -- the fallback hop re-derives its wire
//! body from the same canonical input under its own policy.
//!
//! Both hops are the real providers against mock upstreams, so every
//! assertion reads the bytes each lane actually sent.

#![cfg(all(feature = "gemini", feature = "openai-responses"))]

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{
    ChatRequest, Message, MessageContent, Provider, Role, SystemBlock, SystemContent,
};
use routectl_providers::gemini::{GeminiConfig, GeminiProvider};
use routectl_providers::openai_responses::{
    AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};
use routectl_router::{
    AliasValue, Config, Dispatched, ProviderEntry, ResolvedModel, RetryPolicy, Router,
    RouterOptions,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const FINGERPRINT: &str = "x-anthropic-billing-header: cc_version=9.9.9; cch=fallbk42";
const FINGERPRINT_TELL: &str = "fallbk42";
const CANONICAL_PROMPT: &str = "fallback-canonical-sentinel";
const MESSAGE_PROMPT: &str = "fallback-message-sentinel";

fn message(role: Role, text: &str) -> Message {
    Message {
        refusal: None,
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn block(text: &str) -> SystemBlock {
    SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    }
}

/// The fingerprint rides BOTH canonical system surfaces, each beside a
/// legitimate sentinel, so each hop must filter both.
fn caller_request() -> ChatRequest {
    ChatRequest {
        model: "fast".into(),
        max_tokens: Some(64),
        system: Some(SystemContent::Blocks(vec![
            block(FINGERPRINT),
            block(CANONICAL_PROMPT),
        ])),
        messages: vec![
            message(Role::System, FINGERPRINT),
            message(Role::System, MESSAGE_PROMPT),
            message(Role::User, "hi"),
        ]
        .into(),
        ..Default::default()
    }
}

fn completed_sse() -> String {
    let completed = serde_json::json!({
        "id": "resp_fallback",
        "object": "response",
        "status": "completed",
        "model": "gpt-5",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "pong"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed).expect("fixture renders")
    )
}

fn fallback_router(gemini: Arc<dyn Provider>, responses: Arc<dyn Provider>) -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "p-gemini".into(),
        ProviderEntry::gemini(common::file_ref("k")),
    );
    config.providers.insert(
        "p-responses".into(),
        ProviderEntry::openai_responses(common::file_ref("k")),
    );
    config.aliases.insert(
        "fast".into(),
        AliasValue::Chain(vec!["m-gemini".into(), "m-responses".into()]),
    );
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    retry.initial_backoff_ms = 1;
    retry.backoff_multiplier = 1.0;
    config.retry = retry;

    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m-gemini".into(),
        Arc::new(ResolvedModel::new(
            "m-gemini",
            "p-gemini",
            gemini,
            "gemini-2.5-pro",
        )),
    );
    models.insert(
        "m-responses".into(),
        Arc::new(ResolvedModel::new(
            "m-responses",
            "p-responses",
            responses,
            "gpt-5",
        )),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

/// The one body a mock received, as text.
async fn only_body(server: &MockServer) -> String {
    let received = server.received_requests().await.expect("captured requests");
    assert_eq!(
        received.len(),
        1,
        "exactly one request reached this upstream"
    );
    String::from_utf8(received[0].body.clone()).expect("utf-8 body")
}

#[tokio::test]
#[serial_test::serial]
async fn each_fallback_hop_withholds_the_fingerprint_and_keeps_both_system_sources() {
    // Arrange -- the Responses provider's cookie jar would otherwise persist
    // under the real home directory on drop.
    let jar_dir = tempfile::tempdir().expect("temp dir");
    let _jar = routectl_testkit::ScopedEnv::set(
        "ROUTECTL_COOKIE_FILE",
        jar_dir.path().join("cookies.json"),
    );
    let gemini_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .mount(&gemini_upstream)
        .await;
    let responses_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completed_sse()),
        )
        .mount(&responses_upstream)
        .await;

    let mut gemini_cfg = GeminiConfig::new("p-gemini", "test-key");
    gemini_cfg.base_url = gemini_upstream.uri();
    let mut responses_cfg = OpenAiResponsesConfig::new("p-responses", "test-key");
    responses_cfg.auth_kind = AuthKind::ApiKey;
    responses_cfg.base_url = responses_upstream.uri();
    let router = fallback_router(
        Arc::new(GeminiProvider::new(gemini_cfg)),
        Arc::new(OpenAiResponsesProvider::new(responses_cfg)),
    );
    let caller_req = caller_request();

    // Act
    let Dispatched { meta, result } = router
        .complete_with_options(caller_req.clone(), RouterOptions::new())
        .await;

    // Assert -- the chain really fell back, and the second hop served.
    result.expect("the Responses hop serves the request");
    assert_eq!(meta.fallback_count, 1, "exactly one fallback hop");
    assert_eq!(meta.served_provider.as_deref(), Some("p-responses"));

    // The first hop's wire body: fingerprint withheld, both sources kept.
    let gemini_body = only_body(&gemini_upstream).await;
    assert!(
        !gemini_body.contains(FINGERPRINT_TELL),
        "the Gemini hop shipped the client fingerprint: {gemini_body}"
    );
    assert!(
        gemini_body.contains(CANONICAL_PROMPT) && gemini_body.contains(MESSAGE_PROMPT),
        "the Gemini hop lost legitimate system content: {gemini_body}"
    );

    // The fallback hop's wire body, re-derived from the original request.
    let responses_body: serde_json::Value =
        serde_json::from_str(&only_body(&responses_upstream).await).expect("json body");
    assert!(
        !responses_body.to_string().contains(FINGERPRINT_TELL),
        "the Responses hop shipped the client fingerprint: {responses_body}"
    );
    assert_eq!(
        responses_body["instructions"],
        serde_json::json!(format!("{CANONICAL_PROMPT}\n\n{MESSAGE_PROMPT}")),
        "the Responses hop keeps the top-level system first, then message text"
    );

    // The caller's canonical request left the walk as it entered.
    assert_eq!(
        serde_json::to_value(&caller_req).expect("renders"),
        serde_json::to_value(caller_request()).expect("renders"),
    );
}
