//! A cloak relocation refusal on the own-OAuth Anthropic seat is a local
//! failure: it never debits that seat's breaker, and the fallback hop receives
//! the caller's canonical request exactly as the caller built it.
//!
//! The seat is the REAL anthropic-api provider on the cloak lane, so the
//! refusal comes from the production relocation rather than a stand-in. A
//! breaker threshold of ONE makes any debit open it, so a closed breaker after
//! the walk is evidence rather than a threshold that was simply not reached.
//! The positive control drives the same seat into a debiting failure and
//! watches the breaker open.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::error::{Error, Result};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Message, MessageContent, Provider, Role, SystemContent,
};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use routectl_router::runtime_state::CircuitPhase;
use routectl_router::{
    AliasValue, Config, Dispatched, ProviderEntry, ResolvedModel, RetryPolicy, Router,
    RouterOptions,
};

mod common;

/// Fallback hop: records every request it is handed and succeeds.
struct RecordingFallback {
    seen: Mutex<Vec<ChatRequest>>,
}

impl RecordingFallback {
    fn seen(&self) -> Vec<ChatRequest> {
        self.seen.lock().expect("seen poisoned").clone()
    }
}

#[async_trait]
impl Provider for RecordingFallback {
    fn id(&self) -> &'static str {
        "p-fallback"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p-fallback", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.seen.lock().expect("seen poisoned").push(req);
        Ok(ChatResponse::default())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.seen.lock().expect("seen poisoned").push(req);
        Err(Error::upstream("p-fallback", 503, "unused"))
    }
}

/// A token source whose failure debits: status 0 is a network-class fault.
#[derive(Debug)]
struct NetworkFailingTokenSource;

#[async_trait]
impl routectl_core::TokenSource for NetworkFailingTokenSource {
    async fn token(&self) -> Result<String> {
        Err(Error::upstream("p-oauth", 0, "token endpoint unreachable"))
    }
}

fn oauth_seat() -> Arc<AnthropicApiProvider> {
    let mut cfg = AnthropicApiConfig::new_with_auth("p-oauth", Arc::new(NetworkFailingTokenSource));
    cfg.auth_kind = AuthKind::OauthBearer;
    cfg.base_url = "https://api.anthropic.com".into();
    Arc::new(AnthropicApiProvider::new(cfg))
}

fn router_with(fallback: Arc<RecordingFallback>) -> Router {
    let mut config = Config::default();
    let mut oauth = ProviderEntry::anthropic_api(common::file_ref("k"));
    if let ProviderEntry::AnthropicApi { runtime, .. } = &mut oauth {
        runtime.circuit_failures = Some(1);
        runtime.circuit_cooldown_ms = Some(60_000);
    }
    config.providers.insert("p-oauth".into(), oauth);
    config.providers.insert(
        "p-fallback".into(),
        ProviderEntry::openai_compat("http://example.invalid", common::file_ref("k")),
    );
    config.aliases.insert(
        "fast".into(),
        AliasValue::Chain(vec!["m-oauth".into(), "m-fallback".into()]),
    );
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    retry.initial_backoff_ms = 1;
    retry.backoff_multiplier = 1.0;
    config.retry = retry;

    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m-oauth".into(),
        Arc::new(ResolvedModel::new(
            "m-oauth",
            "p-oauth",
            oauth_seat() as Arc<dyn Provider>,
            "claude-sonnet-4-5",
        )),
    );
    models.insert(
        "m-fallback".into(),
        Arc::new(ResolvedModel::new(
            "m-fallback",
            "p-fallback",
            fallback as Arc<dyn Provider>,
            "up-fallback",
        )),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

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

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "fast".into(),
        max_tokens: Some(64),
        system: Some(SystemContent::Text("client rules".into())),
        messages: messages.into(),
        ..Default::default()
    }
}

fn oauth_circuit(router: &Router) -> CircuitPhase {
    router
        .status_targets(Instant::now())
        .into_iter()
        .find(|t| t.nickname == "m-oauth")
        .expect("the oauth seat has a status row")
        .gate
        .circuit
}

fn fresh_fallback() -> Arc<RecordingFallback> {
    Arc::new(RecordingFallback {
        seen: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn a_refused_relocation_falls_back_with_the_pristine_request_and_no_breaker_debit() {
    let fallback = fresh_fallback();
    let router = router_with(Arc::clone(&fallback));
    let caller = request(vec![message(Role::System, "turn rules")]);

    let Dispatched { meta, result } = router
        .complete_with_options(caller.clone(), RouterOptions::new())
        .await;

    result.expect("the fallback hop serves the request");
    assert_eq!(meta.fallback_count, 1, "exactly one fallback hop");
    assert_eq!(
        oauth_circuit(&router),
        CircuitPhase::Closed,
        "a local refusal must not debit the seat's breaker"
    );
    let seen = fallback.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        serde_json::to_value(&seen[0].system).unwrap(),
        serde_json::to_value(&caller.system).unwrap(),
        "the fallback receives the caller's system untouched"
    );
    assert_eq!(
        serde_json::to_value(&seen[0].messages).unwrap(),
        serde_json::to_value(&caller.messages).unwrap(),
        "the fallback receives the caller's messages untouched"
    );
}

#[tokio::test]
async fn a_debiting_failure_on_the_same_seat_opens_its_breaker() {
    // Positive control: the relocation lands, the seat reaches token
    // resolution, and the network-class failure there debits at threshold one.
    let fallback = fresh_fallback();
    let router = router_with(fallback);
    let caller = request(vec![message(Role::Assistant, "prior")]);

    let Dispatched { result, .. } = router
        .complete_with_options(caller, RouterOptions::new())
        .await;

    result.expect("the fallback hop serves the request");
    assert_eq!(oauth_circuit(&router), CircuitPhase::Open);
}
