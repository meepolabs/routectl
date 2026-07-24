//! Context-reduction wiring on the dispatch path. Asserts the
//! ordering invariant (reduce AFTER overlays, BEFORE auto-cache), the
//! effective-enablement resolution (global AND provider-not-off), and the
//! stable `reduction_strategy` token stamped on `DispatchMeta`. Tests
//! read the captured per-attempt request (the bytes the egress would see)
//! and the returned meta; the original request is never mutated.
use super::*;
use crate::config::{CacheConfig, ProviderEntry, ProviderRuntimePolicy, ReductionConfig};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use parking_lot::Mutex as ParkingMutex;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ContentPart, KnownContentPart, Message,
    MessageContent, Provider, Role,
};
use std::collections::BTreeMap;

struct CapturingProvider {
    id: String,
    captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
}

fn ok_response(model: String) -> ChatResponse {
    ChatResponse {
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
    }
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
        self.captured.lock().push(req);
        Ok(ok_response(model))
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.captured.lock().push(req);
        let s = futures::stream::once(async move {
            Ok(ChatChunk {
                id: "c0".into(),
                model: "x".into(),
                choices: vec![],
                usage: None,
                opaque_events: Vec::new(),
                upstream_meta: None,
            })
        });
        Ok(s.boxed())
    }
}

/// Build a router with one provider entry and one resolved model that
/// dispatches to a CapturingProvider. `reduction_enabled` is the global
/// `[reduction] enabled`; `auto_cache` is the global auto-emit switch.
fn rig(
    entry: ProviderEntry,
    reduction_enabled: bool,
    auto_cache: bool,
) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: auto_cache,
        },
        reduction: ReductionConfig {
            enabled: reduction_enabled,
        },
        retry: RetryPolicy {
            initial_backoff_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config.providers.insert("p".into(), entry);

    let mut router = Router::new(Arc::new(config));
    let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "cap".into(),
        captured: captured.clone(),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let resolved = ResolvedModel::new("m", "p", provider, "upstream-model");
    models.insert("m".into(), Arc::new(resolved));
    router.install_resolved_models(models);
    (router, captured)
}

fn anthropic_entry() -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k")
}

/// Anthropic entry with `reduction_enabled = Some(false)` (provider opt-out).
fn anthropic_entry_reduction_off() -> ProviderEntry {
    ProviderEntry::AnthropicApi {
        api_key_ref: "literal:k".into(),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: Default::default(),
        credential_source: Default::default(),
        header_extras: BTreeMap::new(),
        payload_extras: None,
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: None,
        cache_capability: None,
        auto_emit_top_level_breakpoint: None,
        reduction_enabled: Some(false),
        cloak: routectl_providers::anthropic_api::CloakConfig::default(),
        #[cfg(feature = "bedrock")]
        bedrock_mantle: None,
        runtime: ProviderRuntimePolicy::default(),
    }
}

/// A request whose single mutable-tail message carries a tool_result
/// whose content is a pretty (whitespace-laden) JSON STRING.
fn req_with_pretty_tool_result() -> ChatRequest {
    let pretty = "{\n  \"rows\": [1, 2, 3]\n}";
    ChatRequest {
        model: "m".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: serde_json::json!(pretty),
                    is_error: None,
                    cache_control: None,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        ..Default::default()
    }
}

/// Read the tool_result content string out of the first message's parts.
fn first_tool_result_content(req: &ChatRequest) -> &serde_json::Value {
    let MessageContent::Parts(parts) = &req.messages[0].content else {
        panic!("expected parts");
    };
    let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &parts[0] else {
        panic!("expected tool_result");
    };
    content
}

#[tokio::test]
async fn disabled_by_default_dispatches_unchanged() {
    // Global default off -> apply_json_minify is NOT called; the pretty
    // tool_result string survives verbatim and meta reflects disabled.
    let (router, captured) = rig(anthropic_entry(), false, false);
    let dispatched = router
        .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(dispatched.meta.reduction_strategy, Some("skipped:disabled"));
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        first_tool_result_content(up),
        &serde_json::json!("{\n  \"rows\": [1, 2, 3]\n}"),
        "disabled reduction must leave the pretty JSON string untouched",
    );
}

#[tokio::test]
async fn enabled_globally_compacts_mutable_tail_json() {
    // Global on, provider inherits -> the pretty tool_result string is
    // compacted and meta reports the applied token.
    let (router, captured) = rig(anthropic_entry(), true, false);
    let dispatched = router
        .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(dispatched.meta.reduction_strategy, Some("applied"));
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        first_tool_result_content(up),
        &serde_json::json!("{\"rows\":[1,2,3]}"),
        "enabled reduction must compact the JSON string in the mutable tail",
    );
}

#[tokio::test]
async fn caller_top_level_cache_control_blocks_reduction() {
    // Cache-safety guard at the dispatch boundary: a CALLER top-level
    // cache_control selects Anthropic automatic caching, which freezes the
    // entire prefix. Even with reduction enabled, the tool_result must NOT
    // be compacted -- the dispatched bytes stay verbatim and meta reports
    // no mutable tail.
    let mut req = req_with_pretty_tool_result();
    req.cache_control = Some(routectl_core::CacheControl::ephemeral_5m());
    let (router, captured) = rig(anthropic_entry(), true, false);
    let dispatched = router
        .complete_with_options(req, RouterOptions::default())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(dispatched.meta.reduction_strategy, Some("skipped:no-tail"));
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        first_tool_result_content(up),
        &serde_json::json!("{\n  \"rows\": [1, 2, 3]\n}"),
        "a caller top-level breakpoint must freeze the prefix; reduction must not run",
    );
}

#[tokio::test]
async fn provider_override_off_skips_even_with_global_on() {
    // Global on but provider reduction_enabled = Some(false) -> skipped;
    // the pretty string is untouched.
    let (router, captured) = rig(anthropic_entry_reduction_off(), true, false);
    let dispatched = router
        .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(dispatched.meta.reduction_strategy, Some("skipped:disabled"));
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        first_tool_result_content(up),
        &serde_json::json!("{\n  \"rows\": [1, 2, 3]\n}"),
        "provider opt-out must block reduction even with global on",
    );
}

#[tokio::test]
async fn reduce_runs_before_auto_cache_breakpoint_covers_reduced_bytes() {
    // ORDERING regression: no caller breakpoint, reduction enabled AND
    // auto-emit enabled + capable target. After dispatch the JSON string
    // is compacted AND a top-level cache_control breakpoint is present --
    // proving reduction ran before auto-cache (the auto-emitted
    // breakpoint covers the reduced bytes).
    let (router, captured) = rig(anthropic_entry(), true, true);
    let dispatched = router
        .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(dispatched.meta.reduction_strategy, Some("applied"));
    assert_eq!(dispatched.meta.cache_strategy, Some("auto_emitted"));
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        first_tool_result_content(up),
        &serde_json::json!("{\"rows\":[1,2,3]}"),
        "the dispatched bytes must be the REDUCED string",
    );
    assert_eq!(
        up.cache_control,
        Some(CacheControl::ephemeral_5m()),
        "a top-level breakpoint must be auto-emitted over the reduced request",
    );
}

#[tokio::test]
async fn stream_path_also_reduces() {
    let (router, captured) = rig(anthropic_entry(), true, false);
    let _ = router
        .stream(req_with_pretty_tool_result())
        .await
        .expect("ok")
        .collect::<Vec<_>>()
        .await;
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        first_tool_result_content(up),
        &serde_json::json!("{\"rows\":[1,2,3]}"),
        "stream path must apply reduction like the complete path",
    );
}

#[tokio::test]
async fn stream_reduce_runs_before_auto_cache_breakpoint_covers_reduced_bytes() {
    // ORDERING regression, streaming analogue of the completion-path
    // `reduce_runs_before_auto_cache_breakpoint_covers_reduced_bytes`:
    // no caller breakpoint, reduction enabled AND auto-emit enabled +
    // capable target. After a stream dispatch the captured request's
    // tool_result JSON is compacted AND a top-level cache_control
    // breakpoint is present -- proving reduction ran BEFORE auto-cache on
    // the dominant streaming path. `stream_path_also_reduces` runs with
    // auto_cache OFF, so the interaction is never exercised there: a
    // reorder of the two blocks in `stream_inner` would disable reduction
    // on every auto-breakpoint stream and pass every other test.
    let (router, captured) = rig(anthropic_entry(), true, true);
    let _ = router
        .stream(req_with_pretty_tool_result())
        .await
        .expect("ok")
        .collect::<Vec<_>>()
        .await;
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        first_tool_result_content(up),
        &serde_json::json!("{\"rows\":[1,2,3]}"),
        "the dispatched bytes must be the REDUCED string",
    );
    assert_eq!(
        up.cache_control,
        Some(CacheControl::ephemeral_5m()),
        "a top-level breakpoint must be auto-emitted over the reduced request on the stream path",
    );
}

#[test]
fn strategy_token_maps_every_case_to_stable_string() {
    // Operator-facing contract: pin these tokens exactly.
    assert_eq!(reduction_strategy_token(false, None), "skipped:disabled");
    // Obtain a real `Applied` outcome (the delta type is non-exhaustive
    // and cannot be hand-constructed) by minifying a pretty JSON string.
    let applied = apply_json_minify(&mut req_with_pretty_tool_result());
    assert!(matches!(applied, ReductionOutcome::Applied(_)));
    assert_eq!(reduction_strategy_token(true, Some(&applied)), "applied");
    assert_eq!(
        reduction_strategy_token(true, Some(&ReductionOutcome::NoMutableTail)),
        "skipped:no-tail",
    );
    assert_eq!(
        reduction_strategy_token(true, Some(&ReductionOutcome::NothingToStrip)),
        "skipped:nothing-to-strip",
    );
}
