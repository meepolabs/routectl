//! A caller's effort from a TOML-built model through the factory, the
//! dispatch overlay and the REAL egress to the bytes a wiremock upstream
//! receives: no config key clamps it, so it reaches the openai-compat
//! default dialect (top-level `reasoning_effort`) unchanged.

use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::config::Config;
use crate::factory::{BuildOptions, build_resolved_models};
use crate::router::{Router, RouterOptions};

fn ok_body() -> Value {
    json!({
        "id": "chatcmpl-ok",
        "object": "chat.completion",
        "created": 0,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

/// Parse a one-provider, one-model openai-compat config from TOML, so the
/// model takes its serde defaults rather than builder defaults.
fn config_toml(base_url: &str) -> Config {
    let key_ref = crate::test_secret::file_ref("test-key");
    let toml_text = format!(
        "[providers.p]\nkind = \"openai-compat\"\nbase_url = \"{base_url}/v1\"\napi_key_ref = \"{key_ref}\"\n\n[models.m]\nprovider = \"p\"\nupstream = \"upstream-model\"\n"
    );
    toml::from_str(&toml_text).expect("test config parses")
}

async fn router_for(cfg: Config) -> Router {
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");
    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);
    router
}

fn effort_req(effort: &str) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(4096),
        reasoning: Some(ReasoningConfig {
            effort: Some(effort.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Dispatch one request carrying `caller_effort` and return the effort the
/// upstream received, with the outbound body for failure messages.
async fn wire_effort_for(caller_effort: &str) -> (Option<String>, Value) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let router = router_for(config_toml(&server.uri())).await;

    let d = router
        .complete_with_options(effort_req(caller_effort), RouterOptions::default())
        .await;

    assert!(d.result.is_ok(), "dispatch failed: {:?}", d.result.err());
    let reqs = server.received_requests().await.expect("received requests");
    assert_eq!(reqs.len(), 1, "exactly one upstream request");
    let body: Value = serde_json::from_slice(&reqs[0].body).expect("outbound body is JSON");
    let effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    (effort, body)
}

#[tokio::test]
async fn the_callers_effort_reaches_the_wire_unclamped() {
    let mut failures = Vec::new();
    for caller_effort in ["max", "xhigh"] {
        let (got, body) = wire_effort_for(caller_effort).await;
        if got.as_deref() != Some(caller_effort) {
            failures.push(format!(
                "{caller_effort}: expected it on the wire, got {got:?}; body = {body}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "failing rows:\n{}",
        failures.join("\n")
    );
}
