//! Coverage for the response-evidence observer (`observe_capabilities`): the
//! kill switch, `DetectorContext` derivation, stage-two admission with the
//! `Live` source, per-request dedupe, the acting-only ride-along on
//! `DispatchMeta`, the dedicated counters + WARNs, and the streaming-arm gap
//! (records nothing). The admission path is exercised through the real
//! registry so detector output, admission, and the read-model verdict all
//! participate.

use super::*;

use std::sync::Arc;
use std::time::Instant;

use futures::stream::{BoxStream, StreamExt};
use routectl_core::capability::{
    CACHE_HIT, PROMPT_CACHING, SCHEMA_MISMATCH, SCHEMA_PARSE, SEARCH_ABSENT_FORCED, SEARCH_BLOCKS,
    THINKING, THINKING_BLOCKS, Verdict,
};
use routectl_core::{
    ChatChunk, ChatResponse, Choice, CustomTool, Error, Message, MessageContent, Provider,
    ReasoningConfig, Result, Role, Usage,
};
use routectl_testkit::{CapturedEvent, capture_events};
use serde_json::json;

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;

// --- provider stubs ----------------------------------------------------

/// A provider used only to expand a dispatch target; the observer never
/// invokes it (it reads an already-assembled response).
struct NoopProvider {
    id: &'static str,
}

impl NoopProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self { id: "p1" })
    }
}

#[async_trait::async_trait]
impl Provider for NoopProvider {
    fn id(&self) -> &str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("p1", 500, "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p1", 500, "unused"))
    }
}

/// A provider whose stream yields one Ok chunk, so a real streaming dispatch
/// reaches the streaming success arm.
struct StreamOkProvider {
    id: &'static str,
}

impl StreamOkProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self { id: "p1" })
    }
}

#[async_trait::async_trait]
impl Provider for StreamOkProvider {
    fn id(&self) -> &str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("p1", 500, "unused"))
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let s = futures::stream::once(async move {
            Ok(ChatChunk {
                id: "c0".into(),
                model: req.model,
                choices: vec![],
                usage: None,
                opaque_events: Vec::new(),
                upstream_meta: None,
            })
        });
        Ok(s.boxed())
    }
}

// --- builders ----------------------------------------------------------

/// A minimal openai-compat provider config; the capability subsystem is left
/// at its default (enabled).
const OPENAI_P1: &str = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

/// The same config with the capability subsystem killed.
const OPENAI_P1_DISABLED: &str = r#"
[capability]
enabled = false

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

fn router_with(toml_text: &str, provider: Arc<dyn Provider>) -> Router {
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: std::collections::BTreeMap<String, Arc<ResolvedModel>> =
        std::collections::BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

fn openai_target(router: &Router) -> DispatchTarget {
    let p: Arc<dyn Provider> = NoopProvider::new();
    let model = ResolvedModel::new("m1", "p1", p, "wire-model");
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

fn assistant_text(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn clean_response(message: Message, usage: Option<Usage>) -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage,
        ..Default::default()
    }
}

/// A request declaring an Anthropic structured-output format whose schema
/// carries the given top-level required keys.
fn structured_output_request(required: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        provider_extras: Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": { "type": "object", "required": required }
                }
            }
        })),
        ..Default::default()
    }
}

fn observe_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message == "response-evidence capability observation acted")
        .collect()
}

// --- kill switch -------------------------------------------------------

#[test]
fn kill_switch_fully_disables_observation() {
    // Arrange: capability subsystem off, but the response carries clean
    // verified structured-output evidence that WOULD admit if enabled.
    let router = router_with(OPENAI_P1_DISABLED, NoopProvider::new());
    let target = openai_target(&router);
    let req = structured_output_request(&["name"]);
    let resp = clean_response(assistant_text(r#"{"name":"ok"}"#), None);
    let mut meta = DispatchMeta::for_alias("m1");

    // Act
    let events = capture_events(|| {
        router.observe_capabilities(&req, &resp, &target, &mut meta, Instant::now());
    });

    // Assert: zero writes, zero ride-alongs, zero counters, zero WARNs.
    assert!(meta.capability_observations.is_empty());
    assert!(router.learned_capabilities.snapshot().is_empty());
    assert_eq!(router.metrics.verified_working_total(), 0);
    assert!(observe_warns(&events).is_empty());
}

// --- verified admission + ride-along -----------------------------------

#[test]
fn verified_structured_output_admits_and_rides_along() {
    // Arrange: strict request + a JSON body carrying the required key.
    let router = router_with(OPENAI_P1, NoopProvider::new());
    let target = openai_target(&router);
    let req = structured_output_request(&["name"]);
    let resp = clean_response(assistant_text(r#"{"name":"ok"}"#), None);
    let mut meta = DispatchMeta::for_alias("m1");

    // Act
    let events = capture_events(|| {
        router.observe_capabilities(&req, &resp, &target, &mut meta, Instant::now());
    });

    // Assert: one acting positive rode out with the replay columns.
    assert_eq!(meta.capability_observations.len(), 1);
    let ev = &meta.capability_observations[0];
    assert_eq!(ev.state_key, "m1");
    assert_eq!(ev.capability_key, STRUCTURED_OUTPUT);
    assert_eq!(ev.provider_kind, "openai-compat");
    assert_eq!(ev.evidence_class, SCHEMA_PARSE);
    assert_eq!(ev.direction, ObservationDirection::Verified);
    assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(ev.source, EvidenceSource::Live);
    assert!(ev.request_features.contains(&STRUCTURED_OUTPUT.to_string()));

    // The registry now carries a VerifiedWorking entry, the counter bumped,
    // and the WARN carries only closed-set tokens + the state key.
    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].verdict, Verdict::VerifiedWorking);
    assert_eq!(router.metrics.verified_working_total(), 1);
    let warns = observe_warns(&events);
    assert_eq!(warns.len(), 1);
    assert_eq!(warns[0].field("capability_key"), Some(STRUCTURED_OUTPUT));
    assert_eq!(warns[0].field("evidence_class"), Some(SCHEMA_PARSE));
    assert_eq!(warns[0].field("direction"), Some("verified"));
    assert_eq!(warns[0].field("source"), Some("live"));
}

#[test]
fn verified_positive_no_ops_when_a_negative_resides() {
    // Arrange: a resident self-identifying negative owns the key.
    let router = router_with(OPENAI_P1, NoopProvider::new());
    let target = openai_target(&router);
    router.learned_capabilities.observe(
        "m1",
        STRUCTURED_OUTPUT,
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        Instant::now(),
    );
    let req = structured_output_request(&["name"]);
    let resp = clean_response(assistant_text(r#"{"name":"ok"}"#), None);
    let mut meta = DispatchMeta::for_alias("m1");

    // Act
    router.observe_capabilities(&req, &resp, &target, &mut meta, Instant::now());

    // Assert: the passive positive is suppressed -- no ride-along, no counter,
    // the negative stays resident.
    assert!(meta.capability_observations.is_empty());
    assert_eq!(router.metrics.verified_working_total(), 0);
    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].verdict, Verdict::LearnedBroken(FailurePhase::F1));
}

// --- F3 suspected absence ----------------------------------------------

#[test]
fn forced_search_absent_is_f3_and_needs_corroboration() {
    // Arrange: a forced web-search request whose clean response carries no
    // search evidence -> an inferred F3 suspected absence.
    let router = router_with(OPENAI_P1, NoopProvider::new());
    let target = openai_target(&router);
    let req = ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({"type": "web_search_20250305"}))]),
        tool_choice: Some(json!({"type": "any"})),
        ..Default::default()
    };
    let resp = clean_response(assistant_text("no search happened"), None);
    let now = Instant::now();

    // Act 1 -- a single inferred observation is pending, not acting.
    let mut meta1 = DispatchMeta::for_alias("m1");
    router.observe_capabilities(&req, &resp, &target, &mut meta1, now);
    assert!(
        meta1.capability_observations.is_empty(),
        "a first inferred observation must not act"
    );
    assert_eq!(router.metrics.f3_suspect_total(), 0);

    // Act 2 -- a second (corroborating) observation within the window acts.
    let mut meta2 = DispatchMeta::for_alias("m1");
    let events =
        capture_events(|| router.observe_capabilities(&req, &resp, &target, &mut meta2, now));

    // Assert: the corroborated F3 rode out, advisory-only in the registry.
    assert_eq!(meta2.capability_observations.len(), 1);
    let ev = &meta2.capability_observations[0];
    assert_eq!(ev.capability_key, WEB_SEARCH);
    assert_eq!(ev.evidence_class, SEARCH_ABSENT_FORCED);
    assert_eq!(ev.direction, ObservationDirection::SuspectAbsence);
    assert_eq!(ev.signal_tier, SignalTier::Inferred);
    assert_eq!(router.metrics.f3_suspect_total(), 1);
    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap[0].verdict, Verdict::LearnedBroken(FailurePhase::F3));
    assert_eq!(snap[0].source, EvidenceSource::Live);
    assert_eq!(observe_warns(&events).len(), 1);
}

// --- multiple capabilities, one event each -----------------------------

#[test]
fn distinct_capabilities_each_ride_exactly_once() {
    // Arrange: a response carrying verified web-search, prompt-cache, and
    // thinking evidence at once, on a reasoning request.
    let router = router_with(OPENAI_P1, NoopProvider::new());
    let target = openai_target(&router);
    let req = ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        reasoning: Some(ReasoningConfig {
            enabled: Some(true),
            effort: None,
            max_tokens: None,
            exclude: None,
        }),
        ..Default::default()
    };
    let usage = Usage {
        reasoning_tokens: Some(12),
        cache_read_input_tokens: Some(5),
        server_tool_use: Some(json!({"web_search_requests": 1})),
        ..Default::default()
    };
    let resp = clean_response(assistant_text("done"), Some(usage));
    let mut meta = DispatchMeta::for_alias("m1");

    // Act
    router.observe_capabilities(&req, &resp, &target, &mut meta, Instant::now());

    // Assert: exactly one event per distinct capability, no double-count.
    let mut keys: Vec<&str> = meta
        .capability_observations
        .iter()
        .map(|e| e.capability_key.as_str())
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec![PROMPT_CACHING, THINKING, WEB_SEARCH]);
    assert_eq!(router.metrics.verified_working_total(), 3);

    // A verified web-search evidence class is search_blocks / cache_hit /
    // thinking_blocks respectively.
    let by_key = |k: &str| {
        meta.capability_observations
            .iter()
            .find(|e| e.capability_key == k)
            .map(|e| e.evidence_class.as_str())
    };
    assert_eq!(by_key(WEB_SEARCH), Some(SEARCH_BLOCKS));
    assert_eq!(by_key(PROMPT_CACHING), Some(CACHE_HIT));
    assert_eq!(by_key(THINKING), Some(THINKING_BLOCKS));
}

#[test]
fn schema_mismatch_body_is_f3_suspect() {
    // Arrange: strict request, but a prose body that does not parse as JSON.
    let router = router_with(OPENAI_P1, NoopProvider::new());
    let target = openai_target(&router);
    let req = structured_output_request(&["name"]);
    let resp = clean_response(assistant_text("here is your answer, plainly"), None);
    let now = Instant::now();

    // Act -- two observations to corroborate the inferred suspect absence.
    let mut meta = DispatchMeta::for_alias("m1");
    router.observe_capabilities(&req, &resp, &target, &mut meta, now);
    router.observe_capabilities(&req, &resp, &target, &mut meta, now);

    // Assert: a single acting F3 suspect on the second observation.
    assert_eq!(meta.capability_observations.len(), 1);
    assert_eq!(
        meta.capability_observations[0].evidence_class,
        SCHEMA_MISMATCH
    );
    assert_eq!(
        meta.capability_observations[0].direction,
        ObservationDirection::SuspectAbsence
    );
}

// --- streaming records nothing -----------------------------------------

#[tokio::test]
async fn streaming_success_records_no_observation() {
    // Arrange: a real streaming dispatch that succeeds, on a request that
    // WOULD produce evidence on the non-streaming path.
    let router = router_with(OPENAI_P1, StreamOkProvider::new());
    let req = structured_output_request(&["name"]);

    // Act
    let dispatched = router
        .stream_with_options(req, RouterOptions::default())
        .await;

    // Assert: the stream succeeded, but the observer never ran on the stream
    // path -- the ride-along is empty and no positive counter bumped.
    assert!(dispatched.result.is_ok());
    assert!(dispatched.meta.capability_observations.is_empty());
    assert_eq!(router.metrics.verified_working_total(), 0);
    assert!(router.learned_capabilities.snapshot().is_empty());
}

// --- DetectorContext derivation ----------------------------------------

#[test]
fn detector_context_derives_strict_and_schema_keys_from_output_config() {
    let req = structured_output_request(&["name", "age"]);
    let features = crate::feature_keys::derive_feature_keys(&[], req.provider_extras.as_ref());
    let ctx = detector_context(&req, &features);
    assert!(ctx.strict_output_requested);
    assert_eq!(
        ctx.requested_schema_required_keys,
        vec!["name".to_string(), "age".to_string()]
    );
}

#[test]
fn detector_context_derives_schema_keys_from_strict_tool() {
    let req = ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Custom(CustomTool {
            name: "lookup".into(),
            description: None,
            input_schema: json!({"type": "object", "required": ["q"]}),
            cache_control: None,
            defer_loading: None,
            strict: Some(true),
            type_tag: None,
        })]),
        ..Default::default()
    };
    let features =
        crate::feature_keys::derive_feature_keys(req.tools.as_deref().unwrap_or(&[]), None);
    let ctx = detector_context(&req, &features);
    assert!(ctx.strict_output_requested);
    assert_eq!(ctx.requested_schema_required_keys, vec!["q".to_string()]);
}

#[test]
fn forces_web_search_covers_directive_shapes() {
    // General forces.
    assert!(forces_web_search(Some(&json!("required"))));
    assert!(forces_web_search(Some(&json!("any"))));
    assert!(forces_web_search(Some(&json!({"type": "any"}))));
    assert!(forces_web_search(Some(&json!({"type": "required"}))));
    // Specific-tool force naming web search (dated id tolerated).
    assert!(forces_web_search(Some(
        &json!({"type": "tool", "name": "web_search_20250305"})
    )));
    assert!(forces_web_search(Some(
        &json!({"type": "function", "function": {"name": "web_search"}})
    )));
    // A specific-tool force naming a DIFFERENT tool does not force search.
    assert!(!forces_web_search(Some(
        &json!({"type": "tool", "name": "calculator"})
    )));
    // Auto / none / absent never force.
    assert!(!forces_web_search(Some(&json!("auto"))));
    assert!(!forces_web_search(Some(&json!("none"))));
    assert!(!forces_web_search(None));
}

#[test]
fn reasoning_requested_reads_the_config() {
    let requested = |r: ReasoningConfig| reasoning_requested(Some(&r));
    let cfg = |enabled, effort: Option<&str>, max_tokens| ReasoningConfig {
        enabled,
        effort: effort.map(str::to_string),
        max_tokens,
        exclude: None,
    };
    assert!(requested(cfg(Some(true), None, None)));
    assert!(requested(cfg(None, Some("high"), None)));
    assert!(requested(cfg(None, None, Some(1024))));
    assert!(!requested(cfg(None, Some("none"), None)));
    // An explicit disable wins over any other field.
    assert!(!requested(cfg(Some(false), Some("high"), Some(1024))));
    assert!(!reasoning_requested(None));
}

#[test]
fn cache_requested_reads_top_level_and_tool_markers() {
    // Top-level auto-cache breakpoint.
    let top = ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        cache_control: Some(
            serde_json::from_value(json!({"type": "ephemeral"})).expect("valid cache_control"),
        ),
        ..Default::default()
    };
    assert!(cache_requested(&top));

    // Tool-definition breakpoint.
    let tooled = ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({
            "type": "web_search_20250305",
            "cache_control": {"type": "ephemeral"}
        }))]),
        ..Default::default()
    };
    assert!(cache_requested(&tooled));

    // Nothing set.
    let plain = ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        ..Default::default()
    };
    assert!(!cache_requested(&plain));
}
