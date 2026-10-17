//! A member lost from the egress body on each FIDELITY lane, end to end: a
//! real daemon, real HTTP into its ingress, a real egress against a mock
//! upstream, and the router's wire-tap drop seam removing one member from
//! the body the egress deposited. The live check must count the loss on
//! `rc_wire_loss_total{lane,missing}` and the health panel must name it by
//! path.
//!
//! In-crate because the router-injection seam is `cfg(test)` only; the Bedrock
//! lane needs it to aim a real provider at the mock origin.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
};
use routectl_router::{
    AliasValue, CURRENT_CONFIG_VERSION, CatalogOverlay, Config, ModelEntry, ProviderEntry,
    ResolvedModel, RetryPolicy, Router, parse_config,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::test_support::isolate_usage_db;

pub(super) const ALIAS: &str = "a";
const NICKNAME: &str = "m";
const PROVIDER: &str = "p";
const UPSTREAM: &str = "upstream";
const BEDROCK_MODEL_ID: &str = "us.anthropic.claude-sonnet-4-5";
const REGION: &str = "us-east-1";

/// One FIDELITY lane: the member its tap drops and what the client sends.
#[derive(Clone, Copy, Debug)]
enum FidelityLane {
    AnthropicApi,
    BedrockInvoke,
    OpenaiCompat,
    OpenaiResponses,
}

impl FidelityLane {
    const fn label(self) -> &'static str {
        match self {
            Self::AnthropicApi => "anthropic->anthropic-api",
            Self::BedrockInvoke => "anthropic->bedrock-invoke",
            Self::OpenaiCompat => "chat->openai-compat",
            Self::OpenaiResponses => "responses->openai-responses",
        }
    }

    /// The member the egress body loses, as the tap removes it and as the
    /// check names it.
    const fn dropped(self) -> &'static str {
        match self {
            Self::AnthropicApi | Self::BedrockInvoke => "/max_tokens",
            Self::OpenaiCompat => "/temperature",
            Self::OpenaiResponses => "/max_output_tokens",
        }
    }

    const fn route(self) -> &'static str {
        match self {
            Self::AnthropicApi | Self::BedrockInvoke => "/v1/messages",
            Self::OpenaiCompat => "/v1/chat/completions",
            Self::OpenaiResponses => "/v1/responses",
        }
    }

    fn request_body(self) -> Value {
        match self {
            Self::AnthropicApi | Self::BedrockInvoke => json!({
                "model": ALIAS, "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            Self::OpenaiCompat => json!({
                "model": ALIAS, "temperature": 0.5,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            Self::OpenaiResponses => json!({
                "model": ALIAS, "max_output_tokens": 16,
                "input": [{"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "hi"}]}]
            }),
        }
    }

    fn upstream_answer(self) -> ResponseTemplate {
        match self {
            Self::AnthropicApi | Self::BedrockInvoke => {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn", "stop_sequence": null,
                    "usage": {"input_tokens": 3, "output_tokens": 1}
                }))
            }
            Self::OpenaiCompat => ResponseTemplate::new(200).set_body_json(json!({
                "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
                "model": UPSTREAM, "system_fingerprint": null,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "ok"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
            })),
            Self::OpenaiResponses => ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses_stream()),
        }
    }
}

/// The Responses egress reads every answer as a stream, so the upstream
/// answers with one: created, the output text, completed.
fn responses_stream() -> String {
    let done = json!({
        "id": "resp_1", "object": "response", "created_at": 1_700_000_000_i64,
        "status": "completed", "model": UPSTREAM,
        "output": [{"type": "message", "id": "m1", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "ok", "annotations": []}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    });
    let mut created = done.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    let frames = [
        (
            "response.created",
            json!({"type": "response.created", "response": created}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "message", "role": "assistant",
                            "status": "completed", "content": []}}),
        ),
        (
            "response.content_part.added",
            json!({"type": "response.content_part.added", "output_index": 0,
                   "content_index": 0,
                   "part": {"type": "output_text", "text": "", "annotations": []}}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "output_index": 0,
                   "content_index": 0, "delta": "ok"}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": done}),
        ),
    ];
    frames
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
}

fn single_target_config(provider: ProviderEntry) -> Config {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    Config {
        providers: BTreeMap::from([(PROVIDER.to_string(), provider)]),
        models: BTreeMap::from([(NICKNAME.to_string(), ModelEntry::new(PROVIDER, UPSTREAM))]),
        aliases: BTreeMap::from([(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()))]),
        retry,
        ..Default::default()
    }
}

fn bearer_key() -> String {
    ["wire", "loss", "test", "key"].join("-")
}

pub(super) fn bedrock_config() -> Config {
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.{PROVIDER}]\n\
         kind = \"bedrock\"\n\
         region = \"{REGION}\"\n\
         api_shape = \"invoke\"\n\
         creds = {{ kind = \"bearer-key\", key_ref = \"{key_ref}\" }}\n\n\
         [models.{NICKNAME}]\n\
         provider = \"{PROVIDER}\"\n\
         upstream = \"{BEDROCK_MODEL_ID}\"\n\n\
         [aliases]\n\
         {ALIAS} = \"{NICKNAME}\"\n\n\
         [retry]\n\
         max_attempts = 1\n",
        key_ref = crate::test_secret::file_ref(&bearer_key()),
    );
    parse_config(&text).expect("valid bedrock test config")
}

pub(super) async fn bedrock_router(config: Arc<Config>, origin: &str) -> Router {
    let creds = BedrockCreds::BearerKey { key: bearer_key() };
    let cfg = BedrockConfig {
        id: format!("bedrock:{PROVIDER}"),
        region: REGION.into(),
        model_id: BEDROCK_MODEL_ID.into(),
        api_shape: BedrockApiShape::Invoke,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = auth::resolve(&creds, REGION)
        .await
        .expect("bearer creds resolve");
    let provider = BedrockProvider::new(cfg, resolved)
        .expect("canonical region")
        .with_runtime_origin_for_tests(origin);
    let mut router = Router::new(config);
    router.install_resolved_models(BTreeMap::from([(
        NICKNAME.to_string(),
        Arc::new(ResolvedModel::new(
            NICKNAME,
            PROVIDER,
            Arc::new(provider),
            BEDROCK_MODEL_ID,
        )),
    )]));
    router
}

/// The lane's config and router, aimed at `origin`, before the drop seam.
async fn lane_router(lane: FidelityLane, origin: &str) -> (Config, Router) {
    let key = crate::test_secret::file_ref("k");
    let provider = match lane {
        FidelityLane::BedrockInvoke => {
            let config = bedrock_config();
            let router = bedrock_router(Arc::new(config.clone()), origin).await;
            return (config, router);
        }
        FidelityLane::AnthropicApi => ProviderEntry::anthropic_api(key).with_base_url(origin),
        FidelityLane::OpenaiCompat => ProviderEntry::openai_compat(origin, key),
        FidelityLane::OpenaiResponses => ProviderEntry::openai_responses(key)
            .with_openai_responses_auth_kind(routectl_providers::openai_responses::AuthKind::ApiKey)
            .with_openai_responses_base_url(origin),
    };
    let config = single_target_config(provider);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let router = super::build_router_from_config(Arc::new(config.clone()), secrets)
        .await
        .expect("build router");
    (config, router)
}

/// A running daemon serving an injected router, torn down by
/// [`Daemon::shutdown`].
pub(super) struct Daemon {
    pub(super) base_url: String,
    handle: tokio::task::JoinHandle<()>,
    _usage_dir: tempfile::TempDir,
}

impl Daemon {
    pub(super) async fn spawn(mut config: Config, router: Router) -> Self {
        let usage_dir = isolate_usage_db(&mut config);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let handle = tokio::spawn(async move {
            super::serve::serve_on_listener_with_injected_router(
                Arc::new(config),
                Arc::new(CatalogOverlay::default()),
                listener,
                None,
                Some(secrets),
                super::serve::DaemonTestSeams {
                    injected_router: Some(router),
                    ..Default::default()
                },
            )
            .await
            .expect("the daemon serves");
        });
        let daemon = Self {
            base_url,
            handle,
            _usage_dir: usage_dir,
        };
        daemon.await_health().await;
        daemon
    }

    async fn await_health(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if reqwest::get(format!("{}/health", self.base_url))
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon never answered /health"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn send(&self, lane: FidelityLane) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}{}", self.base_url, lane.route()))
            .header("anthropic-version", "2023-06-01")
            .json(&lane.request_body())
            .send()
            .await
            .expect("the daemon answers")
    }

    async fn wire_check_rows(&self) -> Value {
        let resp = reqwest::get(format!("{}/status/health", self.base_url))
            .await
            .expect("GET /status/health");
        assert!(resp.status().is_success(), "GET /status/health failed");
        let body: Value = resp.json().await.expect("health panel json");
        body["data"]["wire_check"].clone()
    }

    pub(super) async fn shutdown(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// Run one request on `lane` through a daemon whose taps drop the lane's
/// member, and return the health panel's wire-check rows afterwards.
async fn rows_after_a_dropped_member(lane: FidelityLane) -> Value {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(lane.upstream_answer())
        .mount(&upstream)
        .await;
    let (config, router) = lane_router(lane, &upstream.uri()).await;
    let daemon = Daemon::spawn(config, router.with_wire_tap_drop_for_tests(lane.dropped())).await;

    let resp = daemon.send(lane).await;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "{lane:?}: {status} {body}");
    let received = upstream.received_requests().await.expect("recording");
    let sent: Value = serde_json::from_slice(&received[0].body).expect("upstream body json");
    assert!(
        sent.pointer(lane.dropped()).is_some(),
        "{lane:?}: the seam drops only from the deposit, the wire still carries it: {sent}"
    );
    let rows = daemon.wire_check_rows().await;
    daemon.shutdown().await;
    rows
}

/// Assert `rows` count exactly one `missing` loss on `lane` and name it by
/// the dropped path on the request leg.
fn assert_counted_and_named(lane: FidelityLane, rows: &Value) {
    let loss: Vec<(&str, &str, u64)> = rows["loss"]
        .as_array()
        .expect("loss rows")
        .iter()
        .map(|r| {
            (
                r["lane"].as_str().unwrap_or_default(),
                r["path_class"].as_str().unwrap_or_default(),
                r["count"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(loss, [(lane.label(), "missing", 1)], "{lane:?}: {rows}");
    let named: Vec<(&str, &str, &str, &str)> = rows["loss_findings"]
        .as_array()
        .expect("loss finding rows")
        .iter()
        .map(|r| {
            (
                r["lane"].as_str().unwrap_or_default(),
                r["leg"].as_str().unwrap_or_default(),
                r["path"].as_str().unwrap_or_default(),
                r["path_class"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        named,
        [(lane.label(), "request", lane.dropped(), "missing")],
        "{lane:?}: {rows}"
    );
}

async fn a_lost_member_is_counted_and_named(lane: FidelityLane) {
    let rows = rows_after_a_dropped_member(lane).await;

    assert_counted_and_named(lane, &rows);
}

#[tokio::test]
async fn a_member_lost_on_anthropic_to_anthropic_api_is_counted_and_named_by_path() {
    a_lost_member_is_counted_and_named(FidelityLane::AnthropicApi).await;
}

#[tokio::test]
async fn a_member_lost_on_anthropic_to_bedrock_invoke_is_counted_and_named_by_path() {
    a_lost_member_is_counted_and_named(FidelityLane::BedrockInvoke).await;
}

#[tokio::test]
async fn a_member_lost_on_chat_to_openai_compat_is_counted_and_named_by_path() {
    a_lost_member_is_counted_and_named(FidelityLane::OpenaiCompat).await;
}

#[tokio::test]
async fn a_member_lost_on_responses_to_openai_responses_is_counted_and_named_by_path() {
    a_lost_member_is_counted_and_named(FidelityLane::OpenaiResponses).await;
}
