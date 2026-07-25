//! `DispatchTarget::use_forwarded_credential`: populated once at chain
//! expansion from the provider entry's `credential_source`, then read
//! at all three dispatch paths (complete, count_tokens, stream) to
//! bypass the `attempt_req.model` rewrite. A forwarded target forwards
//! the client's requested model verbatim; an own target still rewrites
//! to the target's configured `upstream`.
use super::*;
use crate::config::{AliasValue, CredentialSource, ProviderEntry};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider, TokenCount,
};
use std::collections::BTreeMap;

/// Records the `model` field of every request it is dispatched with,
/// and echoes it straight back on `complete` -- lets a test observe
/// exactly what the router sent upstream without inspecting private
/// dispatch state.
struct ModelSpyProvider {
    id: String,
    seen: Mutex<Vec<String>>,
}

impl ModelSpyProvider {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen_models(&self) -> Vec<String> {
        self.seen.lock().clone()
    }
}

#[async_trait]
impl Provider for ModelSpyProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.seen.lock().push(req.model.clone());
        Ok(ChatResponse {
            id: format!("ok-{}", self.id),
            model: req.model,
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: routectl_core::Role::Assistant,
                    content: routectl_core::MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(routectl_core::Usage::default()),
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        })
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.seen.lock().push(req.model.clone());
        Ok(Box::pin(futures::stream::once(async {
            Ok(ChatChunk::default())
        })))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.seen.lock().push(req.model.clone());
        Ok(TokenCount::default())
    }
}

/// Register one nickname/provider/upstream leg on a fresh router,
/// with the provider entry's `credential_source` set per `forwarded`.
fn router_with_leg(
    nickname: &str,
    provider_name: &str,
    upstream: &str,
    forwarded: bool,
) -> (Router, Arc<ModelSpyProvider>) {
    let spy = Arc::new(ModelSpyProvider::new(provider_name));
    let mut entry = ProviderEntry::anthropic_api("literal:k");
    if forwarded {
        entry = entry.with_credential_source(CredentialSource::Forwarded);
    }
    let mut config = Config::default();
    config.providers.insert(provider_name.to_string(), entry);
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        nickname.to_string(),
        Arc::new(ResolvedModel::new(
            nickname,
            provider_name,
            spy.clone() as Arc<dyn Provider>,
            upstream,
        )),
    );
    router.install_resolved_models(models);
    (router, spy)
}

fn req_for(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![].into(),
        ..Default::default()
    }
}

/// Like `req_for`, but stamps a captured forwarded bearer so a
/// forwarded-credential target's missing-bearer terminal guard (see
/// `missing_forwarded_bearer_error`) does not refuse the request
/// before the model-transparency assertion runs.
fn forwarded_req_for(model: &str) -> ChatRequest {
    let mut req = req_for(model);
    req.routectl_internal.forwarded_bearer =
        Some(ForwardedBearer::new("sk-ant-oat01-test".to_string()));
    req
}

#[test]
fn expand_chain_to_targets_sets_flag_true_for_forwarded_anthropic_provider() {
    let (router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
    let model = Arc::new(ResolvedModel::new(
        "opus",
        "fwd-prov",
        spy as Arc<dyn Provider>,
        "claude-opus-upstream",
    ));
    let targets = router.expand_chain_to_targets(vec![model], None);
    assert_eq!(targets.len(), 1);
    assert!(
        targets[0].use_forwarded_credential,
        "a Forwarded AnthropicApi provider entry must set the target flag true",
    );
}

#[test]
fn expand_chain_to_targets_sets_flag_false_for_own_anthropic_provider() {
    let (router, spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
    let model = Arc::new(ResolvedModel::new(
        "opus",
        "own-prov",
        spy as Arc<dyn Provider>,
        "claude-opus-upstream",
    ));
    let targets = router.expand_chain_to_targets(vec![model], None);
    assert_eq!(targets.len(), 1);
    assert!(
        !targets[0].use_forwarded_credential,
        "the default Own credential source must leave the target flag false",
    );
}

#[test]
fn expand_chain_to_targets_sets_flag_for_every_seat_of_a_forwarded_pool() {
    let spy = Arc::new(ModelSpyProvider::new("fwd-prov"));
    let mut config = Config::default();
    config.providers.insert(
        "fwd-prov".to_string(),
        ProviderEntry::anthropic_api("literal:k")
            .with_credential_source(CredentialSource::Forwarded),
    );
    let router = Router::new(Arc::new(config));
    let seats: Vec<crate::seat_pool::SeatTarget> = ["seat-a", "seat-b"]
        .iter()
        .map(|label| crate::seat_pool::SeatTarget {
            label: Some((*label).to_string()),
            state_key: crate::seat_pool::seat_state_key("nick", Some(label)),
            provider: spy.clone() as Arc<dyn Provider>,
            auth_secret_ref: None,
        })
        .collect();
    let model = Arc::new(
        ResolvedModel::new("nick", "fwd-prov", spy as Arc<dyn Provider>, "claude-x")
            .with_seats(seats.into()),
    );
    let targets = router.expand_chain_to_targets(vec![model], None);
    assert_eq!(targets.len(), 2);
    for target in &targets {
        assert!(
            target.use_forwarded_credential,
            "every seat of a Forwarded provider's pool must carry the flag true",
        );
    }
}

#[tokio::test]
async fn complete_forwards_opus_verbatim_on_a_forwarded_target() {
    let (router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
    router
        .complete(forwarded_req_for("opus"))
        .await
        .expect("forwarded target must dispatch");
    assert_eq!(
        spy.seen_models(),
        vec!["opus".to_string()],
        "the client's requested model must reach egress verbatim",
    );
}

#[tokio::test]
async fn complete_forwards_haiku_verbatim_on_a_forwarded_target() {
    let (router, spy) = router_with_leg("haiku", "fwd-prov", "claude-haiku-upstream", true);
    router
        .complete(forwarded_req_for("haiku"))
        .await
        .expect("forwarded target must dispatch");
    assert_eq!(spy.seen_models(), vec!["haiku".to_string()]);
}

#[tokio::test]
async fn complete_forwards_an_unknown_model_verbatim_via_default_alias() {
    // The requested model matches no alias/glob/nickname and only
    // resolves at all through the `default` catch-all -- exercising
    // "no local model gatekeeping": routing picks the target, but the
    // wire model text is untouched by that routing decision.
    let (mut router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
    let mut config = (*router.config).clone();
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single("opus".to_string()),
    );
    router.config = Arc::new(config);

    router
        .complete(forwarded_req_for("some-unlisted-vendor-model"))
        .await
        .expect("default alias must resolve and dispatch");
    assert_eq!(
        spy.seen_models(),
        vec!["some-unlisted-vendor-model".to_string()],
    );
}

#[tokio::test]
async fn complete_rewrites_model_to_upstream_on_an_own_target() {
    let (router, spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
    router
        .complete(req_for("opus"))
        .await
        .expect("own target must dispatch");
    assert_eq!(
        spy.seen_models(),
        vec!["claude-opus-upstream".to_string()],
        "an own target must still rewrite to the configured upstream",
    );
}

#[tokio::test]
async fn count_tokens_forwards_model_verbatim_on_a_forwarded_target() {
    let (router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
    router
        .count_tokens(forwarded_req_for("opus"))
        .await
        .expect("forwarded count_tokens target must dispatch");
    assert_eq!(spy.seen_models(), vec!["opus".to_string()]);
}

#[tokio::test]
async fn count_tokens_rewrites_model_to_upstream_on_an_own_target() {
    let (router, spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
    router
        .count_tokens(req_for("opus"))
        .await
        .expect("own count_tokens target must dispatch");
    assert_eq!(spy.seen_models(), vec!["claude-opus-upstream".to_string()]);
}

#[tokio::test]
async fn stream_forwards_model_verbatim_on_a_forwarded_target() {
    let (router, spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
    let _stream = router
        .stream(forwarded_req_for("opus"))
        .await
        .expect("forwarded stream target must dispatch");
    assert_eq!(spy.seen_models(), vec!["opus".to_string()]);
}

#[tokio::test]
async fn stream_rewrites_model_to_upstream_on_an_own_target() {
    let (router, spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
    let _stream = router
        .stream(req_for("opus"))
        .await
        .expect("own stream target must dispatch");
    assert_eq!(spy.seen_models(), vec!["claude-opus-upstream".to_string()]);
}

// -------- DispatchMeta::served_upstream / served_forwarded_credential --
//
// `mark_target` mirrors the model-transparency bypass into the
// accounting meta so post-dispatch usage recording (which never sees
// the dropped `DispatchTarget` chain) can tell a forwarded row's
// actual served model apart from `target.upstream`, and flag the row
// as forwarded without re-deriving it from request-global bearer
// presence. `served_model` (the K-triple nickname) is asserted
// unchanged on both lanes -- it must never carry the wire model.

#[tokio::test]
async fn complete_forwarded_target_records_client_model_as_served_upstream() {
    let (router, _spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
    let dispatched = router
        .complete_with_options(forwarded_req_for("opus"), RouterOptions::default())
        .await;
    dispatched.result.expect("forwarded target must dispatch");
    assert_eq!(
        dispatched.meta.served_upstream,
        Some("opus".to_string()),
        "served_upstream must carry the client's requested model, not target.upstream",
    );
    assert_eq!(
        dispatched.meta.served_model,
        Some("opus".to_string()),
        "the K-triple nickname dimension must stay stable on the forwarded lane",
    );
    assert!(
        dispatched.meta.served_forwarded_credential,
        "the forwarded marker must be set for post-dispatch usage disambiguation",
    );
}

#[tokio::test]
async fn complete_forwarded_unlisted_model_records_it_verbatim_as_served_upstream() {
    // Model transparency for an unlisted model routed via the
    // catch-all `default` alias: served_upstream still mirrors the
    // client's exact (unlisted) request, never target.upstream.
    let (mut router, _spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
    let mut config = (*router.config).clone();
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single("opus".to_string()),
    );
    router.config = Arc::new(config);

    let dispatched = router
        .complete_with_options(
            forwarded_req_for("some-unlisted-vendor-model"),
            RouterOptions::default(),
        )
        .await;
    dispatched
        .result
        .expect("default alias must resolve and dispatch");
    assert_eq!(
        dispatched.meta.served_upstream,
        Some("some-unlisted-vendor-model".to_string()),
    );
}

#[tokio::test]
async fn complete_own_target_records_target_upstream_as_served_upstream_unchanged() {
    let (router, _spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
    let dispatched = router
        .complete_with_options(req_for("opus"), RouterOptions::default())
        .await;
    dispatched.result.expect("own target must dispatch");
    assert_eq!(
        dispatched.meta.served_upstream,
        Some("claude-opus-upstream".to_string()),
        "an own target's served_upstream must stay target.upstream, unchanged",
    );
    assert_eq!(dispatched.meta.served_model, Some("opus".to_string()));
    assert!(
        !dispatched.meta.served_forwarded_credential,
        "an own target must never set the forwarded marker",
    );
}

#[tokio::test]
async fn stream_forwarded_target_records_client_model_as_served_upstream() {
    let (router, _spy) = router_with_leg("opus", "fwd-prov", "claude-opus-upstream", true);
    let dispatched = router
        .stream_with_options(forwarded_req_for("opus"), RouterOptions::default())
        .await;
    let _stream = dispatched
        .result
        .expect("forwarded stream target must dispatch");
    assert_eq!(dispatched.meta.served_upstream, Some("opus".to_string()));
    assert_eq!(dispatched.meta.served_model, Some("opus".to_string()));
    assert!(dispatched.meta.served_forwarded_credential);
}

#[tokio::test]
async fn stream_own_target_records_target_upstream_as_served_upstream_unchanged() {
    let (router, _spy) = router_with_leg("opus", "own-prov", "claude-opus-upstream", false);
    let dispatched = router
        .stream_with_options(req_for("opus"), RouterOptions::default())
        .await;
    let _stream = dispatched.result.expect("own stream target must dispatch");
    assert_eq!(
        dispatched.meta.served_upstream,
        Some("claude-opus-upstream".to_string()),
    );
    assert!(!dispatched.meta.served_forwarded_credential);
}
