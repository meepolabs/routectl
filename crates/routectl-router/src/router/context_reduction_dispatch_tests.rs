//! Context-reduction wiring on the dispatch path. Asserts the
//! ordering invariant (reduce AFTER overlays, BEFORE auto-cache), the
//! effective-enablement resolution (global AND provider-not-off), and the
//! stable `reduction_strategy` token stamped on `DispatchMeta`. Tests
//! read the captured per-attempt request (the bytes the egress would see)
//! and the returned meta; the original request is never mutated.
use super::*;
use crate::config::{
    AliasValue, CacheConfig, ProviderEntry, ProviderRuntimePolicy, ReductionConfig,
};
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
            normalize_tools: true,
            k_gated_emission: false,
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

/// Like `rig`, but leaves `reduction` at `ReductionConfig::default()` --
/// the shape of an install whose config never names `[reduction]`.
fn rig_default_reduction(entry: ProviderEntry) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: false,
            normalize_tools: true,
            k_gated_emission: false,
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
    models.insert(
        "m".into(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream-model")),
    );
    router.install_resolved_models(models);
    (router, captured)
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
        auto_emit_per_block_breakpoints: None,
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
        }]
        .into(),
        ..Default::default()
    }
}

/// A request whose mutable tail holds a tool_result carrying plain prose --
/// a candidate target with nothing to strip.
fn req_with_plain_tool_result() -> ChatRequest {
    let mut req = req_with_pretty_tool_result();
    let messages = Arc::make_mut(&mut req.messages);
    let MessageContent::Parts(parts) = &mut messages[0].content else {
        panic!("expected parts");
    };
    let ContentPart::Known(KnownContentPart::ToolResult { content, .. }) = &mut parts[0] else {
        panic!("expected tool_result");
    };
    *content = serde_json::json!("just some text output");
    req
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
async fn explicitly_disabled_dispatches_byte_identical() {
    // Explicit global opt-out (`[reduction] enabled = false`) ->
    // apply_json_minify is NOT called; the pretty tool_result string
    // survives verbatim and meta reflects disabled.
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
async fn default_config_install_runs_the_reducer() {
    // Default-on pin: a config that never names `[reduction]` reduces.
    let (router, captured) = rig_default_reduction(anthropic_entry());
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
        "a default-config install must run the reducer",
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
    // Obtain real outcomes (both payload-carrying variants are
    // non-exhaustive and cannot be hand-constructed) by running the
    // minifier over inputs that land on each one.
    let applied = apply_json_minify(&mut req_with_pretty_tool_result());
    assert!(matches!(applied, ReductionOutcome::Applied(_)));
    assert_eq!(reduction_strategy_token(true, Some(&applied)), "applied");
    assert_eq!(
        reduction_strategy_token(true, Some(&ReductionOutcome::NoMutableTail)),
        "skipped:no-tail",
    );
    let nothing_to_strip = apply_json_minify(&mut req_with_plain_tool_result());
    assert!(matches!(
        nothing_to_strip,
        ReductionOutcome::NothingToStrip(_)
    ));
    assert_eq!(
        reduction_strategy_token(true, Some(&nothing_to_strip)),
        "skipped:nothing-to-strip",
    );
}

/// Shared handle to the requests a capturing provider recorded, in
/// dispatch order.
type Captured = Arc<ParkingMutex<Vec<ChatRequest>>>;

/// Captures each request it is handed (like `CapturingProvider`) and then
/// fails with a fallbackable upstream 503 on BOTH the complete and stream
/// paths. Used as the FIRST chain entry so its per-attempt context
/// reduction fires (mutating its own clone) before it hands the chain off
/// to the second entry.
struct CapturingFailProvider {
    id: String,
    captured: Captured,
}

#[async_trait]
impl Provider for CapturingFailProvider {
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
        self.captured.lock().push(req);
        Err(Error::upstream(&self.id, 503, "entry1 down; fall back"))
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.captured.lock().push(req);
        Err(Error::upstream(&self.id, 503, "entry1 down; fall back"))
    }
}

/// Build a two-entry fallback chain `flow = [entry1, entry2]`.
///
/// entry1 (`CapturingFailProvider`) has reduction ENABLED (inherits the
/// global switch): its per-attempt clone is compacted, then it fails 503
/// and the chain falls over to entry2. entry2 (`CapturingProvider`) has
/// reduction explicitly OFF (`reduction_enabled = Some(false)`) so its own
/// dispatch performs NO mutation -- whatever it captures is exactly the
/// bytes cloned off the shared request. That makes entry2's capture a
/// clean discriminator: if entry1's mutation had leaked into the shared
/// request, entry2 would see the compacted JSON; isolation means it sees
/// the pristine pretty JSON.
///
/// `max_attempts = 1` so entry1 fails fast without burning backoff.
fn two_entry_chain_rig() -> (Router, Captured, Captured) {
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: false,
            normalize_tools: true,
            k_gated_emission: false,
        },
        reduction: ReductionConfig { enabled: true },
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config.aliases.insert(
        "flow".into(),
        AliasValue::Chain(vec!["entry1".into(), "entry2".into()]),
    );
    config.providers.insert("p1".into(), anthropic_entry());
    config
        .providers
        .insert("p2".into(), anthropic_entry_reduction_off());

    let mut router = Router::new(Arc::new(config));
    let cap1: Captured = Arc::new(ParkingMutex::new(Vec::new()));
    let cap2: Captured = Arc::new(ParkingMutex::new(Vec::new()));
    let p1: Arc<dyn Provider> = Arc::new(CapturingFailProvider {
        id: "cap1".into(),
        captured: cap1.clone(),
    });
    let p2: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "cap2".into(),
        captured: cap2.clone(),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "entry1".into(),
        Arc::new(ResolvedModel::new("entry1", "p1", p1, "u1")),
    );
    models.insert(
        "entry2".into(),
        Arc::new(ResolvedModel::new("entry2", "p2", p2, "u2")),
    );
    router.install_resolved_models(models);
    (router, cap1, cap2)
}

/// A `flow` request whose model is the two-entry chain alias.
fn flow_req_with_pretty_tool_result() -> ChatRequest {
    ChatRequest {
        model: "flow".into(),
        ..req_with_pretty_tool_result()
    }
}

#[tokio::test]
async fn fallback_chain_isolates_per_attempt_reduction_from_shared_request() {
    // INVARIANT the P2 Arc<[Message]> copy-on-write must preserve: a
    // per-attempt mutation on the FIRST chain entry (message-tail context
    // reduction) must not leak into the shared request the SECOND entry
    // clones from. entry1 reduces its own clone then 503s; entry2 (reduction
    // off) must see the PRISTINE pretty JSON, proving the mutation stayed
    // local to entry1's attempt.
    let (router, cap1, cap2) = two_entry_chain_rig();
    let dispatched = router
        .complete_with_options(flow_req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched
        .result
        .expect("entry2 serves after entry1 falls back");
    assert_eq!(
        dispatched.meta.fallback_count, 1,
        "the request must have fallen over to the second chain entry",
    );

    let up1 = cap1.lock();
    let up1 = up1.first().expect("entry1 dispatched once");
    assert_eq!(
        first_tool_result_content(up1),
        &serde_json::json!("{\"rows\":[1,2,3]}"),
        "entry1's per-attempt clone must be COMPACTED (its reduction fired)",
    );

    let up2 = cap2.lock();
    let up2 = up2.first().expect("entry2 dispatched once after fallback");
    assert_eq!(
        first_tool_result_content(up2),
        &serde_json::json!("{\n  \"rows\": [1, 2, 3]\n}"),
        "entry2 must receive PRISTINE messages; entry1's mutation must not \
         leak into the shared request",
    );
}

#[tokio::test]
async fn stream_fallback_chain_isolates_per_attempt_reduction_from_shared_request() {
    // Streaming analogue: the same isolation invariant on the stream
    // dispatch clone site. entry1 reduces its own clone then fails
    // stream-open with a fallbackable 503; entry2 (reduction off) opens the
    // stream and must see the PRISTINE pretty JSON.
    let (router, cap1, cap2) = two_entry_chain_rig();
    let dispatched = router
        .stream_with_options(flow_req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    let stream = dispatched
        .result
        .expect("entry2 serves after entry1 falls back");
    let _ = stream.collect::<Vec<_>>().await;
    assert_eq!(
        dispatched.meta.fallback_count, 1,
        "the stream request must have fallen over to the second chain entry",
    );

    let up1 = cap1.lock();
    let up1 = up1.first().expect("entry1 stream-dispatched once");
    assert_eq!(
        first_tool_result_content(up1),
        &serde_json::json!("{\"rows\":[1,2,3]}"),
        "entry1's per-attempt clone must be COMPACTED (its reduction fired)",
    );

    let up2 = cap2.lock();
    let up2 = up2
        .first()
        .expect("entry2 stream-dispatched once after fallback");
    assert_eq!(
        first_tool_result_content(up2),
        &serde_json::json!("{\n  \"rows\": [1, 2, 3]\n}"),
        "entry2 must receive PRISTINE messages on the stream path; entry1's \
         mutation must not leak into the shared request",
    );
}

/// The four `DispatchMeta` reduction counters as a tuple, in
/// compressed / skipped / rejected / bytes order. Comparing the whole tuple
/// keeps a counter that silently stopped being written visible.
type Counters = (Option<u64>, Option<u64>, Option<u64>, Option<u64>);

fn counters(meta: &DispatchMeta) -> Counters {
    (
        meta.reduction_strings_compressed,
        meta.reduction_strings_skipped,
        meta.reduction_strings_rejected,
        meta.reduction_bytes_saved,
    )
}

/// The ledger ONE reduction pass over `req_with_pretty_tool_result` produces,
/// read straight off the minifier. Derived rather than hardcoded so the
/// dispatch-wiring assertions pin the WIRING, not the minifier's byte math.
fn single_pass_pretty_ledger() -> ReductionDelta {
    let ReductionOutcome::Applied(delta) = apply_json_minify(&mut req_with_pretty_tool_result())
    else {
        panic!("the pretty tool_result must be an Applied pass");
    };
    delta
}

/// Captures each request, then fails a bounded number of leading attempts
/// with a retryable upstream 503 before succeeding. Used with a SINGLE-entry
/// chain so the retries are same-target: the reduction helper ran once
/// preparing this target's clone and every retry re-sends those same bytes.
struct FlakyCapturingProvider {
    id: String,
    captured: Captured,
    failures_left: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl Provider for FlakyCapturingProvider {
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
        if self
            .failures_left
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(Error::upstream(&self.id, 503, "flaky; retry me"));
        }
        Ok(ok_response(model))
    }
    async fn stream(&self, _req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream(&self.id, 503, "unused"))
    }
}

/// Single-entry router whose provider fails `failures` attempts with a
/// retryable 503 before serving. `max_attempts` admits `failures + 1`
/// same-target attempts, and there is no second chain entry to fall back to.
fn flaky_retry_rig(failures: u32) -> (Router, Captured) {
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: false,
            normalize_tools: true,
            k_gated_emission: false,
        },
        reduction: ReductionConfig { enabled: true },
        retry: RetryPolicy {
            max_attempts: failures + 1,
            initial_backoff_ms: 0,
            jitter_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config.providers.insert("p".into(), anthropic_entry());

    let mut router = Router::new(Arc::new(config));
    let captured: Captured = Arc::new(ParkingMutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(FlakyCapturingProvider {
        id: "flaky".into(),
        captured: captured.clone(),
        failures_left: std::sync::atomic::AtomicU32::new(failures),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m".into(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream-model")),
    );
    router.install_resolved_models(models);
    (router, captured)
}

/// Two-entry chain where BOTH entries have reduction on (unlike
/// `two_entry_chain_rig`, whose second entry opts out to serve as a
/// mutation-isolation discriminator). entry1 reduces its clone and 503s;
/// entry2 reduces its own fresh clone and serves -- so each entry
/// contributes one pass to the counters.
fn two_reducing_entries_rig() -> (Router, Captured, Captured) {
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: false,
            normalize_tools: true,
            k_gated_emission: false,
        },
        reduction: ReductionConfig { enabled: true },
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config.aliases.insert(
        "flow".into(),
        AliasValue::Chain(vec!["entry1".into(), "entry2".into()]),
    );
    config.providers.insert("p1".into(), anthropic_entry());
    config.providers.insert("p2".into(), anthropic_entry());

    let mut router = Router::new(Arc::new(config));
    let cap1: Captured = Arc::new(ParkingMutex::new(Vec::new()));
    let cap2: Captured = Arc::new(ParkingMutex::new(Vec::new()));
    let p1: Arc<dyn Provider> = Arc::new(CapturingFailProvider {
        id: "cap1".into(),
        captured: cap1.clone(),
    });
    let p2: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "cap2".into(),
        captured: cap2.clone(),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "entry1".into(),
        Arc::new(ResolvedModel::new("entry1", "p1", p1, "u1")),
    );
    models.insert(
        "entry2".into(),
        Arc::new(ResolvedModel::new("entry2", "p2", p2, "u2")),
    );
    router.install_resolved_models(models);
    (router, cap1, cap2)
}

#[tokio::test]
async fn applied_pass_writes_all_four_counters_from_the_delta() {
    // The helper copies the pass's whole ledger onto meta, not just the
    // bytes: an applied pass over one pretty tool_result reports exactly the
    // minifier's own counts.
    let expected = single_pass_pretty_ledger();
    let (router, _captured) = rig(anthropic_entry(), true, false);
    let dispatched = router
        .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched.result.expect("ok");

    assert_eq!(dispatched.meta.reduction_strategy, Some("applied"));
    assert_eq!(
        counters(&dispatched.meta),
        (
            Some(expected.strings_minified as u64),
            Some(expected.strings_skipped as u64),
            Some(expected.strings_rejected as u64),
            Some(expected.bytes_saved as u64),
        ),
    );
    assert!(
        expected.bytes_saved > 0,
        "the fixture must actually save bytes or the assertion above is vacuous",
    );
}

#[tokio::test]
async fn nothing_to_strip_pass_writes_measured_zeros_and_the_skip_count() {
    // A pass that examined the tail and changed nothing is a MEASURED zero,
    // not an absence: compressed / bytes are Some(0) while the untouchable
    // target is accounted for as skipped. `None` would be indistinguishable
    // from "reduction never ran", which is the whole point of the split.
    let (router, _captured) = rig(anthropic_entry(), true, false);
    let dispatched = router
        .complete_with_options(req_with_plain_tool_result(), RouterOptions::default())
        .await;
    dispatched.result.expect("ok");

    assert_eq!(
        dispatched.meta.reduction_strategy,
        Some("skipped:nothing-to-strip")
    );
    assert_eq!(
        counters(&dispatched.meta),
        (Some(0), Some(1), Some(0), Some(0)),
    );
}

#[tokio::test]
async fn disabled_reduction_leaves_every_counter_none() {
    // Reduction never ran, so there is nothing measured to report.
    let (router, _captured) = rig(anthropic_entry(), false, false);
    let dispatched = router
        .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched.result.expect("ok");

    assert_eq!(dispatched.meta.reduction_strategy, Some("skipped:disabled"));
    assert_eq!(counters(&dispatched.meta), (None, None, None, None));
}

#[tokio::test]
async fn no_mutable_tail_leaves_every_counter_none() {
    // A caller top-level breakpoint freezes the whole prefix: no candidate
    // target was ever examined, so there is no classification to report --
    // distinct from a nothing-to-strip pass's measured zeros.
    let mut req = req_with_pretty_tool_result();
    req.cache_control = Some(routectl_core::CacheControl::ephemeral_5m());
    let (router, _captured) = rig(anthropic_entry(), true, false);
    let dispatched = router
        .complete_with_options(req, RouterOptions::default())
        .await;
    dispatched.result.expect("ok");

    assert_eq!(dispatched.meta.reduction_strategy, Some("skipped:no-tail"));
    assert_eq!(counters(&dispatched.meta), (None, None, None, None));
}

#[tokio::test]
async fn same_target_retries_never_re_count_the_prepared_payload() {
    // The helper runs ONCE per chain-entry preparation, outside the retry
    // loop: three same-target attempts re-send the bytes that one pass
    // prepared, so the counters must read exactly one pass. Moving the call
    // inside the retry loop would treble them here while leaving every
    // single-attempt test green.
    let expected = single_pass_pretty_ledger();
    let (router, captured) = flaky_retry_rig(2);
    let dispatched = router
        .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched.result.expect("the third attempt serves");

    assert_eq!(
        captured.lock().len(),
        3,
        "the rig must actually retry the same target twice",
    );
    assert_eq!(
        dispatched.meta.fallback_count, 0,
        "a retry is not a fallback: no second chain entry was prepared",
    );
    assert_eq!(
        counters(&dispatched.meta),
        (
            Some(expected.strings_minified as u64),
            Some(expected.strings_skipped as u64),
            Some(expected.strings_rejected as u64),
            Some(expected.bytes_saved as u64),
        ),
        "three attempts off ONE prepared payload must count once",
    );
}

#[tokio::test]
async fn fallback_entry_preparations_aggregate_the_counters() {
    // The retry case's counterpart: falling over to a second entry prepares
    // a FRESH clone and reduces it again, which is a genuinely new
    // measurement -- so the same two dispatches that count once under retry
    // count twice under fallback.
    let one = single_pass_pretty_ledger();
    let (router, cap1, cap2) = two_reducing_entries_rig();
    let dispatched = router
        .complete_with_options(flow_req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    dispatched
        .result
        .expect("entry2 serves after entry1 falls back");

    assert_eq!(dispatched.meta.fallback_count, 1);
    assert_eq!(cap1.lock().len(), 1, "entry1 dispatched once");
    assert_eq!(cap2.lock().len(), 1, "entry2 dispatched once");
    assert_eq!(
        counters(&dispatched.meta),
        (
            Some(2 * one.strings_minified as u64),
            Some(2 * one.strings_skipped as u64),
            Some(2 * one.strings_rejected as u64),
            Some(2 * one.bytes_saved as u64),
        ),
        "each fallback-entry preparation contributes its own pass",
    );
    assert_eq!(
        dispatched.meta.reduction_strategy,
        Some("applied"),
        "the token stays TERMINAL-target semantics while the counters aggregate",
    );
}

#[tokio::test]
async fn completion_and_stream_paths_report_identical_counters() {
    // Both dispatch paths route through the one shared helper; this pins the
    // observation as equivalent, so a future edit to one call site cannot
    // silently leave the other path's counters unwritten.
    let (completion_router, _c) = rig(anthropic_entry(), true, false);
    let completion = completion_router
        .complete_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    completion.result.expect("ok");

    let (stream_router, _s) = rig(anthropic_entry(), true, false);
    let streamed = stream_router
        .stream_with_options(req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    let _ = streamed.result.expect("ok").collect::<Vec<_>>().await;

    assert_eq!(counters(&streamed.meta), counters(&completion.meta));
    assert_eq!(
        streamed.meta.reduction_strategy,
        completion.meta.reduction_strategy,
    );
}

#[tokio::test]
async fn stream_fallback_entry_preparations_aggregate_the_counters() {
    // Streaming analogue of the fallback aggregation: the stream call site
    // must also sit outside its own inner loop and accumulate per entry.
    let one = single_pass_pretty_ledger();
    let (router, _cap1, _cap2) = two_reducing_entries_rig();
    let dispatched = router
        .stream_with_options(flow_req_with_pretty_tool_result(), RouterOptions::default())
        .await;
    let stream = dispatched
        .result
        .expect("entry2 serves after entry1 falls back");
    let _ = stream.collect::<Vec<_>>().await;

    assert_eq!(dispatched.meta.fallback_count, 1);
    assert_eq!(
        counters(&dispatched.meta),
        (
            Some(2 * one.strings_minified as u64),
            Some(2 * one.strings_skipped as u64),
            Some(2 * one.strings_rejected as u64),
            Some(2 * one.bytes_saved as u64),
        ),
    );
}
