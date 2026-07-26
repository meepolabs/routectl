//! End-to-end wiring of the strip interceptor at the three dispatch
//! paths (`complete`, `stream`, `count_tokens`). Each test drives a
//! real acting learned negative so the feature filter populates
//! `strip_capabilities`, then asserts the bytes that reach the
//! provider are the stripped bytes -- identically across all three
//! paths.
use super::*;
use crate::config::Config;
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use parking_lot::Mutex as ParkingMutex;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Message, Provider, TokenCount, ToolDef,
};
use routectl_testkit::with_capture;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Records every request that reaches the upstream at any of the three
/// dispatch entry points, and is `anthropic-api`-kind so it also serves
/// the `count_tokens` walk.
struct ProbeProvider {
    captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl Provider for ProbeProvider {
    fn id(&self) -> &'static str {
        "probe"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("probe", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let model = req.model.clone();
        self.captured.lock().push(req);
        Ok(ChatResponse {
            id: "ok".into(),
            model,
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
        self.captured.lock().push(req);
        let s = futures::stream::once(async move {
            Ok(ChatChunk {
                id: "c0".into(),
                model: "m".into(),
                choices: vec![],
                usage: None,
                opaque_events: Vec::new(),
                upstream_meta: None,
            })
        });
        Ok(s.boxed())
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.captured.lock().push(req);
        Ok(TokenCount {
            input_tokens: 7,
            extras: serde_json::Map::new(),
        })
    }
}

fn advisor_request() -> ChatRequest {
    ChatRequest {
        model: "haiku".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(
            json!({"type": "advisor", "name": "advisor"}),
        )]),
        ..Default::default()
    }
}

fn acting_advisor_negative(state_key: &str) -> crate::learned_capability::ExportedEntry {
    let base = Instant::now();
    crate::learned_capability::ExportedEntry {
        state_key: state_key.into(),
        feature_key: "advisor".into(),
        signal: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: base,
        last_seen: base,
        expires_at: base + Duration::from_hours(48),
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        in_flight: false,
        consecutive_failed_probes: 0,
    }
}

/// Router with a single `anthropic-api` provider `prov` and a model
/// `haiku` whose upstream is served by `provider`. When `learning` is
/// off the kill switch disables the learned pass entirely.
fn build_router(provider: Arc<dyn Provider>, learning: bool) -> Router {
    build_router_strict(provider, learning, false)
}

fn build_router_strict(provider: Arc<dyn Provider>, learning: bool, strict: bool) -> Router {
    let toml = format!(
        "version = 3\n[server]\nstrict_translation = {strict}\n\
             [capability]\nenabled = {learning}\n\
             [providers.prov]\nkind = \"anthropic-api\"\n"
    );
    let config: Config = toml::from_str(&toml).expect("config parses");
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new("haiku", "prov", provider, "claude-haiku");
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("haiku".into(), Arc::new(model));
    router.install_resolved_models(models);
    router
}

/// Advisor request whose `tool_choice` forces the advisor tool the
/// strip removes -- a strip-created hazard the post-strip check rolls
/// back, driving the route-away branch.
fn advisor_request_forcing_advisor() -> ChatRequest {
    ChatRequest {
        tool_choice: Some(json!({"type": "tool", "name": "advisor"})),
        ..advisor_request()
    }
}

/// Advisor request whose `tool_choice` mandates SOME tool (`{"type":
/// "any"}`) while the advisor is the only tool -- stripping it empties
/// the list, a strip-created hazard the post-strip check rolls back.
fn advisor_request_mandatory_choice() -> ChatRequest {
    ChatRequest {
        tool_choice: Some(json!({"type": "any"})),
        ..advisor_request()
    }
}

fn captured() -> Arc<ParkingMutex<Vec<ChatRequest>>> {
    Arc::new(ParkingMutex::new(Vec::new()))
}

fn dispatched_tool_types(req: &ChatRequest) -> Vec<String> {
    req.tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| match t {
                    ToolDef::Other(v) => v.get("type").and_then(|x| x.as_str()).map(str::to_string),
                    ToolDef::Custom(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn complete_strips_advisor_before_dispatch() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    router.complete(advisor_request()).await.expect("ok");

    let cap = cap.lock();
    let upstream = cap.first().expect("one upstream call");
    assert!(
        dispatched_tool_types(upstream).is_empty(),
        "advisor tool is stripped before dispatch",
    );
    assert_eq!(router.metrics.strip_total(), 1);
}

#[tokio::test]
async fn stream_strips_advisor_identically() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let _ = router
        .stream(advisor_request())
        .await
        .expect("ok")
        .collect::<Vec<_>>()
        .await;

    let cap = cap.lock();
    let upstream = cap.first().expect("one upstream call");
    assert!(
        dispatched_tool_types(upstream).is_empty(),
        "the streaming path strips identically to the completion path",
    );
    assert_eq!(router.metrics.strip_total(), 1);
}

#[tokio::test]
async fn count_tokens_strips_so_estimated_prefix_matches_shipped() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let count = router.count_tokens(advisor_request()).await.expect("ok");
    assert_eq!(count.input_tokens, 7);

    let cap = cap.lock();
    let upstream = cap.first().expect("one count_tokens call");
    assert!(
        dispatched_tool_types(upstream).is_empty(),
        "count_tokens counts the stripped prefix, matching the shipped prefix",
    );
    assert_eq!(router.metrics.strip_total(), 1);
}

#[tokio::test]
async fn kill_switch_off_leaves_advisor_intact() {
    // With `[capability] enabled = false` the learned pass never runs,
    // so `strip_capabilities` stays empty and the advisor tool reaches
    // the upstream unstripped -- the helper is inert by construction.
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, false);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    router.complete(advisor_request()).await.expect("ok");

    let cap = cap.lock();
    let upstream = cap.first().expect("one upstream call");
    assert_eq!(
        dispatched_tool_types(upstream),
        vec!["advisor".to_string()],
        "a disabled kill switch leaves the request untouched",
    );
    assert_eq!(router.metrics.strip_total(), 0);
}

#[tokio::test]
async fn complete_strict_rejects_without_dispatching() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router_strict(provider, true, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let err = router
        .complete(advisor_request())
        .await
        .expect_err("strict translation rejects the strip");

    assert!(matches!(err, Error::Validation(_)), "{err:?}");
    assert!(
        cap.lock().is_empty(),
        "a strict-rejected attempt never reaches the upstream",
    );
    assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
    assert_eq!(router.metrics.strip_total(), 0);
}

#[tokio::test]
async fn complete_rollback_routes_away_without_dispatching_mutated_request() {
    // The only chain entry rolls back (dangling forced tool_choice), so
    // the mutated request is never dispatched and the single-entry chain
    // exhausts to an error -- with zero upstream calls.
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let result = router.complete(advisor_request_forcing_advisor()).await;

    assert!(result.is_err(), "the rolled-back attempt does not dispatch");
    assert!(
        cap.lock().is_empty(),
        "a rolled-back attempt never dispatches the mutated request",
    );
    assert_eq!(router.metrics.strip_rollback_total(), 1);
    assert_eq!(router.metrics.strip_total(), 0);
}

#[tokio::test]
async fn stream_strict_rejects_without_dispatching() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router_strict(provider, true, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let err = router
        .stream(advisor_request())
        .await
        .err()
        .expect("strict translation rejects the strip before the stream opens");

    assert!(matches!(err, Error::Validation(_)), "{err:?}");
    assert!(cap.lock().is_empty(), "no upstream call on strict reject");
    assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
}

#[tokio::test]
async fn count_tokens_strict_rejects_without_dispatching() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router_strict(provider, true, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let err = router
        .count_tokens(advisor_request())
        .await
        .expect_err("strict translation rejects the strip");

    assert!(matches!(err, Error::Validation(_)), "{err:?}");
    assert!(
        cap.lock().is_empty(),
        "count_tokens never reaches the upstream on strict reject",
    );
    assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
}

#[tokio::test]
async fn count_tokens_rollback_advances_seat_without_dispatching() {
    // The only capable seat rolls back, so count_tokens advances past it
    // and the walk exhausts -- with no provider count_tokens call.
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let result = router.count_tokens(advisor_request_forcing_advisor()).await;

    assert!(result.is_err(), "the only seat rolled back and was skipped");
    assert!(
        cap.lock().is_empty(),
        "a rolled-back seat never calls the upstream count_tokens",
    );
    assert_eq!(router.metrics.strip_rollback_total(), 1);
}

#[tokio::test]
async fn complete_rollback_on_mandatory_choice_emptied_tools() {
    // The sole tool is the advisor; stripping it empties the list while
    // tool_choice mandates a tool. The post-strip check rolls back, so
    // the single-entry chain exhausts with no upstream call.
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let result = router.complete(advisor_request_mandatory_choice()).await;

    assert!(
        result.is_err(),
        "the emptied-tools attempt does not dispatch"
    );
    assert!(
        cap.lock().is_empty(),
        "a rolled-back attempt never dispatches the mutated request",
    );
    assert_eq!(router.metrics.strip_rollback_total(), 1);
    assert_eq!(router.metrics.strip_total(), 0);
}

#[tokio::test]
async fn stream_rollback_on_mandatory_choice_emptied_tools() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let result = router.stream(advisor_request_mandatory_choice()).await;

    assert!(result.is_err(), "the streaming path rolls back identically");
    assert!(
        cap.lock().is_empty(),
        "a rolled-back stream never dispatches the mutated request",
    );
    assert_eq!(router.metrics.strip_rollback_total(), 1);
    assert_eq!(router.metrics.strip_total(), 0);
}

#[tokio::test]
async fn count_tokens_rollback_on_mandatory_choice_emptied_tools() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("haiku")]);

    let result = router
        .count_tokens(advisor_request_mandatory_choice())
        .await;

    assert!(result.is_err(), "the only seat rolled back and was skipped");
    assert!(
        cap.lock().is_empty(),
        "a rolled-back seat never calls the upstream count_tokens",
    );
    assert_eq!(router.metrics.strip_rollback_total(), 1);
}

/// An `anthropic-api`-kind provider whose `count_tokens` always fails
/// with a fixed upstream health status, so the seat reaches the
/// class/remap/debit settle point rather than returning a count.
struct HealthErrorProvider {
    status: u16,
}

#[async_trait]
impl Provider for HealthErrorProvider {
    fn id(&self) -> &'static str {
        "health"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("health", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("health", self.status, "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("health", self.status, "unused"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        Err(Error::upstream(
            "health",
            self.status,
            "upstream health error",
        ))
    }
}

fn plain_count_request() -> ChatRequest {
    ChatRequest {
        model: "haiku".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn count_tokens_class_path_emits_one_observability_event() {
    // A 500 is a health class the seat debits: the terminal walk exits
    // here (count_tokens never falls back on health), so the settle point
    // fires exactly one INFO `count_tokens` event carrying the class
    // decision -- no longer silent.
    let provider: Arc<dyn Provider> = Arc::new(HealthErrorProvider { status: 500 });
    let router = build_router(provider, false);

    let (result, events) = with_capture(router.count_tokens(plain_count_request())).await;

    assert!(result.is_err(), "the 500 health error surfaces terminal");
    let emitted: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("count_tokens"))
        .collect();
    assert_eq!(
        emitted.len(),
        1,
        "the class/remap/debit path emits exactly one count_tokens event",
    );
    let ev = emitted[0];
    assert_eq!(ev.level, tracing::Level::INFO);
    assert_eq!(ev.field("state_key"), Some("haiku"));
    assert_eq!(ev.field("status"), Some("500"));
    assert_eq!(ev.field("effective_class"), Some("server_error"));
    assert_eq!(ev.field("debit"), Some("true"));
    assert_eq!(ev.field("remapped"), Some("false"));
    assert!(
        !ev.fields.iter().any(|(k, _)| k == "body" || k == "prompt"),
        "the event carries no body or prompt",
    );
}

#[tokio::test]
async fn count_tokens_clean_passthrough_emits_no_class_event() {
    // A successful count never reaches the class/remap/debit settle point,
    // so no count_tokens observability event fires on the happy path.
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, false);

    let (result, events) = with_capture(router.count_tokens(plain_count_request())).await;

    assert!(result.is_ok(), "a clean count_tokens passthrough succeeds");
    assert!(
        !events
            .iter()
            .any(|e| e.field("event") == Some("count_tokens")),
        "a clean passthrough must not emit the class-decision event",
    );
}
