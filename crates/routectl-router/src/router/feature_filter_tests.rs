//! Tests for the v0.6.0 per-provider `unsupported_features`
//! pre-filter. Confirms that providers listing a request feature
//! get skipped BEFORE dispatch (no upstream call, no breaker
//! account) and that a chain reduced to empty surfaces as
//! `Error::NotImplemented` rather than walking and 400ing.
use super::*;
#[cfg(feature = "bedrock")]
use crate::capability_matcher::resolve_requested_capability;
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use crate::router::LearnedProbeGuard;
use crate::router::chain::into_one_dispatch_target;
use async_trait::async_trait;
use futures::stream::BoxStream;
use parking_lot::Mutex as ParkingMutex;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, CustomTool, Error, Message, Provider, ToolDef,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Provider stub that records every `complete()` call. The test
/// asserts on `captured.len()` to prove a provider was (or was not)
/// dispatched to.
struct CapturingProvider {
    id: String,
    captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl Provider for CapturingProvider {
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
        let model = req.model.clone();
        let id = self.id.clone();
        self.captured.lock().push(req);
        Ok(ChatResponse {
            id: format!("ok-{id}"),
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
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
}

fn web_search_tool() -> ToolDef {
    ToolDef::Other(json!({
        "type": "web_search_20250305",
        "name": "search"
    }))
}

fn web_search_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![].into(),
        tools: Some(vec![web_search_tool()]),
        ..Default::default()
    }
}

/// Request carrying an Anthropic structured-output `output_config.
/// format` on `provider_extras` -- the non-tool-derived source of
/// the `structured_output` feature key.
fn structured_output_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![].into(),
        tools: None,
        provider_extras: Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object"}
                }
            }
        })),
        ..Default::default()
    }
}

/// Per-provider captured-request log for test introspection.
type CapturedRequests = Arc<ParkingMutex<Vec<ChatRequest>>>;

/// Request carrying a canonical top-level `response_format` json_schema
/// directive -- the OpenAI-shape structured-output source forwarded by
/// the router call sites, distinct from the Anthropic
/// `output_config.format` slot on `provider_extras`.
fn response_format_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![].into(),
        tools: None,
        response_format: Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "r",
                "schema": {"type": "object"}
            }
        })),
        ..Default::default()
    }
}

/// Build a router with a 2-entry alias chain `["bedrock-opus" ->
/// "anthropic-opus"]`. Each provider entry carries the
/// `unsupported_features` list passed by the caller.
fn build_router_with_chain(
    unsupported_first: Vec<String>,
    unsupported_second: Vec<String>,
) -> (Router, CapturedRequests, CapturedRequests) {
    let mut config = Config::default();
    config.providers.insert(
        "bedrock-prov".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                unsupported_features: unsupported_first,
                ..Default::default()
            },
        },
    );
    config.providers.insert(
        "anthropic-prov".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                unsupported_features: unsupported_second,
                ..Default::default()
            },
        },
    );
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(vec!["bedrock-opus".into(), "anthropic-opus".into()]),
    );

    let mut router = Router::new(Arc::new(config));
    let captured_first: Arc<ParkingMutex<Vec<ChatRequest>>> =
        Arc::new(ParkingMutex::new(Vec::new()));
    let captured_second: Arc<ParkingMutex<Vec<ChatRequest>>> =
        Arc::new(ParkingMutex::new(Vec::new()));
    let p_first: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "bedrock-prov".into(),
        captured: captured_first.clone(),
    });
    let p_second: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "anthropic-prov".into(),
        captured: captured_second.clone(),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "bedrock-opus".into(),
        Arc::new(ResolvedModel::new(
            "bedrock-opus",
            "bedrock-prov",
            p_first,
            "opus-via-bedrock",
        )),
    );
    models.insert(
        "anthropic-opus".into(),
        Arc::new(ResolvedModel::new(
            "anthropic-opus",
            "anthropic-prov",
            p_second,
            "opus-via-anthropic",
        )),
    );
    router.install_resolved_models(models);
    (router, captured_first, captured_second)
}

#[tokio::test]
async fn web_search_skips_first_provider_when_listed_unsupported() {
    // Chain [bedrock, anthropic]. Bedrock declares web_search
    // unsupported. Request carries web_search_20250305. Dispatch
    // must go DIRECTLY to anthropic (no bedrock attempt, no
    // breaker accounting on bedrock).
    let (router, captured_bedrock, captured_anthropic) =
        build_router_with_chain(vec!["web_search".into()], vec![]);
    let req = web_search_request("alias");
    let resp = router.complete(req).await.expect("dispatch must succeed");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic-prov"));
    assert_eq!(
        captured_bedrock.lock().len(),
        0,
        "bedrock must be skipped, not tried-and-fallback",
    );
    assert_eq!(captured_anthropic.lock().len(), 1);
}

#[tokio::test]
async fn empty_chain_after_filter_returns_not_implemented() {
    // Both chain entries declare the feature unsupported. The
    // filter eliminates everyone, so the router synthesizes a
    // 501 NotImplemented naming the feature key. No upstream
    // attempt happens.
    let (router, captured_bedrock, captured_anthropic) =
        build_router_with_chain(vec!["web_search".into()], vec!["web_search".into()]);
    let req = web_search_request("alias");
    let err = router.complete(req).await.unwrap_err();
    match err {
        Error::NotImplemented(alias, msg) => {
            assert_eq!(alias, "alias");
            assert!(
                msg.contains("web_search"),
                "error message must name the feature; got: {msg}",
            );
        }
        other => panic!("expected Error::NotImplemented; got {other:?}"),
    }
    assert_eq!(captured_bedrock.lock().len(), 0);
    assert_eq!(captured_anthropic.lock().len(), 0);
}

#[tokio::test]
async fn no_features_in_request_is_no_op_filter() {
    // Even when bedrock declares web_search unsupported, a
    // request without tools (no feature keys derived) dispatches
    // to bedrock first per the chain order.
    let (router, captured_bedrock, _captured_anthropic) =
        build_router_with_chain(vec!["web_search".into()], vec![]);
    let req = ChatRequest {
        model: "alias".into(),
        messages: vec![].into(),
        tools: None,
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("bedrock-prov"));
    assert_eq!(
        captured_bedrock.lock().len(),
        1,
        "no features -> filter is a no-op, bedrock takes the request",
    );
}

#[tokio::test]
async fn dated_suffix_versions_normalize_to_same_key() {
    // `web_search_20250305` and a hypothetical
    // `web_search_20251102` both reduce to the same key
    // `web_search`. Bedrock declares `web_search` unsupported, so
    // both versions get filtered identically.
    let (router, captured_bedrock, captured_anthropic) =
        build_router_with_chain(vec!["web_search".into()], vec![]);
    let req = ChatRequest {
        model: "alias".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({
            "type": "web_search_20251102",
            "name": "search"
        }))]),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic-prov"));
    assert_eq!(captured_bedrock.lock().len(), 0);
    assert_eq!(captured_anthropic.lock().len(), 1);
}

#[tokio::test]
async fn custom_tools_dont_contribute_feature_keys() {
    // A user-defined `ToolDef::Custom` tool has no version-stamped
    // `type` and therefore contributes NO feature key. The filter
    // is a no-op even when bedrock has unsupported_features set.
    let (router, captured_bedrock, _captured_anthropic) =
        build_router_with_chain(vec!["web_search".into()], vec![]);
    let req = ChatRequest {
        model: "alias".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Custom(CustomTool {
            name: "calculator".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]),
        ..Default::default()
    };
    let resp = router.complete(req).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("bedrock-prov"));
    assert_eq!(
        captured_bedrock.lock().len(),
        1,
        "Custom tools must not be treated as feature keys",
    );
}

#[tokio::test]
async fn structured_output_skips_first_provider_when_listed_unsupported() {
    // Chain [bedrock, anthropic]. Bedrock declares structured_output
    // unsupported. Request carries output_config.format. Dispatch
    // must go DIRECTLY to anthropic (Bedrock Invoke can't enforce
    // constrained decoding -> malformed tool_use the client can't
    // parse).
    let (router, captured_bedrock, captured_anthropic) =
        build_router_with_chain(vec!["structured_output".into()], vec![]);
    let req = structured_output_request("alias");
    let resp = router.complete(req).await.expect("dispatch must succeed");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic-prov"));
    assert_eq!(
        captured_bedrock.lock().len(),
        0,
        "bedrock must be skipped, not tried-and-fallback",
    );
    assert_eq!(captured_anthropic.lock().len(), 1);
}

#[tokio::test]
async fn response_format_json_schema_skips_first_provider_when_unsupported() {
    // Proactive route-away for the canonical top-level `response_format`
    // source (OpenAI-shape json_schema), forwarded by the chain call site.
    // Chain [bedrock, anthropic]; bedrock declares structured_output
    // unsupported. A request carrying response_format={json_schema} must
    // skip bedrock and land on anthropic -- the proactive leg the reactive
    // matcher previously had to cover alone.
    let (router, captured_bedrock, captured_anthropic) =
        build_router_with_chain(vec!["structured_output".into()], vec![]);
    let req = response_format_request("alias");
    let resp = router.complete(req).await.expect("dispatch must succeed");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic-prov"));
    assert_eq!(
        captured_bedrock.lock().len(),
        0,
        "bedrock must be skipped proactively on the response_format source",
    );
    assert_eq!(captured_anthropic.lock().len(), 1);
}

#[tokio::test]
async fn structured_output_empty_chain_returns_not_implemented() {
    // Both chain entries declare structured_output unsupported. The
    // filter eliminates everyone -> 501 NotImplemented naming the
    // feature key, no upstream attempt.
    let (router, captured_bedrock, captured_anthropic) = build_router_with_chain(
        vec!["structured_output".into()],
        vec!["structured_output".into()],
    );
    let req = structured_output_request("alias");
    let err = router.complete(req).await.unwrap_err();
    match err {
        Error::NotImplemented(alias, msg) => {
            assert_eq!(alias, "alias");
            assert!(
                msg.contains("structured_output"),
                "error message must name the feature; got: {msg}",
            );
        }
        other => panic!("expected Error::NotImplemented; got {other:?}"),
    }
    assert_eq!(captured_bedrock.lock().len(), 0);
    assert_eq!(captured_anthropic.lock().len(), 0);
}

// --- per-MODEL unsupported_features (unioned with the
// per-provider list, keyed on nickname so two models on one provider
// filter independently) ---

/// Build a router whose alias chain is two MODELS on the SAME single
/// provider: `["mA" -> "mB"]`. The provider itself declares NO
/// unsupported features; each model carries its own per-model list.
/// Proves nickname-keying: two nicknames on one provider filter
/// independently. Returns per-model captured-request logs.
fn build_router_two_models_one_provider(
    unsupported_model_a: Vec<String>,
    unsupported_model_b: Vec<String>,
) -> (Router, CapturedRequests, CapturedRequests) {
    let mut config = Config::default();
    config.providers.insert(
        "shared-prov".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                unsupported_features: vec![],
                ..Default::default()
            },
        },
    );
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(vec!["mA".into(), "mB".into()]),
    );
    // Model-static lists live in config.models: the override registry
    // is built from config, mirroring the factory's
    // build_resolved_models.
    config.models.insert(
        "mA".into(),
        crate::config::ModelEntry::new("shared-prov", "upstream-a")
            .with_unsupported_features(unsupported_model_a),
    );
    config.models.insert(
        "mB".into(),
        crate::config::ModelEntry::new("shared-prov", "upstream-b")
            .with_unsupported_features(unsupported_model_b),
    );

    let mut router = Router::new(Arc::new(config));
    let captured_a: CapturedRequests = Arc::new(ParkingMutex::new(Vec::new()));
    let captured_b: CapturedRequests = Arc::new(ParkingMutex::new(Vec::new()));
    let p_a: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "shared-prov".into(),
        captured: captured_a.clone(),
    });
    let p_b: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "shared-prov".into(),
        captured: captured_b.clone(),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "mA".into(),
        Arc::new(ResolvedModel::new("mA", "shared-prov", p_a, "upstream-a")),
    );
    models.insert(
        "mB".into(),
        Arc::new(ResolvedModel::new("mB", "shared-prov", p_b, "upstream-b")),
    );
    router.install_resolved_models(models);
    (router, captured_a, captured_b)
}

#[tokio::test]
async fn model_unsupported_drops_only_that_nickname_not_sibling() {
    // (a) Two models on ONE provider. mA declares structured_output
    // unsupported; mB does NOT. An SO request must skip mA and land
    // on mB -- proving the model list is keyed on NICKNAME, not on
    // the (shared) provider name.
    let (router, captured_a, captured_b) =
        build_router_two_models_one_provider(vec!["structured_output".into()], vec![]);
    let req = structured_output_request("alias");
    let resp = router.complete(req).await.expect("dispatch must succeed");
    assert_eq!(resp.routectl_provider.as_deref(), Some("shared-prov"));
    assert_eq!(
        captured_a.lock().len(),
        0,
        "mA must be skipped on its per-model unsupported list",
    );
    assert_eq!(
        captured_b.lock().len(),
        1,
        "sibling mB on the same provider must still be tried",
    );
}

#[tokio::test]
async fn empty_model_lists_leave_routing_unchanged() {
    // (c) Neither model declares anything unsupported. An SO request
    // dispatches to the first chain entry exactly as before.
    let (router, captured_a, captured_b) = build_router_two_models_one_provider(vec![], vec![]);
    let req = structured_output_request("alias");
    let resp = router.complete(req).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("shared-prov"));
    assert_eq!(
        captured_a.lock().len(),
        1,
        "empty per-model lists -> filter is a no-op, first entry takes it",
    );
    assert_eq!(captured_b.lock().len(), 0);
}

#[tokio::test]
async fn both_models_unsupported_returns_not_implemented_naming_feature() {
    // (d) Both models declare the feature unsupported via the static
    // union. The chain filters to empty -> 501 NotImplemented naming
    // the feature, no upstream attempt.
    let (router, captured_a, captured_b) = build_router_two_models_one_provider(
        vec!["structured_output".into()],
        vec!["structured_output".into()],
    );
    let req = structured_output_request("alias");
    let err = router.complete(req).await.unwrap_err();
    match err {
        Error::NotImplemented(alias, msg) => {
            assert_eq!(alias, "alias");
            assert!(
                msg.contains("structured_output"),
                "error message must name the feature; got: {msg}",
            );
        }
        other => panic!("expected Error::NotImplemented; got {other:?}"),
    }
    assert_eq!(captured_a.lock().len(), 0);
    assert_eq!(captured_b.lock().len(), 0);
}

#[tokio::test]
async fn route_not_strip_leaves_output_config_intact() {
    // (f) ROUTE-not-STRIP: mA is incapable, mB is capable. The
    // filter only DROPS the incapable target -- it must never mutate
    // the request body. The dispatched request on mB must still
    // carry the original output_config.format untouched.
    let (router, _captured_a, captured_b) =
        build_router_two_models_one_provider(vec!["structured_output".into()], vec![]);
    let req = structured_output_request("alias");
    let resp = router.complete(req).await.expect("dispatch must succeed");
    assert_eq!(resp.routectl_provider.as_deref(), Some("shared-prov"));
    let dispatched = captured_b.lock();
    assert_eq!(dispatched.len(), 1, "capable mB must receive the request");
    let extras = dispatched[0]
        .provider_extras
        .as_ref()
        .expect("filter must not strip provider_extras");
    assert_eq!(
        extras
            .get("output_config")
            .and_then(|c| c.get("format"))
            .and_then(|f| f.get("type"))
            .and_then(|t| t.as_str()),
        Some("json_schema"),
        "output_config.format must reach the capable target unmodified",
    );
}

#[test]
fn helper_distinguishes_provider_and_model_source() {
    // (e) Unit-test the decision seam directly: provider-scoped vs
    // model-scoped matches return distinct FilterSource variants;
    // a supported feature returns None. Also pins the precedence:
    // with features listed at different scopes, the FIRST requested
    // feature that matches wins (iteration order).
    let mut config = Config::default();
    config.providers.insert(
        "prov-blocks-ws".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                unsupported_features: vec!["web_search".into()],
                ..Default::default()
            },
        },
    );
    // The per-model list lives in config.models: the override registry
    // is built from config (mirroring build_resolved_models).
    config.models.insert(
        "m".into(),
        crate::config::ModelEntry::new("prov-blocks-ws", "u")
            .with_unsupported_features(vec!["structured_output".into()]),
    );
    let router = Router::new(Arc::new(config));
    let stub: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "prov-blocks-ws".into(),
        captured: Arc::new(ParkingMutex::new(Vec::new())),
    });
    let model = Arc::new(ResolvedModel::new("m", "prov-blocks-ws", stub, "u"));
    let target = into_one_dispatch_target(model);

    // Provider-scoped match.
    assert_eq!(
        router.unsupported_feature_for_target(
            &target,
            &["web_search".to_string()],
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        Some(("web_search".to_string(), FilterSource::ProviderStatic)),
    );
    // Model-scoped match.
    assert_eq!(
        router.unsupported_feature_for_target(
            &target,
            &["structured_output".to_string()],
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        Some(("structured_output".to_string(), FilterSource::ModelStatic)),
    );
    // Supported feature -> None.
    assert_eq!(
        router.unsupported_feature_for_target(
            &target,
            &["computer_use".to_string()],
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        None,
    );
    // Both scopes list a (different) feature: the FIRST requested
    // feature that matches wins, and web_search resolves to the
    // provider scope.
    assert_eq!(
        router.unsupported_feature_for_target(
            &target,
            &["web_search".to_string(), "structured_output".to_string()],
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        Some(("web_search".to_string(), FilterSource::ProviderStatic)),
    );
}

#[test]
fn multi_feature_scan_routes_away_and_captures_earlier_probe_admission() {
    // Regression: a target carrying an EXPIRED (probe-due) learned
    // negative on one feature AND an acting negative on another, hit by a
    // request that names both. The scan must not stop at the first
    // feature's probe admission -- it has to reach the second feature's
    // RouteAway (tail-drop the target) AND still capture the earlier probe
    // admission. Dropping that admission would latch the `in_flight` slot
    // the probe claimed, so the feature could never re-probe.
    use crate::learned_capability::{ExportedEntry, RoutingDecision};

    let router = Router::new(Arc::new(Config::default()));
    let stub: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "prov".into(),
        captured: Arc::new(ParkingMutex::new(Vec::new())),
    });
    let model = Arc::new(ResolvedModel::new("nick", "prov", stub, "upstream"));
    let mut target = into_one_dispatch_target(model);
    // The learned pass runs only for a target that carries a provider kind.
    target.provider_kind = Some("openai-compat");

    // Seed the registry directly so each `expires_at` is fixed relative to
    // a captured base -- the filter's own `Instant::now()` fires strictly
    // later, so `structured_output` reads expired (probe-due) and
    // `web_search` still acts, with no fragile clock subtraction.
    let base = Instant::now();
    let probe_due_key = normalize_capability_key("structured_output", "openai-compat");
    let acting_key = normalize_capability_key("web_search", "openai-compat");
    router.learned_capabilities.import_entries(vec![
        ExportedEntry {
            state_key: "nick".into(),
            feature_key: probe_due_key.clone(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
        },
        ExportedEntry {
            state_key: "nick".into(),
            feature_key: acting_key,
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base + Duration::from_hours(48),
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
        },
    ]);

    // Features in [probe-due, acting] order: the pre-fix code
    // short-circuited on the probe admission and returned `None` (target
    // wrongly kept as supported); the fix scans on to the acting negative.
    let mut admissions = Vec::new();
    let decision = router.unsupported_feature_for_target(
        &target,
        &["structured_output".to_string(), "web_search".to_string()],
        &mut admissions,
        &mut Vec::new(),
    );
    assert_eq!(
        decision,
        Some(("web_search".to_string(), FilterSource::Learned)),
        "RouteAway on the acting feature must decide, not the earlier probe admission",
    );

    // The earlier probe admission survived the scan -- not swallowed by a
    // short-circuit -- so the dispatch path can settle its slot.
    assert_eq!(
        admissions.len(),
        1,
        "the probe-due feature's admission must be captured",
    );
    assert_eq!(admissions[0].state_key, "nick");
    assert_eq!(admissions[0].feature, probe_due_key);

    // The probe claimed the single in_flight slot: while it is held a
    // repeat query routes away, proving the slot is genuinely occupied.
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "nick",
            "structured_output",
            "openai-compat",
            Instant::now(),
        ),
        RoutingDecision::RouteAway {
            signal: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
        },
        "the in_flight slot is held until the admission settles",
    );

    // Settle exactly as dispatch does: arm the guard from the captured
    // admissions and drop it (the fallback / other-error settle path).
    {
        let _guard =
            LearnedProbeGuard::armed(router.learned_capabilities.clone(), admissions, "complete");
    }
    // The released slot makes the feature re-probable; had the admission
    // been dropped the slot would have latched forever.
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "nick",
            "structured_output",
            "openai-compat",
            Instant::now(),
        ),
        RoutingDecision::ProbeAdmitted,
        "settling the captured admission releases in_flight; the feature re-probes",
    );
}

#[test]
fn filter_source_as_str_tokens() {
    assert_eq!(FilterSource::ProviderStatic.as_str(), "provider");
    assert_eq!(FilterSource::ModelStatic.as_str(), "model");
}

// --- strip-vs-route verdict (capability-strip wiring) ---

/// An acting (non-expired) learned negative for `(state_key, feature)`,
/// normalized under the `openai-compat` kind these strip tests use
/// (identity normalization for a clean key).
fn acting_negative(
    state_key: &str,
    feature: &str,
    base: Instant,
) -> crate::learned_capability::ExportedEntry {
    crate::learned_capability::ExportedEntry {
        state_key: state_key.into(),
        feature_key: normalize_capability_key(feature, "openai-compat"),
        verdict: crate::learned_capability::EntryVerdict::Negative,
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

/// A probe-due (expired) learned negative: `expires_at == base`, which
/// the filter's strictly-later `Instant::now()` reads as expired, so
/// the single re-probe slot is admitted.
fn probe_due_negative(
    state_key: &str,
    feature: &str,
    base: Instant,
) -> crate::learned_capability::ExportedEntry {
    crate::learned_capability::ExportedEntry {
        expires_at: base,
        ..acting_negative(state_key, feature, base)
    }
}

fn strip_target(nickname: &str) -> DispatchTarget {
    let stub: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: nickname.into(),
        captured: Arc::new(ParkingMutex::new(Vec::new())),
    });
    let model = Arc::new(ResolvedModel::new(nickname, "prov", stub, "upstream"));
    let mut target = into_one_dispatch_target(model);
    // The learned pass runs only for a target carrying a provider kind.
    target.provider_kind = Some("openai-compat");
    target
}

#[test]
fn all_strip_negatives_keep_target_supported_with_sorted_keys() {
    // Two acting negatives, both droppable: advisor (tool-shape strip)
    // and context_management (beta strip). No route-away, no pin -> the
    // target stays supported carrying both keys in sorted normalized
    // order so a per-session cache prefix stays stable.
    let router = Router::new(Arc::new(Config::default()));
    let target = strip_target("nick");
    let base = Instant::now();
    router.learned_capabilities.import_entries(vec![
        acting_negative("nick", "context_management", base),
        acting_negative("nick", "advisor", base),
    ]);

    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let decision = router.unsupported_feature_for_target(
        &target,
        &["context_management".to_string(), "advisor".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    assert_eq!(
        decision, None,
        "all-strip negatives keep the target supported"
    );
    assert_eq!(
        strip_keys,
        vec!["advisor".to_string(), "context_management".to_string()],
        "strip keys are sorted normalized",
    );
    assert!(admissions.is_empty());
}

#[test]
fn any_route_away_negative_demotes_target_and_leaves_strip_empty() {
    // A droppable negative (context_management) coexists with an
    // essential route-away one (web_search). ANY route-away demotes the
    // whole target to the tail; the strip set is abandoned so the target
    // is never half-stripped.
    let router = Router::new(Arc::new(Config::default()));
    let target = strip_target("nick");
    let base = Instant::now();
    router.learned_capabilities.import_entries(vec![
        acting_negative("nick", "context_management", base),
        acting_negative("nick", "web_search", base),
    ]);

    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let decision = router.unsupported_feature_for_target(
        &target,
        &["context_management".to_string(), "web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    assert_eq!(
        decision,
        Some(("web_search".to_string(), FilterSource::Learned))
    );
    assert!(
        strip_keys.is_empty(),
        "a route-away target never carries strip keys"
    );
}

#[test]
fn admitted_probe_feature_excluded_from_strip_but_admission_recorded() {
    // context_management is probe-due (a would-be strip) and advisor is
    // an acting strip. The admitted re-probe tests the REAL capability on
    // the full request, so context_management is excluded from the strip
    // set -- yet its admission still reaches `admissions` to settle the
    // in_flight slot. advisor still strips.
    let router = Router::new(Arc::new(Config::default()));
    let target = strip_target("nick");
    let base = Instant::now();
    router.learned_capabilities.import_entries(vec![
        probe_due_negative("nick", "context_management", base),
        acting_negative("nick", "advisor", base),
    ]);

    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let decision = router.unsupported_feature_for_target(
        &target,
        &["context_management".to_string(), "advisor".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    assert_eq!(decision, None);
    assert_eq!(
        strip_keys,
        vec!["advisor".to_string()],
        "the admitted-probe feature is never stripped",
    );
    assert_eq!(
        admissions.len(),
        1,
        "the probe admission still settles its slot"
    );
    assert_eq!(
        admissions[0].feature,
        normalize_capability_key("context_management", "openai-compat"),
    );
}

#[test]
fn stripped_success_leaves_negative_while_admitted_probe_success_clears() {
    // The two-sided invariant: an admitted probe's full-request 2xx clears
    // its negative, but a stripped success clears nothing. advisor is an
    // ACTING strip (stripped in place -> NO admission); context_management
    // is PROBE-DUE (admitted, bypassed from the strip set). The filter
    // records only the probe admission, so settling a 2xx over the recorded
    // admissions clears context_management yet leaves the stripped advisor
    // negative acting.
    let router = Router::new(Arc::new(Config::default()));
    let target = strip_target("nick");
    let base = Instant::now();
    router.learned_capabilities.import_entries(vec![
        acting_negative("nick", "advisor", base),
        probe_due_negative("nick", "context_management", base),
    ]);

    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let decision = router.unsupported_feature_for_target(
        &target,
        &["advisor".to_string(), "context_management".to_string()],
        &mut admissions,
        &mut strip_keys,
    );
    assert_eq!(decision, None);
    assert_eq!(strip_keys, vec!["advisor".to_string()]);
    assert_eq!(
        admissions.len(),
        1,
        "only the probe admits; the strip records no admission",
    );

    // A full-request 2xx settles exactly the recorded admissions.
    for adm in &admissions {
        router.learned_capabilities.record_probe_outcome(
            &adm.state_key,
            &adm.feature,
            adm.provider_kind,
            crate::learned_capability::ProbeOutcome::Success,
            base,
        );
    }

    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "nick",
            "context_management",
            "openai-compat",
            base,
        ),
        crate::learned_capability::RoutingDecision::Allow,
        "the admitted probe's 2xx cleared its negative",
    );
    assert_eq!(
        router
            .learned_capabilities
            .acting_negative_for("nick", "advisor", "openai-compat", base,),
        crate::learned_capability::RoutingDecision::RouteAway {
            signal: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
        },
        "a stripped success never clears the stripped feature's negative",
    );
}

#[test]
fn probe_bypass_of_strip_eligible_feature_emits_probe_bypassed_warn() {
    // context_management is a probe-due droppable Strip (strip-eligible),
    // web_search is a probe-due essential (route-away). Both are admitted
    // for re-probe, so neither is stripped -- but only the strip-eligible
    // bypass surfaces a `probe_bypassed` WARN at the verdict site; the
    // route-away feature was never strip-eligible and stays silent.
    let router = Router::new(Arc::new(Config::default()));
    let target = strip_target("nick");
    let base = Instant::now();
    router.learned_capabilities.import_entries(vec![
        probe_due_negative("nick", "context_management", base),
        probe_due_negative("nick", "web_search", base),
    ]);

    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let mut decision = None;
    let events = routectl_testkit::capture_events(|| {
        decision = router.unsupported_feature_for_target(
            &target,
            &["context_management".to_string(), "web_search".to_string()],
            &mut admissions,
            &mut strip_keys,
        );
    });

    // Both features are admitted probes: the target stays supported, no
    // key is stripped, and both admissions settle their slots.
    assert_eq!(decision, None);
    assert!(strip_keys.is_empty());
    assert_eq!(admissions.len(), 2);

    // Exactly one `probe_bypassed` WARN fires, for the strip-eligible
    // feature, carrying the capability token and the target's state key.
    let bypass_warns: Vec<_> = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN
                && e.message == "capability_strip_decision"
                && e.field("outcome") == Some("probe_bypassed")
        })
        .collect();
    assert_eq!(
        bypass_warns.len(),
        1,
        "one probe_bypassed WARN for the strip-eligible bypassed feature",
    );
    assert_eq!(
        bypass_warns[0].field("capability_key"),
        Some(normalize_capability_key("context_management", "openai-compat").as_str()),
    );
    assert_eq!(bypass_warns[0].field("event"), Some("strip"));
    assert_eq!(bypass_warns[0].field("state_key"), Some("nick"));
}

#[test]
fn operator_pinned_beta_capability_routes_away_never_strips() {
    // context_management is a droppable beta strip, but the operator pins
    // its beta token via the model's header_extras anthropic-beta floor.
    // Bedrock/Anthropic egresses re-add the token AFTER the canonical
    // strip, so a strip would be a false success -> route away instead.
    let router = Router::new(Arc::new(Config::default()));
    let stub: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "nick".into(),
        captured: Arc::new(ParkingMutex::new(Vec::new())),
    });
    let mut headers = BTreeMap::new();
    headers.insert(
        "anthropic-beta".to_string(),
        "context-management-2025-06-27".to_string(),
    );
    let model =
        Arc::new(ResolvedModel::new("nick", "prov", stub, "upstream").with_header_extras(headers));
    let mut target = into_one_dispatch_target(model);
    target.provider_kind = Some("openai-compat");

    let base = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![acting_negative("nick", "context_management", base)]);

    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let decision = router.unsupported_feature_for_target(
        &target,
        &["context_management".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    assert_eq!(
        decision,
        Some(("context_management".to_string(), FilterSource::Learned)),
        "an operator-pinned beta strip routes away",
    );
    assert!(strip_keys.is_empty());
}

#[test]
fn beta_pinned_reads_provider_and_model_floors_and_ignores_non_beta_strips() {
    // Provider header_extras pins the beta; a tool-shape strip (advisor)
    // carries no beta token and is never pinned.
    let mut config = Config::default();
    let mut provider_headers = BTreeMap::new();
    provider_headers.insert(
        "anthropic-beta".to_string(),
        "context-management-2025-06-27".to_string(),
    );
    config.providers.insert(
        "prov".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: provider_headers,
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: crate::config::ProviderRuntimePolicy::default(),
        },
    );
    let router = Router::new(Arc::new(config));
    let target = strip_target("prov");

    assert!(
        router.beta_pinned_for_target(&target, "context_management"),
        "provider header_extras floor pins the beta",
    );
    assert!(
        !router.beta_pinned_for_target(&target, "advisor"),
        "a tool-shape strip carries no beta token",
    );
}

#[cfg(feature = "bedrock")]
#[test]
fn bedrock_provider_beta_floor_routes_away_never_strips() {
    // Bedrock analogue of `operator_pinned_beta_capability_routes_away_
    // never_strips`: here the beta token is pinned by the Bedrock
    // provider's `anthropic_beta` floor, not by header_extras. The
    // invoke/converse adapters re-add that floor on the wire AFTER the
    // canonical strip, so a BetaFlag strip of a floor-pinned token is a
    // false success -> the target must route away instead of shipping the
    // pinned flag. This pins the `anthropic_beta_floor` source of the
    // guard, which is otherwise exercised only via header_extras.
    use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};
    let mut config = Config::default();
    config.providers.insert(
        "prov".into(),
        ProviderEntry::Bedrock {
            region: "us-west-2".into(),
            api_shape: BedrockApiShapeConfig::Invoke,
            creds: BedrockCredsConfig::DefaultChain,
            user_agent: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: vec!["context-management-2025-06-27".to_string()],
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            runtime: ProviderRuntimePolicy::default(),
        },
    );
    let router = Router::new(Arc::new(config));
    let target = strip_target("prov");

    assert!(
        router.beta_pinned_for_target(&target, "context_management"),
        "the Bedrock provider anthropic_beta floor pins the beta token",
    );

    let base = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![acting_negative("prov", "context_management", base)]);

    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let decision = router.unsupported_feature_for_target(
        &target,
        &["context_management".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    assert_eq!(
        decision,
        Some(("context_management".to_string(), FilterSource::Learned)),
        "a Bedrock floor-pinned beta strip routes away rather than stripping",
    );
    assert!(
        strip_keys.is_empty(),
        "the pinned strip must not be attached to the target",
    );
}

#[test]
fn filter_chain_keeps_stripped_target_and_tails_route_away() {
    // Two targets: one carrying a droppable-only negative
    // (context_management) STAYS supported with the strip key attached;
    // one carrying an essential negative (web_search) is tail-demoted.
    let router = Router::new(Arc::new(Config::default()));
    let strip_t = strip_target("strip-nick");
    let route_t = strip_target("route-nick");
    let base = Instant::now();
    router.learned_capabilities.import_entries(vec![
        acting_negative("strip-nick", "context_management", base),
        acting_negative("route-nick", "web_search", base),
    ]);

    let mut admissions = Vec::new();
    let out = router
        .filter_chain_by_features(
            vec![strip_t, route_t],
            &["context_management".to_string(), "web_search".to_string()],
            "alias",
            &mut admissions,
        )
        .unwrap();

    assert_eq!(out.len(), 2);
    // The strip target stays first (supported); the route-away target is
    // demoted to the tail.
    assert_eq!(out[0].state_key, "strip-nick");
    assert_eq!(
        &*out[0].strip_capabilities,
        &["context_management".to_string()],
    );
    assert_eq!(out[1].state_key, "route-nick");
    assert!(
        out[1].strip_capabilities.is_empty(),
        "a tail-demoted target carries no strip keys",
    );
}

// --- apply_strip_interceptor: outcome mapping, mutation, metrics ---

fn advisor_tool() -> ToolDef {
    ToolDef::Other(json!({"type": "advisor", "name": "advisor"}))
}

fn advisor_request() -> ChatRequest {
    ChatRequest {
        model: "nick".into(),
        messages: vec![].into(),
        tools: Some(vec![advisor_tool()]),
        ..Default::default()
    }
}

fn with_strip_keys(mut target: DispatchTarget, keys: &[&str]) -> DispatchTarget {
    target.strip_capabilities =
        std::sync::Arc::from(keys.iter().map(|k| (*k).to_string()).collect::<Vec<_>>());
    target
}

fn strict_router() -> Router {
    let mut config = Config::default();
    config.server.strict_translation = true;
    Router::new(Arc::new(config))
}

#[test]
fn strip_helper_applies_and_bumps_strip_total() {
    let router = Router::new(Arc::new(Config::default()));
    let target = with_strip_keys(strip_target("nick"), &["advisor"]);
    let mut attempt_req = advisor_request();

    let mut decision = None;
    let events = routectl_testkit::capture_events(|| {
        decision = Some(router.apply_strip_interceptor(&target, &mut attempt_req));
    });
    let decision = decision.expect("interceptor ran");

    assert!(matches!(decision, StripDecision::Proceed));
    assert!(
        attempt_req.tools.is_none(),
        "the sole advisor tool is stripped and the emptied list normalizes to None",
    );
    assert_eq!(router.metrics.strip_total(), 1);
    assert_eq!(router.metrics.strip_rollback_total(), 0);
    assert_eq!(router.metrics.strip_strict_rejected_total(), 0);

    let warn = events
        .iter()
        .find(|e| e.message == "capability_strip_decision")
        .expect("a strip must emit a capability_strip_decision WARN");
    assert_eq!(warn.field("event"), Some("strip"));
    assert_eq!(warn.field("state_key"), Some("nick"));
    assert_eq!(warn.field("capability_key"), Some("advisor"));
    assert_eq!(warn.field("outcome"), Some("applied"));
}

#[test]
fn strip_helper_is_noop_when_surface_absent() {
    // The verdict names advisor, but the request carries no advisor
    // surface: a plain no-op, not a strip -- strip_total stays at zero.
    let router = Router::new(Arc::new(Config::default()));
    let target = with_strip_keys(strip_target("nick"), &["advisor"]);
    let mut attempt_req = ChatRequest {
        model: "nick".into(),
        tools: Some(vec![ToolDef::Other(
            json!({"type": "web_search_20250305", "name": "search"}),
        )]),
        ..Default::default()
    };
    let before = serde_json::to_value(&attempt_req).unwrap();

    let mut decision = None;
    let events = routectl_testkit::capture_events(|| {
        decision = Some(router.apply_strip_interceptor(&target, &mut attempt_req));
    });
    let decision = decision.expect("interceptor ran");

    assert!(matches!(decision, StripDecision::Proceed));
    assert_eq!(serde_json::to_value(&attempt_req).unwrap(), before);
    assert_eq!(router.metrics.strip_total(), 0);

    let warn = events
        .iter()
        .find(|e| e.message == "capability_strip_decision")
        .expect("a no-op strip decision still emits a WARN");
    assert_eq!(warn.field("event"), Some("strip"));
    assert_eq!(warn.field("state_key"), Some("nick"));
    assert_eq!(warn.field("capability_key"), Some("advisor"));
    assert_eq!(warn.field("outcome"), Some("noop"));
}

#[test]
fn strip_helper_strict_rejects_without_mutation() {
    let router = strict_router();
    let target = with_strip_keys(strip_target("nick"), &["advisor"]);
    let mut attempt_req = advisor_request();
    let before = serde_json::to_value(&attempt_req).unwrap();

    let mut decision = None;
    let events = routectl_testkit::capture_events(|| {
        decision = Some(router.apply_strip_interceptor(&target, &mut attempt_req));
    });
    let decision = decision.expect("interceptor ran");

    match decision {
        StripDecision::StrictReject(Error::Validation(msg)) => {
            assert!(msg.contains("advisor"), "{msg}");
        }
        other => panic!("expected StrictReject(Validation), got {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(&attempt_req).unwrap(),
        before,
        "strict mode blocks before any mutation",
    );
    assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
    assert_eq!(router.metrics.strip_total(), 0);
    assert_eq!(router.metrics.strip_rollback_total(), 0);

    let warn = events
        .iter()
        .find(|e| e.message == "capability_strip_decision")
        .expect("a strict rejection still emits a WARN");
    assert_eq!(warn.field("event"), Some("strip"));
    assert_eq!(warn.field("state_key"), Some("nick"));
    assert_eq!(warn.field("capability_key"), Some("advisor"));
    assert_eq!(warn.field("outcome"), Some("strict_rejected"));
}

#[test]
fn strip_helper_rolls_back_and_routes_away() {
    // tool_choice forces the advisor tool the strip removes: a
    // strip-created hazard the post-strip check rolls back. The
    // request is restored and the attempt routes away.
    let router = Router::new(Arc::new(Config::default()));
    let target = with_strip_keys(strip_target("nick"), &["advisor"]);
    let mut attempt_req = ChatRequest {
        model: "nick".into(),
        tools: Some(vec![advisor_tool()]),
        tool_choice: Some(json!({"type": "tool", "name": "advisor"})),
        ..Default::default()
    };
    let before = serde_json::to_value(&attempt_req).unwrap();

    let mut decision = None;
    let events = routectl_testkit::capture_events(|| {
        decision = Some(router.apply_strip_interceptor(&target, &mut attempt_req));
    });
    let decision = decision.expect("interceptor ran");

    assert!(matches!(decision, StripDecision::RouteAway(_)));
    assert_eq!(
        serde_json::to_value(&attempt_req).unwrap(),
        before,
        "the rolled-back request is byte-identical to the pre-strip bytes",
    );
    assert_eq!(router.metrics.strip_rollback_total(), 1);
    assert_eq!(router.metrics.strip_total(), 0);

    let warn = events
        .iter()
        .find(|e| e.message == "capability_strip_decision")
        .expect("a rolled-back strip still emits a WARN");
    assert_eq!(warn.field("event"), Some("strip"));
    assert_eq!(warn.field("state_key"), Some("nick"));
    assert_eq!(warn.field("capability_key"), Some("advisor"));
    assert_eq!(warn.field("outcome"), Some("validation_rolled_back"));
}

#[test]
fn strip_helper_is_inert_with_empty_verdict_even_under_strict() {
    // Kill-switch by construction: an empty strip verdict (disabled
    // learning, probe-admitted, or operator-pinned features) leaves the
    // helper inert -- no mutation, no counter, even under strict.
    let router = strict_router();
    let target = strip_target("nick");
    let mut attempt_req = advisor_request();
    let before = serde_json::to_value(&attempt_req).unwrap();

    let decision = router.apply_strip_interceptor(&target, &mut attempt_req);

    assert!(matches!(decision, StripDecision::Proceed));
    assert_eq!(serde_json::to_value(&attempt_req).unwrap(), before);
    assert_eq!(router.metrics.strip_total(), 0);
    assert_eq!(router.metrics.strip_strict_rejected_total(), 0);
    assert_eq!(router.metrics.strip_rollback_total(), 0);
}

#[cfg(feature = "bedrock")]
#[test]
fn bedrock_target_threads_kind_so_dotted_capability_resolves_to_head() {
    use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};
    use routectl_core::capability::SignalTier;
    use routectl_core::failure_class::{ClassifiedFailure, FailureClass, MatchedBy};

    // A bedrock provider entry keyed by the name the resolved model
    // targets: chain expansion looks it up and threads the kind onto the
    // DispatchTarget. The provider_kind=None regression was that missing
    // thread, which left a dotted Converse field path un-normalized on
    // the learning seam.
    let mut config = Config::default();
    config.providers.insert(
        "bedrock-prov".into(),
        ProviderEntry::Bedrock {
            region: "us-east-1".into(),
            api_shape: BedrockApiShapeConfig::default(),
            creds: BedrockCredsConfig::DefaultChain,
            user_agent: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: vec![],
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            runtime: ProviderRuntimePolicy::default(),
        },
    );
    let router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "bedrock-prov".into(),
        captured: Arc::new(ParkingMutex::new(Vec::new())),
    });
    let model = Arc::new(ResolvedModel::new(
        "bedrock-haiku",
        "bedrock-prov",
        provider,
        "anthropic.claude-x",
    ));

    let targets = router.expand_chain_to_targets(vec![model], None);
    let target = targets.first().expect("one target for a non-seat model");

    // Anti-regression: expansion threaded the concrete kind, not None.
    assert_eq!(target.provider_kind, Some("bedrock"));

    // Resolve-side contract: under the threaded bedrock kind, a dotted
    // request-bag field path reduces to the capability head.
    let err = Error::upstream_full("bedrock-prov", 400, "{}", None, None, None);
    let cf = ClassifiedFailure {
        class: FailureClass::FeatureUnsupported {
            capability: "additionalModelRequestFields.anthropic_beta".into(),
        },
        matched_by: MatchedBy::Status,
    };
    let resolved = resolve_requested_capability(
        target.provider_kind.expect("bedrock kind threaded"),
        &err,
        &cf,
    );
    assert_eq!(
        resolved,
        Some((
            "anthropic_beta".to_string(),
            SignalTier::SelfIdentifying,
            FailurePhase::F1
        )),
    );
}
