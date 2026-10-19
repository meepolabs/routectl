//! `[models.X] effort_levels` from TOML through the factory, the dispatch
//! overlay and the REAL egress to the bytes a wiremock upstream receives.
//! An entry that omits the key forwards the caller's effort unclamped; an
//! explicit list clamps. Covers the anthropic-api adaptive shape
//! (`output_config.effort`) and the openai-compat default dialect
//! (top-level `reasoning_effort`).

use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::config::Config;
use crate::factory::{BuildOptions, build_resolved_models};
use crate::router::{Router, RouterOptions};

/// The egress kinds under test: the TOML provider block, the success body
/// the upstream answers with, and where the effort lands on the wire.
#[derive(Clone, Copy, Debug)]
enum Lane {
    AnthropicApi,
    OpenaiCompat,
}

impl Lane {
    fn provider_toml(self, base_url: &str, key_ref: &str) -> String {
        match self {
            Self::AnthropicApi => format!(
                "kind = \"anthropic-api\"\nbase_url = \"{base_url}\"\napi_key_ref = \"{key_ref}\"\n"
            ),
            Self::OpenaiCompat => format!(
                "kind = \"openai-compat\"\nbase_url = \"{base_url}/v1\"\napi_key_ref = \"{key_ref}\"\n"
            ),
        }
    }

    /// `supports_adaptive_thinking` puts the anthropic-api effort on the
    /// wire as a string (`output_config.effort`); the legacy shape turns it
    /// into a token budget instead.
    const fn model_knobs(self) -> &'static str {
        match self {
            Self::AnthropicApi => "supports_adaptive_thinking = true\n",
            Self::OpenaiCompat => "",
        }
    }

    fn ok_body(self) -> Value {
        match self {
            Self::AnthropicApi => json!({
                "id": "msg_ok",
                "type": "message",
                "role": "assistant",
                "model": "upstream-model",
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1},
                "content": [{"type": "text", "text": "ok"}]
            }),
            Self::OpenaiCompat => json!({
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
            }),
        }
    }

    fn wire_effort(self, body: &Value) -> Option<&str> {
        let field = match self {
            Self::AnthropicApi => body.pointer("/output_config/effort"),
            Self::OpenaiCompat => body.get("reasoning_effort"),
        };
        field.and_then(Value::as_str)
    }
}

/// Parse a one-provider, one-model config from TOML, so an omitted
/// `effort_levels` takes the serde default rather than a builder default.
fn config_toml(lane: Lane, base_url: &str, effort_levels_line: &str) -> Config {
    let key_ref = crate::test_secret::file_ref("test-key");
    let toml_text = format!(
        "[providers.p]\n{provider}\n[models.m]\nprovider = \"p\"\nupstream = \"upstream-model\"\n{knobs}{effort_levels_line}\n",
        provider = lane.provider_toml(base_url, &key_ref),
        knobs = lane.model_knobs(),
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
async fn wire_effort_for(
    lane: Lane,
    effort_levels_line: &str,
    caller_effort: &str,
) -> (Option<String>, Value) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lane.ok_body()))
        .mount(&server)
        .await;
    let router = router_for(config_toml(lane, &server.uri(), effort_levels_line)).await;

    let d = router
        .complete_with_options(effort_req(caller_effort), RouterOptions::default())
        .await;

    assert!(
        d.result.is_ok(),
        "{lane:?}: dispatch failed: {:?}",
        d.result.err()
    );
    let reqs = server.received_requests().await.expect("received requests");
    assert_eq!(reqs.len(), 1, "{lane:?}: exactly one upstream request");
    let body: Value = serde_json::from_slice(&reqs[0].body).expect("outbound body is JSON");
    (lane.wire_effort(&body).map(str::to_string), body)
}

struct Row {
    name: &'static str,
    lane: Lane,
    effort_levels_line: &'static str,
    caller_effort: &'static str,
    wire_effort: &'static str,
}

const ROWS: &[Row] = &[
    Row {
        name: "anthropic-api, key omitted, max passes through",
        lane: Lane::AnthropicApi,
        effort_levels_line: "",
        caller_effort: "max",
        wire_effort: "max",
    },
    Row {
        name: "anthropic-api, key omitted, xhigh passes through",
        lane: Lane::AnthropicApi,
        effort_levels_line: "",
        caller_effort: "xhigh",
        wire_effort: "xhigh",
    },
    Row {
        name: "anthropic-api, explicit low/medium/high, max clamps to high",
        lane: Lane::AnthropicApi,
        effort_levels_line: "effort_levels = [\"low\", \"medium\", \"high\"]",
        caller_effort: "max",
        wire_effort: "high",
    },
    Row {
        name: "openai-compat, key omitted, max passes through",
        lane: Lane::OpenaiCompat,
        effort_levels_line: "",
        caller_effort: "max",
        wire_effort: "max",
    },
    Row {
        name: "openai-compat, key omitted, xhigh passes through",
        lane: Lane::OpenaiCompat,
        effort_levels_line: "",
        caller_effort: "xhigh",
        wire_effort: "xhigh",
    },
    Row {
        name: "openai-compat, explicit low/medium/high, max clamps to high",
        lane: Lane::OpenaiCompat,
        effort_levels_line: "effort_levels = [\"low\", \"medium\", \"high\"]",
        caller_effort: "max",
        wire_effort: "high",
    },
];

#[tokio::test]
async fn effort_levels_from_config_decides_the_effort_on_the_wire() {
    let mut failures = Vec::new();
    for row in ROWS {
        let (got, body) =
            wire_effort_for(row.lane, row.effort_levels_line, row.caller_effort).await;
        if got.as_deref() != Some(row.wire_effort) {
            failures.push(format!(
                "{}: expected wire effort {:?}, got {got:?}; body = {body}",
                row.name, row.wire_effort
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "failing rows:\n{}",
        failures.join("\n")
    );
}
