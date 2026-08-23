//! Tests for the v0.6.0 dispatch path. Builds a router with an
//! installed `ResolvedModel` table and verifies dispatch walks
//! the chain correctly, including the "wire model maps to a
//! direct nickname" path and the "alias chain that references
//! an unknown nickname" startup-validation path (the latter
//! enforced at `install_resolved_models` callers in C4).
use super::*;
use crate::config::{ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::capability::EvidenceSource;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountedProvider {
    id: String,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for CountedProvider {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
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
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
}

fn router_with_resolved(table: Vec<(&str, &str, &str, Arc<dyn Provider>)>) -> Router {
    let cfg = Arc::new(Config::default());
    let mut router = Router::new(cfg);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, provider_name, upstream, p) in table {
        models.insert(
            nickname.to_string(),
            Arc::new(ResolvedModel::new(nickname, provider_name, p, upstream)),
        );
    }
    router.install_resolved_models(models);
    router
}

#[test]
fn reported_model_survives_config_resolved_dispatch_relay() {
    // Structural-relay sanity check: a configured `reported_model`
    // rides the 4-hop relay (ModelEntry -> ResolvedModel ->
    // DispatchTarget) including the seat-pinned dispatch path used by
    // pooled-OAuth models. The end-to-end BEHAVIOR coverage (that the
    // override actually surfaces in resp.model) lives in
    // `seat_backed_complete_honors_reported_model_override`.
    let entry =
        crate::config::ModelEntry::new("p1", "wire-model").with_reported_model("public-label");
    assert_eq!(entry.reported_model.as_deref(), Some("public-label"));

    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "p1".into(),
        calls: AtomicUsize::new(0),
    });
    let mut resolved = ResolvedModel::new("m1", "p1", p.clone(), "wire-model");
    if let Some(label) = entry.reported_model.as_ref() {
        resolved = resolved.with_reported_model(label.clone());
    }
    assert_eq!(resolved.reported_model.as_deref(), Some("public-label"));

    let m = Arc::new(resolved);
    let direct = into_one_dispatch_target(m.clone());
    assert_eq!(direct.reported_model.as_deref(), Some("public-label"));

    let seat = crate::seat_pool::SeatTarget {
        provider_name: "seat-a".to_string(),
        provider: p.clone(),
        auth_secret_ref: None,
    };
    let via_seat = dispatch_target_for_seat(&m, &seat, None);
    assert_eq!(via_seat.reported_model.as_deref(), Some("public-label"));
}

#[test]
fn visible_routectl_provider_survives_config_resolved_dispatch_relay() {
    // Structural-relay sanity check mirroring
    // `reported_model_survives_config_resolved_dispatch_relay`: a
    // configured `visible_routectl_provider=false` rides the 4-hop
    // relay (ModelEntry -> ResolvedModel -> DispatchTarget) including
    // the seat-pinned dispatch path. The end-to-end BEHAVIOR coverage
    // lives in `visible_routectl_provider_false_suppresses_field`.
    let entry =
        crate::config::ModelEntry::new("p1", "wire-model").with_visible_routectl_provider(false);
    assert!(!entry.visible_routectl_provider);

    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "p1".into(),
        calls: AtomicUsize::new(0),
    });
    let resolved = ResolvedModel::new("m1", "p1", p.clone(), "wire-model")
        .with_visible_routectl_provider(entry.visible_routectl_provider);
    assert!(!resolved.visible_routectl_provider);

    let m = Arc::new(resolved);
    let direct = into_one_dispatch_target(m.clone());
    assert!(!direct.visible_routectl_provider);

    let seat = crate::seat_pool::SeatTarget {
        provider_name: "seat-a".to_string(),
        provider: p.clone(),
        auth_secret_ref: None,
    };
    let via_seat = dispatch_target_for_seat(&m, &seat, None);
    assert!(!via_seat.visible_routectl_provider);
}

#[test]
fn seat_dispatch_target_carries_provider_kind() {
    // A seat-backed target must classify errors against the seat
    // provider's OWN kind, not the union table. `provider_kind` is
    // config-derived (a seat shares its model's provider entry), so
    // the chain expander resolves it from `provider_name` and threads
    // it onto every seat target -- not left `None`.
    let mut config = Config::default();
    config.providers.insert(
        "test-prov".into(),
        crate::config::ProviderEntry::anthropic_api("literal:k"),
    );
    let router = Router::new(Arc::new(config));

    let provider: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "test-prov".into(),
        calls: AtomicUsize::new(0),
    });
    let seats = vec![crate::seat_pool::SeatTarget {
        provider_name: "test-prov".to_string(),
        provider: provider.clone(),
        auth_secret_ref: None,
    }];
    let model = Arc::new(
        ResolvedModel::new("nick", "test-prov", provider, "claude-x").with_seats(seats.into()),
    );

    let targets = router.expand_chain_to_targets(vec![model], None);
    assert_eq!(targets.len(), 1, "one dispatch target per seat");
    for target in &targets {
        assert_eq!(
            target.provider_kind,
            Some("anthropic-api"),
            "seat target must carry its member entry's provider kind",
        );
    }
}

#[test]
fn expand_chain_to_targets_fills_class_overrides_from_provider_config() {
    // The provider's `[class_overrides]` table is adapted to canonical
    // `FailureClass` ONCE at chain expansion, mirroring the
    // `provider_kind` only-when-empty fill discipline. Uses the real
    // `ConfigFailureClass` adapter (`to_failure_class`), not a
    // hand-built `FailureClass`.
    use crate::class_policy::ConfigFailureClass;
    let mut entry = crate::config::ProviderEntry::anthropic_api("literal:k");
    if let crate::config::ProviderEntry::AnthropicApi { runtime, .. } = &mut entry {
        runtime
            .class_overrides
            .insert(503, ConfigFailureClass::ContentPolicy);
        runtime
            .class_overrides
            .insert(529, ConfigFailureClass::FeatureUnsupported);
    }
    let mut config = Config::default();
    config.providers.insert("test-prov".into(), entry);
    let router = Router::new(Arc::new(config));

    let provider: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "test-prov".into(),
        calls: AtomicUsize::new(0),
    });
    let model = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        provider,
        "claude-x",
    ));

    let targets = router.expand_chain_to_targets(vec![model], None);
    assert_eq!(targets.len(), 1);
    let target = &targets[0];
    assert_eq!(
        target.class_overrides.get(&503),
        Some(&FailureClass::ContentPolicy),
    );
    assert_eq!(
        target.class_overrides.get(&529),
        Some(&FailureClass::FeatureUnsupported {
            capability: crate::class_policy::OPERATOR_REMAP_CAPABILITY.to_string(),
        }),
    );
    assert!(!target.class_overrides.contains_key(&500));
}

#[test]
fn expand_chain_to_targets_leaves_class_overrides_empty_with_no_provider_config() {
    // No `[class_overrides]` on the provider entry (the default) must
    // leave every target's map empty -- the no-op case for `apply_remap`.
    let mut config = Config::default();
    config.providers.insert(
        "test-prov".into(),
        crate::config::ProviderEntry::anthropic_api("literal:k"),
    );
    let router = Router::new(Arc::new(config));

    let provider: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "test-prov".into(),
        calls: AtomicUsize::new(0),
    });
    let model = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        provider,
        "claude-x",
    ));

    let targets = router.expand_chain_to_targets(vec![model], None);
    assert!(targets[0].class_overrides.is_empty());
}

#[test]
fn visible_routectl_provider_defaults_true_across_relay() {
    // DEFAULT-TRUE guard: a model built without the override carries
    // `visible_routectl_provider=true` all the way to the dispatch
    // target, keeping existing consumers (which assert a present
    // `routectl_provider`) green.
    let entry = crate::config::ModelEntry::new("p1", "wire-model");
    assert!(entry.visible_routectl_provider);
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "p1".into(),
        calls: AtomicUsize::new(0),
    });
    let m = Arc::new(ResolvedModel::new("m1", "p1", p, "wire-model"));
    assert!(m.visible_routectl_provider);
    assert!(into_one_dispatch_target(m).visible_routectl_provider);
}

#[tokio::test]
async fn visible_routectl_provider_false_suppresses_field() {
    // SUPPRESS: a model with visible_routectl_provider=false yields a
    // response with NO `routectl_provider` (left None -> serde's
    // skip_serializing_if drops the field).
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "anthropic".into(),
        calls: AtomicUsize::new(0),
    });
    let cfg = Arc::new(Config::default());
    let mut router = Router::new(cfg);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "haiku".to_string(),
        Arc::new(
            ResolvedModel::new("haiku", "anthropic", p, "claude-haiku-4-5")
                .with_visible_routectl_provider(false),
        ),
    );
    router.install_resolved_models(models);

    let req = ChatRequest {
        model: "haiku".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert!(
        resp.routectl_provider.is_none(),
        "suppressed model must leave routectl_provider unset"
    );
    // The skip_serializing_if drops the absent field from the wire.
    let body = serde_json::to_value(&resp).expect("serialize");
    assert!(
        body.get("routectl_provider").is_none(),
        "routectl_provider must be absent from the serialized body"
    );
}

#[tokio::test]
async fn suppressed_provider_clears_prestamped_field() {
    // LEAK GUARD: concrete providers pre-stamp `routectl_provider`
    // with their own id before returning. CountedProvider returns
    // None and so cannot exercise the suppression gate's clearing
    // behavior. This provider returns Some("leaked-provider"); with
    // visible_routectl_provider=false the gate MUST clear it to None,
    // and the field MUST be absent from the serialized OpenAI body.
    struct PrestampProvider {
        id: String,
    }
    #[async_trait]
    impl Provider for PrestampProvider {
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
                // Pre-stamp, mirroring every concrete provider.
                routectl_provider: Some("leaked-provider".into()),
                extras: Default::default(),
                upstream_meta: None,
            })
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    let p: Arc<dyn Provider> = Arc::new(PrestampProvider {
        id: "anthropic".into(),
    });
    let cfg = Arc::new(Config::default());
    let mut router = Router::new(cfg);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "haiku".to_string(),
        Arc::new(
            ResolvedModel::new("haiku", "anthropic", p, "claude-haiku-4-5")
                .with_visible_routectl_provider(false),
        ),
    );
    router.install_resolved_models(models);

    let req = ChatRequest {
        model: "haiku".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert!(
        resp.routectl_provider.is_none(),
        "suppression must clear the provider's pre-stamped routectl_provider"
    );
    let body = serde_json::to_value(&resp).expect("serialize");
    assert!(
        body.get("routectl_provider").is_none(),
        "pre-stamped routectl_provider must be absent from the serialized body"
    );
}

#[tokio::test]
async fn suppressed_provider_still_records_dispatch_meta() {
    // ACCOUNTING GUARD: suppressing the client-visible field must NOT
    // affect internal accounting -- DispatchMeta still records
    // served_provider / served_upstream on the suppressed model.
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "anthropic".into(),
        calls: AtomicUsize::new(0),
    });
    let cfg = Arc::new(Config::default());
    let mut router = Router::new(cfg);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "haiku".to_string(),
        Arc::new(
            ResolvedModel::new("haiku", "anthropic", p, "claude-haiku-4-5")
                .with_visible_routectl_provider(false),
        ),
    );
    router.install_resolved_models(models);

    let req = ChatRequest {
        model: "haiku".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let dispatched = router
        .complete_with_options(req, RouterOptions::default())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(
        dispatched.meta.served_provider.as_deref(),
        Some("anthropic"),
        "served_provider must still be recorded when the field is suppressed"
    );
    assert_eq!(
        dispatched.meta.served_upstream.as_deref(),
        Some("claude-haiku-4-5"),
        "served_upstream must still be recorded when the field is suppressed"
    );
}

/// Minimal streaming-capable provider for the seat-path end-to-end
/// tests. Emits a text chunk followed by a usage-only terminal tail
/// chunk, mirroring a real provider; both carry the upstream wire id
/// in `model`, so the router's per-chunk relabel (including the
/// terminal chunk) is the only thing that can change it.
struct StreamingProvider {
    id: String,
}

#[async_trait]
impl Provider for StreamingProvider {
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
        let model = req.model;
        let id = self.id.clone();
        let text = ChatChunk {
            id: format!("chunk-{id}"),
            model: model.clone(),
            choices: vec![routectl_core::ChunkChoice {
                index: 0,
                delta: routectl_core::ChunkDelta {
                    content: Some("ok".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
            opaque_events: Vec::new(),
            upstream_meta: None,
        };
        let tail = ChatChunk {
            id: format!("chunk-{id}-tail"),
            model,
            choices: Vec::new(),
            usage: Some(routectl_core::UsageDelta::default()),
            opaque_events: Vec::new(),
            upstream_meta: None,
        };
        Ok(futures::stream::iter(vec![Ok(text), Ok(tail)]).boxed())
    }
}

/// Build a router whose single model nickname is pooled onto a fixed
/// set of seats. Mirrors the factory's seat-expansion path
/// (`ResolvedModel::with_seats`) so dispatch walks the seat-pinned
/// `DispatchTarget`s, the path used by pooled-OAuth models. An
/// optional `reported_model` override is threaded onto the model.
fn router_with_pooled_model(
    nickname: &str,
    pool_name: &str,
    upstream: &str,
    provider: Arc<dyn Provider>,
    members: &[&str],
    reported_model: Option<&str>,
) -> Router {
    let cfg = Arc::new(Config::default());
    let mut router = Router::new(cfg);

    let seats: Vec<crate::seat_pool::SeatTarget> = members
        .iter()
        .map(|member| crate::seat_pool::SeatTarget {
            provider_name: (*member).to_string(),
            provider: provider.clone(),
            auth_secret_ref: None,
        })
        .collect();

    let mut resolved =
        ResolvedModel::new(nickname, pool_name, provider, upstream).with_seats(seats.into());
    if let Some(label) = reported_model {
        resolved = resolved.with_reported_model(label);
    }

    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(nickname.to_string(), Arc::new(resolved));
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn seat_backed_complete_echoes_client_alias_by_default() {
    // A pooled (seat-backed) model with no `reported_model` override
    // must echo the client's requested alias in resp.model, even
    // though dispatch went through a seat-pinned DispatchTarget whose
    // upstream wire id differs from the alias.
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "oauth-pool".into(),
        calls: AtomicUsize::new(0),
    });
    let router = router_with_pooled_model(
        "opus",
        "anthropic-oauth",
        "claude-opus-4-7-wire",
        p.clone(),
        &["seat-a", "seat-b"],
        None,
    );
    let req = ChatRequest {
        model: "opus".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    // Default flip: the seat-served response echoes the requested
    // alias, not the upstream wire id.
    assert_eq!(resp.model, "opus");
}

#[tokio::test]
async fn seat_backed_complete_honors_reported_model_override() {
    // A pooled model WITH a `reported_model` override must surface
    // that override in resp.model on the seat-served path.
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "oauth-pool".into(),
        calls: AtomicUsize::new(0),
    });
    let router = router_with_pooled_model(
        "opus",
        "anthropic-oauth",
        "claude-opus-4-7-wire",
        p.clone(),
        &["seat-a", "seat-b"],
        Some("public-opus"),
    );
    let req = ChatRequest {
        model: "opus".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert_eq!(resp.model, "public-opus");
}

#[tokio::test]
async fn seat_backed_stream_relabels_chunk_model() {
    // The seat-served streaming path must relabel every chunk.model
    // to the client-visible label. Default (no override) echoes the
    // requested alias.
    let p: Arc<dyn Provider> = Arc::new(StreamingProvider {
        id: "oauth-pool".into(),
    });
    let router = router_with_pooled_model(
        "opus",
        "anthropic-oauth",
        "claude-opus-4-7-wire",
        p.clone(),
        &["seat-a", "seat-b"],
        None,
    );
    let req = ChatRequest {
        model: "opus".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let mut stream = router.stream(req).await.expect("stream opens");
    // Per-chunk relabel: every seat-served chunk, including the
    // usage-only terminal tail, carries the requested alias rather
    // than the upstream wire id the provider stamped.
    let mut count = 0;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("ok");
        assert_eq!(chunk.model, "opus");
        count += 1;
    }
    assert_eq!(count, 2, "text + terminal");
}

#[tokio::test]
async fn dispatch_resolves_wire_string_to_nickname_directly() {
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "anthropic-test".into(),
        calls: AtomicUsize::new(0),
    });
    let router = router_with_resolved(vec![("haiku", "anthropic", "claude-haiku-4-5", p.clone())]);
    let req = ChatRequest {
        model: "haiku".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic"));
    // Default flip: the response echoes the client's requested
    // alias, not the upstream wire model id.
    assert_eq!(resp.model, "haiku");
}

#[tokio::test]
async fn install_resolved_models_creates_runtime_state_per_nickname() {
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "p-test".into(),
        calls: AtomicUsize::new(0),
    });
    let router = router_with_resolved(vec![
        ("alpha", "p-shared", "u1", p.clone()),
        ("beta", "p-shared", "u2", p.clone()),
    ]);
    // Both nicknames present in the resolved table.
    assert!(router.resolved_models.contains_key("alpha"));
    assert!(router.resolved_models.contains_key("beta"));
    // v0.6.0 keys runtime state by nickname so two models on one
    // provider quarantine independently. Both nicknames must own
    // their own slot.
    assert!(router.state.contains_key("alpha"));
    assert!(router.state.contains_key("beta"));
}

#[tokio::test]
async fn status_targets_one_entry_per_nickname_for_non_pooled() {
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "p-test".into(),
        calls: AtomicUsize::new(0),
    });
    let router = router_with_resolved(vec![
        ("alpha", "p-shared", "u1", p.clone()),
        ("beta", "p-shared", "u2", p.clone()),
    ]);
    let targets = router.status_targets(Instant::now());
    assert_eq!(targets.len(), 2, "one entry per non-pooled nickname");
    let alpha = targets
        .iter()
        .find(|t| t.nickname == "alpha")
        .expect("alpha present");
    assert_eq!(alpha.state_key, "alpha");
    assert_eq!(alpha.provider_name, "p-shared");
    assert_eq!(alpha.upstream, "u1");
    assert_eq!(alpha.seat_label, None);
    // A fresh, unconfigured gate reads Closed with no probe in flight.
    assert_eq!(alpha.gate.circuit, CircuitPhase::Closed);
    assert!(!alpha.gate.half_open_probe_in_flight);
}

#[tokio::test]
async fn status_targets_one_entry_per_seat_for_pooled() {
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "oauth-pool".into(),
        calls: AtomicUsize::new(0),
    });
    let router = router_with_pooled_model(
        "opus",
        "anthropic-pool",
        "claude-opus-4-7-wire",
        p.clone(),
        &["anthropic-a", "anthropic-b"],
        None,
    );
    let targets = router.status_targets(Instant::now());
    assert_eq!(targets.len(), 2, "one entry per seat of a pooled model");
    let mut keys: Vec<&str> = targets.iter().map(|t| t.state_key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["opus#anthropic-a", "opus#anthropic-b"]);
    let mut members: Vec<&str> = targets.iter().map(|t| t.provider_name.as_str()).collect();
    members.sort_unstable();
    assert_eq!(
        members,
        vec!["anthropic-a", "anthropic-b"],
        "a seat entry names the MEMBER it dispatches, not the pool"
    );
    for t in &targets {
        assert_eq!(t.nickname, "opus");
        assert_eq!(t.upstream, "claude-opus-4-7-wire");
        assert!(
            t.seat_label.is_some(),
            "seat entries carry their member identity"
        );
    }
}

#[tokio::test]
async fn status_targets_missing_state_slot_fails_safe_open() {
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "p-test".into(),
        calls: AtomicUsize::new(0),
    });
    let mut router = router_with_resolved(vec![("gamma", "p-shared", "u1", p.clone())]);
    // Drop the state slot: the resolved-model entry survives but has no
    // runtime gate. status_targets must not panic and must fail safe.
    router.state.remove("gamma");
    let targets = router.status_targets(Instant::now());
    assert_eq!(targets.len(), 1);
    let gamma = &targets[0];
    assert_eq!(gamma.state_key, "gamma");
    assert_eq!(
        gamma.gate.circuit,
        CircuitPhase::Open,
        "a target with no state slot fails safe to Open",
    );
    assert!(!gamma.gate.half_open_probe_in_flight);
    assert_eq!(gamma.gate.rpm_available, None);
}

#[tokio::test]
async fn learned_capability_snapshot_surfaces_negatives() {
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "p-test".into(),
        calls: AtomicUsize::new(0),
    });
    let router = router_with_resolved(vec![("alpha", "openai-compat", "u1", p.clone())]);
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "fresh registry surfaces no negatives",
    );
    router.learned_capabilities.observe(
        "alpha",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        routectl_core::capability::FailurePhase::F1,
        EvidenceSource::Live,
        Instant::now(),
    );
    let snap = router.learned_capability_snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].state_key, "alpha");
    assert_eq!(snap[0].feature_key, "web_search");
    assert_eq!(snap[0].signal_tier, SignalTier::SelfIdentifying);
}

#[tokio::test]
async fn status_targets_does_not_claim_half_open_probe_slot() {
    // THE non-perturbation guard, at the router seam. Drive one seat to
    // HalfOpenReady, hammer status_targets (serially AND concurrently),
    // and assert the read never claims the probe slot: every entry stays
    // HalfOpenReady with half_open_probe_in_flight == false. Only THEN
    // does a real try_dispatch claim the probe.
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "oauth-pool".into(),
        calls: AtomicUsize::new(0),
    });
    let router = Arc::new(router_with_pooled_model(
        "opus",
        "anthropic-oauth",
        "claude-opus-4-7-wire",
        p.clone(),
        &["seat-a", "seat-b"],
        None,
    ));

    // Park seat-a's breaker; compute an instant past its cooldown so the
    // gate reads HalfOpenReady without any probe in flight.
    let t0 = Instant::now();
    assert!(
        router.force_open_breaker("opus#seat-a", Duration::from_millis(500)),
        "seat-a must own a state slot",
    );
    let t_ready = t0 + Duration::from_millis(600);

    let seat_a_ready = |targets: &[RouteTargetStatus]| {
        let seat = targets
            .iter()
            .find(|t| t.state_key == "opus#seat-a")
            .expect("seat-a present");
        assert_eq!(seat.gate.circuit, CircuitPhase::HalfOpenReady);
        assert!(!seat.gate.half_open_probe_in_flight);
    };

    // Serial hammering.
    for _ in 0..100 {
        seat_a_ready(&router.status_targets(t_ready));
    }

    // Concurrent hammering.
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let r = Arc::clone(&router);
            scope.spawn(move || {
                for _ in 0..100 {
                    let targets = r.status_targets(t_ready);
                    let seat = targets
                        .iter()
                        .find(|t| t.state_key == "opus#seat-a")
                        .expect("seat-a present");
                    assert_eq!(seat.gate.circuit, CircuitPhase::HalfOpenReady);
                    assert!(!seat.gate.half_open_probe_in_flight);
                }
            });
        }
    });

    // The reads never perturbed the slot: a real dispatch still gets the
    // probe and claims it.
    let slot = router.state.get("opus#seat-a").expect("slot present");
    assert_eq!(slot.lock().try_dispatch(t_ready), GateDecision::Allow);
    assert!(
        slot.lock().half_open_probe_in_flight(),
        "the real dispatch claimed the probe slot the reads left untouched",
    );
}

#[tokio::test]
async fn per_model_breaker_isolates_failures() {
    // Pin: when two models share one provider entry, tripping
    // model A's breaker does NOT block model B from dispatching.
    // Pre-rc.2 this regressed because state was keyed by provider
    // name (one breaker shared across all models on that provider).
    struct AlwaysFailing {
        id: String,
    }
    #[async_trait]
    impl Provider for AlwaysFailing {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            Err(Error::upstream(&self.id, 0, "always fails"))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    // Provider with a 1-failure breaker. Both models share it.
    let mut config = Config::default();
    config.providers.insert(
        "p-shared".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                circuit_failures: Some(1),
                circuit_cooldown_ms: Some(60_000),
                ..Default::default()
            },
        },
    );

    let mut router = Router::new(Arc::new(config));
    let p_a: Arc<dyn Provider> = Arc::new(AlwaysFailing { id: "a".into() });
    let p_b: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "b".into(),
        calls: AtomicUsize::new(0),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "alpha".into(),
        Arc::new(ResolvedModel::new("alpha", "p-shared", p_a, "u1")),
    );
    models.insert(
        "beta".into(),
        Arc::new(ResolvedModel::new("beta", "p-shared", p_b, "u2")),
    );
    router.install_resolved_models(models);

    // Trip alpha's breaker: one failed dispatch puts it Open.
    let req_a = ChatRequest {
        model: "alpha".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let _ = router.complete(req_a).await; // failure, breaker trips

    // Beta MUST still be routable. Pre-fix (state keyed by
    // provider) this returned a circuit_breaker gate-block error.
    let req_b = ChatRequest {
        model: "beta".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req_b).await.expect(
        "beta dispatch must succeed even though alpha's breaker is tripped; \
             same-provider models must not share a breaker",
    );
    assert_eq!(resp.routectl_provider.as_deref(), Some("p-shared"));
}

#[test]
fn dispatch_chain_unknown_nickname_returns_unknown_alias() {
    // When the wire model isn't a known nickname AND has no
    // alias-table hit, dispatch_chain returns UnknownAlias.
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "p".into(),
        calls: AtomicUsize::new(0),
    });
    let router = router_with_resolved(vec![("haiku", "anthropic", "u", p)]);
    let res = router.dispatch_chain("does-not-exist", None);
    assert!(matches!(res, Err(Error::UnknownAlias(_))));
}

#[tokio::test]
async fn alias_entry_shadows_direct_model_nickname() {
    // Pin: when the same string is both a `[models.X]` nickname
    // AND an `[aliases]` key, the alias wins. Operators rely on
    // this to prepend a fallback chain to an existing model
    // without renaming the nickname.
    let p_direct: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "direct-p".into(),
        calls: AtomicUsize::new(0),
    });
    let p_via_alias: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "alias-p".into(),
        calls: AtomicUsize::new(0),
    });
    // Build a config where "foo" is both a nickname AND an alias
    // pointing at a different nickname. Dispatch must hit the
    // alias's target, not the direct nickname.
    let mut config = Config::default();
    config
        .aliases
        .insert("foo".into(), AliasValue::Single("backup".into()));
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "foo".into(),
        Arc::new(ResolvedModel::new(
            "foo",
            "p-direct",
            p_direct.clone(),
            "u-direct",
        )),
    );
    models.insert(
        "backup".into(),
        Arc::new(ResolvedModel::new(
            "backup",
            "p-alias",
            p_via_alias.clone(),
            "u-alias",
        )),
    );
    router.install_resolved_models(models);

    let req = ChatRequest {
        model: "foo".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    // Alias wins: dispatch landed on the `backup` model's
    // provider, not the direct `foo` model's provider.
    assert_eq!(resp.routectl_provider.as_deref(), Some("p-alias"));
    // Default flip: the response echoes the client's requested
    // alias (`foo`), not the served upstream wire model id.
    assert_eq!(resp.model, "foo");
}

// ----- Recursive alias-chain resolution (Task #5) -----
//
// Pin the runtime DFS expansion: an alias entry that is itself an
// alias key gets recursively expanded inline so the operator's
// stated fallback order is preserved. Globs follow the same rule
// as exact matches. The depth cap is exercised via a forced cycle
// (whose static walk would normally have rejected it) to confirm
// the belt-and-suspenders runtime guard fires.

fn make_provider(id: &str) -> Arc<dyn Provider> {
    Arc::new(CountedProvider {
        id: id.to_string(),
        calls: AtomicUsize::new(0),
    })
}

/// Build a `Router` whose alias map references both alias keys
/// and model nicknames (so the recursive resolver has something
/// to walk). `aliases` is a slice of `(key, AliasValue)` pairs;
/// `models` is a slice of `(nickname, provider_name, upstream)`
/// tuples. Every provider name in `models` gets a fresh
/// `CountedProvider` instance, so the test can assert which model
/// landed in the dispatch chain by reading
/// `resp.routectl_provider`.
fn router_with_recursive_aliases(
    aliases: &[(&str, AliasValue)],
    models: &[(&str, &str, &str)],
) -> Router {
    let mut config = Config::default();
    for (key, value) in aliases {
        config.aliases.insert((*key).into(), value.clone());
    }
    let mut router = Router::new(Arc::new(config));
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, provider_name, upstream) in models {
        let provider = make_provider(provider_name);
        resolved.insert(
            (*nickname).into(),
            Arc::new(ResolvedModel::new(
                *nickname,
                *provider_name,
                provider,
                *upstream,
            )),
        );
    }
    router.install_resolved_models(resolved);
    router
}

#[tokio::test]
async fn alias_pointing_to_another_alias_resolves_two_deep() {
    // A = ["B"], B = ["model-x"]. Wire model "a" must dispatch
    // to model-x's provider (one hop through B).
    let router = router_with_recursive_aliases(
        &[
            ("a", AliasValue::Single("b".into())),
            ("b", AliasValue::Single("model-x".into())),
        ],
        &[("model-x", "p-x", "u-x")],
    );
    let req = ChatRequest {
        model: "a".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p-x"));
    // Default flip: the response echoes the client's requested
    // wire model (`a`), not the resolved upstream id (`u-x`).
    assert_eq!(resp.model, "a");
}

#[tokio::test]
async fn alias_three_deep_resolves_to_full_chain() {
    // A = ["B"], B = ["C"], C = ["model-x", "model-y"]. Wire
    // model "a" should dispatch to model-x first; if model-x
    // were absent, would fall back to model-y. We just confirm
    // the head of the resolved chain.
    let router = router_with_recursive_aliases(
        &[
            ("a", AliasValue::Single("b".into())),
            ("b", AliasValue::Single("c".into())),
            (
                "c",
                AliasValue::Chain(vec!["model-x".into(), "model-y".into()]),
            ),
        ],
        &[("model-x", "p-x", "u-x"), ("model-y", "p-y", "u-y")],
    );
    let req = ChatRequest {
        model: "a".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p-x"));
}

#[test]
fn alias_chain_preserves_fallback_order_across_recursion() {
    // A = ["B", "model-c"], B = ["model-d", "model-e"]. Static
    // expansion must yield [model-d, model-e, model-c] -- B's
    // chain expanded inline before C, preserving the operator's
    // stated fallback order. We test via dispatch_chain directly
    // to inspect ordering without bringing up the full async
    // dispatch loop.
    let router = router_with_recursive_aliases(
        &[
            ("a", AliasValue::Chain(vec!["b".into(), "model-c".into()])),
            (
                "b",
                AliasValue::Chain(vec!["model-d".into(), "model-e".into()]),
            ),
        ],
        &[
            ("model-c", "p-c", "u-c"),
            ("model-d", "p-d", "u-d"),
            ("model-e", "p-e", "u-e"),
        ],
    );
    let chain = router.dispatch_chain("a", None).expect("dispatch_chain ok");
    let upstreams: Vec<&str> = chain.iter().map(|t| t.upstream.as_str()).collect();
    assert_eq!(
        upstreams,
        vec!["u-d", "u-e", "u-c"],
        "B's chain must expand inline before C, preserving fallback order"
    );
}

#[test]
fn dry_single_pointer_alias_resolves_to_underlying_model() {
    // The DRY operator-config pattern from the spec:
    // `a = ["model-x"]`, `claude-a = ["a"]`. Both wire models
    // must dispatch to model-x. This is the shape that lets the
    // operator collapse the inline-duplicated `claude-cheap`,
    // `claude-codex-pro`, etc. wrappers in the user config.
    let router = router_with_recursive_aliases(
        &[
            ("a", AliasValue::Single("model-x".into())),
            ("claude-a", AliasValue::Single("a".into())),
        ],
        &[("model-x", "p-x", "u-x")],
    );

    let chain_a = router.dispatch_chain("a", None).expect("a resolves");
    assert_eq!(chain_a.len(), 1);
    assert_eq!(chain_a[0].upstream, "u-x");

    let chain_claude = router
        .dispatch_chain("claude-a", None)
        .expect("claude-a resolves");
    assert_eq!(chain_claude.len(), 1);
    assert_eq!(chain_claude[0].upstream, "u-x");
}

#[test]
fn glob_alias_expands_through_nested_alias() {
    // Per architect's verdict F: glob keys follow the same
    // recursion rule as exact aliases. `claude-haiku*` -> `a` ->
    // `model-x`. A wire model "claude-haiku-3" hits the glob and
    // must resolve through `a` to model-x's provider.
    let router = router_with_recursive_aliases(
        &[
            ("claude-haiku*", AliasValue::Single("a".into())),
            ("a", AliasValue::Single("model-x".into())),
        ],
        &[("model-x", "p-x", "u-x")],
    );
    let chain = router
        .dispatch_chain("claude-haiku-3", None)
        .expect("glob match resolves");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].upstream, "u-x");
}

#[test]
fn recursion_depth_cap_fires_on_cycle_at_dispatch_time() {
    // Belt-and-suspenders: if the static walk somehow missed a
    // cycle (e.g. operator hot-edited the live Config without
    // re-running validation), the runtime resolver must fail
    // fast with `Error::Config` rather than recurse forever.
    // We force the case here by building a router with a
    // self-cycle directly (skipping `validate_alias_chain_targets`).
    let router = router_with_recursive_aliases(&[("a", AliasValue::Single("a".into()))], &[]);
    let res = router.dispatch_chain("a", None);
    match res {
        Err(Error::Config(msg)) => {
            assert!(
                msg.contains("recursion exceeded depth"),
                "expected depth-cap error, got: {msg}"
            );
        }
        Err(other) => panic!("expected Error::Config from depth cap, got {other:?}"),
        Ok(_) => panic!("expected Error::Config from depth cap, got Ok(...)"),
    }
}

// ---- Learned-capability act path (soft-drop + probe admission) ----

fn learned_provider_config() -> Arc<Config> {
    let mut config = Config::default();
    config.providers.insert(
        "test-prov".into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    Arc::new(config)
}

fn learned_target(router: &Router, nickname: &str) -> DispatchTarget {
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "test-prov".into(),
        calls: AtomicUsize::new(0),
    });
    let model = ResolvedModel::new(nickname, "test-prov", p, "claude-x");
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

#[test]
fn filter_source_learned_stringifies_to_contract_token() {
    assert_eq!(FilterSource::Learned.as_str(), "learned");
}

#[test]
fn learned_negative_deprioritizes_target_to_tail() {
    let router = Router::new(learned_provider_config());
    let front = learned_target(&router, "front");
    let back = learned_target(&router, "back");
    router.learned_capabilities.observe(
        "front",
        "web_search",
        "anthropic-api",
        routectl_core::capability::SignalTier::SelfIdentifying,
        routectl_core::capability::FailurePhase::F1,
        EvidenceSource::Live,
        std::time::Instant::now(),
    );
    let features = vec!["web_search".to_string()];

    let mut out = Vec::new();
    let events = routectl_testkit::capture_events(|| {
        out = router
            .filter_chain_by_features(vec![front, back], &features, "alias", &mut Vec::new())
            .expect("a supported survivor keeps the chain non-empty");
    });

    // Result = [supported...] ++ [learned tail]: back survives up front,
    // the learned-negative "front" is de-prioritized to the tail.
    let order: Vec<&str> = out.iter().map(|t| t.state_key.as_str()).collect();
    assert_eq!(order, vec!["back", "front"]);
    assert_eq!(
        router.metrics.d17_tail_total(),
        0,
        "a supported survivor is not a tail-only entry",
    );
    // A healthy alternative remains, so the demotion emits route_away at
    // INFO (not WARN) carrying the unified state_key / capability_key.
    let info = events
        .iter()
        .find(|e| e.field("event") == Some("route_away"))
        .expect("a tail demotion must emit a route_away event");
    assert_eq!(info.level, tracing::Level::INFO);
    assert_eq!(info.field("state_key"), Some("front"));
    assert_eq!(info.field("capability_key"), Some("web_search"));
    assert!(
        !events
            .iter()
            .any(|e| e.field("event") == Some("route_away") && e.level == tracing::Level::WARN),
        "a surviving alternative must not raise route_away to WARN",
    );
}

#[test]
fn static_unsupported_emptying_chain_returns_not_implemented() {
    // The model-static list lives in config.models: the override
    // registry is built from config.
    let mut config = Config::default();
    config.providers.insert(
        "test-prov".into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config.models.insert(
        "only".into(),
        crate::config::ModelEntry::new("test-prov", "claude-x")
            .with_unsupported_features(vec!["web_search".to_string()]),
    );
    let router = Router::new(Arc::new(config));
    let only = learned_target(&router, "only");
    let features = vec!["web_search".to_string()];

    let result = router.filter_chain_by_features(vec![only], &features, "alias", &mut Vec::new());

    assert!(
        matches!(result, Err(Error::NotImplemented(..))),
        "a static hard-drop of the whole chain must fail",
    );
}

#[test]
fn sole_learned_tail_target_still_attempts_and_counts_d17() {
    let router = Router::new(learned_provider_config());
    let only = learned_target(&router, "only");
    router.learned_capabilities.observe(
        "only",
        "web_search",
        "anthropic-api",
        routectl_core::capability::SignalTier::SelfIdentifying,
        routectl_core::capability::FailurePhase::F1,
        EvidenceSource::Live,
        std::time::Instant::now(),
    );
    let features = vec!["web_search".to_string()];

    let events = routectl_testkit::capture_events(|| {
        let out = router
            .filter_chain_by_features(vec![only], &features, "alias", &mut Vec::new())
            .expect("a learned-only chain proceeds into the de-prioritized tail");
        assert_eq!(out.len(), 1, "the sole tail target is still attempted");
        assert_eq!(out[0].state_key, "only");
    });

    assert_eq!(router.metrics.d17_tail_total(), 1);
    let warn = events
        .iter()
        .find(|e| e.level == tracing::Level::WARN)
        .expect("entering the learned tail must WARN");
    assert_eq!(warn.field("event"), Some("route_away"));
    assert_eq!(warn.field("state_key"), Some("only"));
    assert_eq!(warn.field("capability_key"), Some("web_search"));
}

#[test]
fn kill_switch_off_skips_the_learned_consult() {
    let mut config = Config::default();
    config.capability.enabled = false;
    config.providers.insert(
        "test-prov".into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    let router = Router::new(Arc::new(config));
    let front = learned_target(&router, "front");
    let back = learned_target(&router, "back");
    router.learned_capabilities.observe(
        "front",
        "web_search",
        "anthropic-api",
        routectl_core::capability::SignalTier::SelfIdentifying,
        routectl_core::capability::FailurePhase::F1,
        EvidenceSource::Live,
        std::time::Instant::now(),
    );
    let features = vec!["web_search".to_string()];

    let out = router
        .filter_chain_by_features(vec![front, back], &features, "alias", &mut Vec::new())
        .expect("kill switch off leaves the chain intact");

    // The learned negative is ignored: original order, nothing tailed.
    let order: Vec<&str> = out.iter().map(|t| t.state_key.as_str()).collect();
    assert_eq!(order, vec!["front", "back"]);
    assert_eq!(router.metrics.d17_tail_total(), 0);
}

#[test]
fn expired_learned_negative_admits_one_probe_through_filter() {
    // A zero decay window makes a negative expired the instant it is
    // observed, so the next filter pass claims the single re-probe slot.
    let mut config = Config::default();
    config.capability.decay_hours = 0;
    config.providers.insert(
        "test-prov".into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    let router = Router::new(Arc::new(config));
    let only = learned_target(&router, "only");
    router.learned_capabilities.observe(
        "only",
        "web_search",
        "anthropic-api",
        routectl_core::capability::SignalTier::SelfIdentifying,
        routectl_core::capability::FailurePhase::F1,
        EvidenceSource::Live,
        std::time::Instant::now(),
    );
    let features = vec!["web_search".to_string()];

    // First pass: the lapsed negative admits a probe -> the target stays
    // in the supported set (routed to), and the probe is counted.
    let out = router
        .filter_chain_by_features(vec![only.clone()], &features, "alias", &mut Vec::new())
        .expect("an admitted probe routes to the target");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].state_key, "only");
    assert_eq!(router.metrics.probe_attempts_total(), 1);
    assert_eq!(router.metrics.d17_tail_total(), 0);

    // Second pass: the probe slot is claimed (in_flight) -> concurrent
    // lookups keep routing away, landing the target in the tail.
    let out2 = router
        .filter_chain_by_features(vec![only], &features, "alias", &mut Vec::new())
        .expect("a claimed in-flight probe routes away into the tail");
    assert_eq!(out2.len(), 1);
    assert_eq!(
        router.metrics.probe_attempts_total(),
        1,
        "exactly one probe is admitted per decay lapse",
    );
    assert_eq!(router.metrics.d17_tail_total(), 1);
}

#[test]
fn dispatch_target_carries_catalog_capability_prior() {
    use crate::catalog::{CatalogRow, EffectiveRow, Source};

    let router = Router::new(learned_provider_config());
    let mut row = CatalogRow::sentinel();
    row.capabilities.insert("web_search".to_string(), false);
    let p: Arc<dyn Provider> = Arc::new(CountedProvider {
        id: "test-prov".into(),
        calls: AtomicUsize::new(0),
    });
    let model = ResolvedModel::new("only", "test-prov", p, "claude-x").with_effective_row(
        EffectiveRow::Present {
            row,
            source: Source::Baked,
            verified_at: "2026-01-01".to_string(),
        },
    );

    let target = router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target");

    // Present key returns the prior; an absent key is NO PRIOR (None),
    // distinct from Some(false). No filter consumes it yet.
    assert_eq!(target.capability_prior("web_search"), Some(false));
    assert_eq!(target.capability_prior("computer_use"), None);
}

// ---- context_window_for: the /v1/models discovery read ----
//
// The window the discovery payload reports and the window the proactive
// gate acts on come from ONE accessor
// (`ResolvedModel::context_window_tokens`), so these tests pin the
// resolution rules around it: first chain entry, no `default` fallback,
// and a zero window degrading to unknown on both surfaces.

/// A `Present` effective row confirming `window` tokens, or leaving the
/// window unset when `window` is `None`.
fn window_row(window: Option<u32>) -> crate::catalog::EffectiveRow {
    use crate::catalog::{CatalogRow, EffectiveRow, Source};
    let mut row = CatalogRow::sentinel();
    row.max_context_tokens = window;
    EffectiveRow::Present {
        row,
        source: Source::Baked,
        verified_at: "seed".to_string(),
    }
}

/// A router whose `[aliases] chain` names `models` in the given order, each
/// nickname resolved onto provider `p` with the stated context window
/// stamped on its effective row. `aliases` adds further alias keys (used to
/// pin that a configured `default` is NOT consulted).
fn router_with_window(models: &[(&str, Option<u32>)], aliases: &[(&str, AliasValue)]) -> Router {
    let mut config = Config::default();
    config
        .providers
        .insert("p".into(), ProviderEntry::anthropic_api("literal:k"));
    config.aliases.insert(
        "chain".into(),
        AliasValue::Chain(models.iter().map(|(n, _)| (*n).to_string()).collect()),
    );
    for (key, value) in aliases {
        config.aliases.insert((*key).into(), value.clone());
    }
    for (nickname, _) in models {
        config.models.insert(
            (*nickname).into(),
            crate::config::ModelEntry::new("p", *nickname),
        );
    }
    let mut router = Router::new(Arc::new(config));
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, window) in models {
        resolved.insert(
            (*nickname).into(),
            Arc::new(
                ResolvedModel::new(*nickname, "p", make_provider("p"), *nickname)
                    .with_effective_row(window_row(*window)),
            ),
        );
    }
    router.install_resolved_models(resolved);
    router
}

/// A request whose estimate is far above the estimator's own granularity,
/// so the windows derived from it below pin the gate's RATIO rather than a
/// byte count.
fn oversized_request() -> ChatRequest {
    ChatRequest {
        model: "chain".into(),
        messages: vec![Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("window-gate-filler".repeat(2_000)),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    }
}

#[test]
fn context_window_for_reports_the_first_chain_targets_window() {
    // FIRST-CONFIGURED-TARGET rule: an alias chain reports its head's
    // window, not the tail's, and not the largest in the chain.
    let router = router_with_window(&[("head", Some(200_000)), ("tail", Some(1_000_000))], &[]);

    assert_eq!(router.context_window_for("chain"), Some(200_000));
    // A direct `[models]` nickname resolves through the same accessor.
    assert_eq!(router.context_window_for("tail"), Some(1_000_000));
}

#[test]
fn context_window_for_is_none_for_an_unset_window_or_an_unlisted_id() {
    // An unset window is UNKNOWN, never a fabricated figure -- and no
    // `default` catch-all fallback: an id that is neither an alias key nor a
    // nickname resolves to nothing, even with a `default` alias configured.
    let router = router_with_window(
        &[("unset", None), ("known", Some(128_000))],
        &[("default", AliasValue::Single("known".into()))],
    );

    assert_eq!(router.context_window_for("unset"), None);
    assert_eq!(
        router.context_window_for("not-a-configured-id"),
        None,
        "the `default` catch-all must not answer for an unlisted id",
    );
    // Positive control: the fixture DOES report a window for a listed id, so
    // the two Nones above are about resolution, not a broken fixture.
    assert_eq!(router.context_window_for("known"), Some(128_000));
}

#[test]
fn a_zero_window_degrades_to_unknown_on_discovery_and_keeps_the_gate_target() {
    // Defense-in-depth weld: validation rejects `Some(0)`, but if one slipped
    // through, BOTH surfaces must read it as unknown -- discovery omits the
    // figure and the gate keeps the target, rather than skipping a target
    // every request is nominally "too large" for.
    let req = oversized_request();
    let comfortably_large =
        u32::try_from(crate::context_trim::estimate_total_tokens(&req) * 8).expect("fits u32");
    let router = router_with_window(
        &[("zero", Some(0)), ("large", Some(comfortably_large))],
        &[],
    );

    assert_eq!(router.context_window_for("chain"), None);
    assert_eq!(router.context_window_for("zero"), None);

    let chain = router
        .dispatch_chain("chain", None)
        .expect("chain resolves");
    let kept = router.filter_chain_by_window(chain, &req);
    let nicknames: Vec<&str> = kept
        .iter()
        .map(|t| t.nickname.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        nicknames,
        vec!["zero", "large"],
        "a zero window is unconfirmed, so the gate keeps the target",
    );
    assert_eq!(router.metrics.window_gate_skips_total(), 0);
}

#[test]
fn context_window_for_is_none_for_a_configured_model_missing_from_the_installed_table() {
    // SERVABILITY-SHAPED READ: `[models]` is config, the installed table is
    // what the build actually produced. A model whose provider failed to
    // build is absent from the table, so discovery has no dispatch target to
    // report a window off and must omit the figure -- even though the config
    // entry (and its catalog cell) still exists. The /v1/models entry itself
    // stays listed; only the enrichment is suppressed.
    let mut config = Config::default();
    config
        .providers
        .insert("p".into(), ProviderEntry::anthropic_api("literal:k"));
    for nickname in ["installed", "failed-to-build"] {
        config.models.insert(
            nickname.into(),
            crate::config::ModelEntry::new("p", nickname),
        );
    }
    config.aliases.insert(
        "alias-to-failed".into(),
        AliasValue::Single("failed-to-build".into()),
    );

    // Only the healthy model reaches the table -- exactly the shape
    // `build_resolved_models` leaves behind when a provider build fails.
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    resolved.insert(
        "installed".into(),
        Arc::new(
            ResolvedModel::new("installed", "p", make_provider("p"), "installed")
                .with_effective_row(window_row(Some(128_000))),
        ),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(resolved);

    assert_eq!(
        router.context_window_for("failed-to-build"),
        None,
        "a config-only nickname has no dispatch target, so its window is unknown",
    );
    assert_eq!(
        router.context_window_for("alias-to-failed"),
        None,
        "an alias whose whole chain failed to build reports no window either",
    );
    // Positive control: the read DOES answer for a model that is installed,
    // so the two Nones above are about the missing table entry rather than a
    // fixture that reports nothing at all.
    assert_eq!(router.context_window_for("installed"), Some(128_000));
}

// ---- first_target_oauth_id: the oauth-id read backing enrichment suppression ----

#[test]
fn first_target_oauth_id_reports_the_bare_oauth_id() {
    let mut config = Config::default();
    config.providers.insert(
        "p".into(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    config.models.insert(
        "claude".into(),
        crate::config::ModelEntry::new("p", "claude"),
    );
    let mut router = Router::new(Arc::new(config));
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    resolved.insert(
        "claude".into(),
        Arc::new(ResolvedModel::new(
            "claude",
            "p",
            make_provider("p"),
            "claude",
        )),
    );
    router.install_resolved_models(resolved);

    assert_eq!(
        router.first_target_oauth_id("claude"),
        Some("anthropic".to_string())
    );
}

#[test]
fn first_target_oauth_id_strips_a_seat_label_suffix() {
    let mut config = Config::default();
    config.providers.insert(
        "p".into(),
        ProviderEntry::anthropic_api("oauth://anthropic#work"),
    );
    config.models.insert(
        "claude".into(),
        crate::config::ModelEntry::new("p", "claude"),
    );
    let mut router = Router::new(Arc::new(config));
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    resolved.insert(
        "claude".into(),
        Arc::new(ResolvedModel::new(
            "claude",
            "p",
            make_provider("p"),
            "claude",
        )),
    );
    router.install_resolved_models(resolved);

    assert_eq!(
        router.first_target_oauth_id("claude"),
        Some("anthropic".to_string()),
        "a pool seat label after '#' must not leak into the reported oauth id",
    );
}

#[test]
fn first_target_oauth_id_is_none_for_an_api_key_or_env_ref() {
    let mut config = Config::default();
    config
        .providers
        .insert("p".into(), ProviderEntry::anthropic_api("literal:k"));
    config.models.insert(
        "claude".into(),
        crate::config::ModelEntry::new("p", "claude"),
    );
    let mut router = Router::new(Arc::new(config));
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    resolved.insert(
        "claude".into(),
        Arc::new(ResolvedModel::new(
            "claude",
            "p",
            make_provider("p"),
            "claude",
        )),
    );
    router.install_resolved_models(resolved);

    assert_eq!(
        router.first_target_oauth_id("claude"),
        None,
        "positive control: the same fixture with a non-oauth ref must not name an id",
    );
}

#[test]
fn the_gate_and_discovery_read_the_same_overlay_corrected_window() {
    // The weld, against the REAL overlay merge
    // (`factory::apply_catalog_overlay`) rather than a hand-stamped row: an
    // operator overlay that shrinks the window must move the discovery
    // figure AND the gate decision together. Near-tautological while both
    // read one accessor -- it pins that against a future fork.
    use crate::catalog_overlay::{CatalogOverlay, OverlayCell, OverlaySource};

    let req = oversized_request();
    let overlay_window =
        u32::try_from(crate::context_trim::estimate_total_tokens(&req) / 2).expect("fits u32");

    let mut config = Config::default();
    config
        .providers
        .insert("p".into(), ProviderEntry::anthropic_api("literal:k"));
    config.aliases.insert(
        "chain".into(),
        AliasValue::Chain(vec!["shrunk".into(), "unconfirmed".into()]),
    );
    config.models.insert(
        "shrunk".into(),
        crate::config::ModelEntry::new("p", "shrunk-upstream"),
    );
    config.models.insert(
        "unconfirmed".into(),
        crate::config::ModelEntry::new("p", "unconfirmed-upstream"),
    );

    let mut overlay = CatalogOverlay::default();
    overlay.cells.insert(
        "anthropic-api:shrunk-upstream".to_string(),
        Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-08-21".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: Some(overlay_window),
            max_output_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );

    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    resolved.insert(
        "shrunk".into(),
        Arc::new(ResolvedModel::new(
            "shrunk",
            "p",
            make_provider("p"),
            "shrunk-upstream",
        )),
    );
    resolved.insert(
        "unconfirmed".into(),
        Arc::new(ResolvedModel::new(
            "unconfirmed",
            "p",
            make_provider("p"),
            "unconfirmed-upstream",
        )),
    );
    let stamped = crate::factory::apply_catalog_overlay(resolved, &config, &overlay);

    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(stamped);

    // Discovery reports the overlay-corrected window...
    assert_eq!(router.context_window_for("chain"), Some(overlay_window));

    // ...and the gate acts on that same number: the estimate is twice it, so
    // the head is skipped while the sibling remains.
    let chain = router
        .dispatch_chain("chain", None)
        .expect("chain resolves");
    let kept = router.filter_chain_by_window(chain, &req);
    let nicknames: Vec<&str> = kept
        .iter()
        .map(|t| t.nickname.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        nicknames,
        vec!["unconfirmed"],
        "the gate skipped exactly the target whose overlay-corrected window discovery reported",
    );
    assert_eq!(router.metrics.window_gate_skips_total(), 1);
}
