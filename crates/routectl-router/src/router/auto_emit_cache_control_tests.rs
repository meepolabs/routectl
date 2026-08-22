//! Dispatch-path auto-emission of a top-level `cache_control`
//! ephemeral_5m breakpoint. Tests assert on the captured per-attempt
//! request (the bytes the egress would see), and the original request
//! is never mutated.
use super::*;
use crate::config::{CacheConfig, ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use parking_lot::Mutex as ParkingMutex;
use routectl_core::cache_control::compute_frozen_floor;
use routectl_core::{
    CacheControl, ChatChunk, ChatRequest, ChatResponse, Choice, ContentPart, CustomTool,
    KnownContentPart, Message, MessageContent, Provider, Role, SystemBlock, SystemContent, ToolDef,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Captures every dispatched request; can fail the first `fail_first`
/// attempts with a retryable 503 to drive multi-attempt idempotence.
struct CapturingProvider {
    id: String,
    captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
    fail_first: usize,
    seen: AtomicUsize,
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
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_first {
            return Err(Error::upstream(&self.id, 503, "transient"));
        }
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

/// Build a router with one provider entry (its KIND drives capability)
/// and one resolved model that dispatches to a CapturingProvider.
/// `global_enabled` / `provider_override` exercise the kill-switches;
/// `fail_first` drives multi-attempt idempotence.
fn rig(
    entry: ProviderEntry,
    global_enabled: bool,
    fail_first: usize,
) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
    let provider_kind = entry.kind_str();
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: global_enabled,
            normalize_tools: true,
            k_gated_emission: false,
        },
        // Zero backoff keeps the multi-attempt test fast.
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
        fail_first,
        seen: AtomicUsize::new(0),
    });
    // Mirror `factory::apply_catalog_overlay` (empty overlay, no
    // `[cache_pricing]` overrides): this test rig builds `ResolvedModel`
    // directly instead of through the factory, so it must stamp
    // `effective_row` itself the same way -- `record_would_trim` now
    // reads the precomputed merge off the resolved target rather than
    // re-resolving `(provider_kind, upstream)` at dispatch time.
    let baked = crate::catalog::lookup_baked_with_overrides(
        provider_kind,
        "upstream-model",
        None,
        &BTreeMap::new(),
    );
    let effective_row = crate::catalog::merge(baked.as_ref(), None);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let resolved =
        ResolvedModel::new("m", "p", provider, "upstream-model").with_effective_row(effective_row);
    models.insert("m".into(), Arc::new(resolved));
    router.install_resolved_models(models);
    (router, captured)
}

fn anthropic_entry() -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k")
}

fn anthropic_entry_provider_disabled() -> ProviderEntry {
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
        auto_emit_top_level_breakpoint: Some(false),
        auto_emit_per_block_breakpoints: None,
        reduction_enabled: None,
        cloak: routectl_providers::anthropic_api::CloakConfig::default(),
        #[cfg(feature = "bedrock")]
        bedrock_mantle: None,
        runtime: ProviderRuntimePolicy::default(),
    }
}

fn openai_entry() -> ProviderEntry {
    ProviderEntry::openai_compat("https://example.invalid/v1", "literal:k")
}

fn base_req() -> ChatRequest {
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
        ..Default::default()
    }
}

#[tokio::test]
async fn capable_target_no_breakpoint_gets_one_ephemeral_5m() {
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let req = base_req();
    router.complete(req).await.expect("ok");
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        up.cache_control,
        Some(CacheControl::ephemeral_5m()),
        "capable target with no caller breakpoint must get exactly one top-level marker",
    );
}

#[tokio::test]
async fn caller_breakpoint_request_is_byte_identical() {
    // Caller already set a top-level cache_control. Auto-emit must
    // defer entirely; the dispatched request must equal the caller's
    // (no second / rewritten marker). The dispatch path rewrites
    // `model` to the upstream id, so normalize that field out before
    // the byte compare -- everything else, including cache_control,
    // must be untouched.
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let mut req = base_req();
    req.cache_control = Some(CacheControl::ephemeral_1h());
    let mut before = req.clone();
    before.model = "upstream-model".into();
    let before_bytes = serde_json::to_vec(&before).expect("serialize before");
    router.complete(req).await.expect("ok");
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    // Same marker (no second / rewritten one).
    assert_eq!(up.cache_control, Some(CacheControl::ephemeral_1h()));
    let after = serde_json::to_vec(up).expect("serialize after");
    assert_eq!(
        before_bytes, after,
        "caller-supplied request must dispatch byte-identical (modulo upstream model id)",
    );
}

#[tokio::test]
async fn openai_compat_target_gets_no_injection() {
    let (router, captured) = rig(openai_entry(), true, 0);
    router.complete(base_req()).await.expect("ok");
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        up.cache_control, None,
        "openai-compat (no top-level cache_control capability) must not be injected",
    );
}

#[tokio::test]
async fn volatile_high_prefix_blocks_injection() {
    // A UUIDv4 in the system prompt is a high-confidence volatile veto.
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let mut req = base_req();
    req.system = Some(SystemContent::Text(
        "session 550e8400-e29b-41d4-a716-446655440000 active".into(),
    ));
    router.complete(req).await.expect("ok");
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        up.cache_control, None,
        "high-confidence volatile prefix must veto auto-emit",
    );
}

#[tokio::test]
async fn global_kill_switch_off_blocks_injection() {
    let (router, captured) = rig(anthropic_entry(), false, 0);
    router.complete(base_req()).await.expect("ok");
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        up.cache_control, None,
        "global switch off must block auto-emit"
    );
}

#[tokio::test]
async fn provider_kill_switch_off_blocks_injection() {
    let (router, captured) = rig(anthropic_entry_provider_disabled(), true, 0);
    router.complete(base_req()).await.expect("ok");
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        up.cache_control, None,
        "per-provider switch off must block even with global on",
    );
}

#[tokio::test]
async fn provider_switch_true_with_global_true_injects() {
    // Per-provider Some(true) + global true -> injects.
    let mut entry = anthropic_entry_provider_disabled();
    if let ProviderEntry::AnthropicApi {
        auto_emit_top_level_breakpoint,
        ..
    } = &mut entry
    {
        *auto_emit_top_level_breakpoint = Some(true);
    }
    let (router, captured) = rig(entry, true, 0);
    router.complete(base_req()).await.expect("ok");
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
}

#[tokio::test]
async fn injection_is_idempotent_across_attempts() {
    // First attempt 503 (retryable), second ok: both attempt bodies on
    // the same target must be byte-identical -- the decision does not
    // drift between retries.
    let (router, captured) = rig(anthropic_entry(), true, 1);
    router.complete(base_req()).await.expect("ok after retry");
    let captured = captured.lock();
    assert_eq!(captured.len(), 2, "expected one failed + one ok attempt");
    let a = serde_json::to_vec(&captured[0]).expect("serialize attempt 0");
    let b = serde_json::to_vec(&captured[1]).expect("serialize attempt 1");
    assert_eq!(a, b, "retried attempt bodies must be byte-identical");
    assert_eq!(
        captured[0].cache_control,
        Some(CacheControl::ephemeral_5m())
    );
}

#[tokio::test]
async fn injection_lands_on_dispatched_clone_not_the_caller_shape() {
    // The original request is moved into complete(), so it cannot be
    // read back. The meaningful invariant is the SPLIT: the dispatched
    // clone carries the injected marker, while the caller-visible
    // request shape (a freshly built identical request) still carries
    // no cache_control. That gap is exactly what proves the injection
    // touched only the per-attempt clone, never the caller's shape.
    // (The helper-level tests pin the "&mut attempt_req only" contract
    // at the unit boundary.)
    let (router, captured) = rig(anthropic_entry(), true, 0);
    router.complete(base_req()).await.expect("ok");
    // The dispatched clone WAS injected.
    assert_eq!(
        captured.lock().first().expect("dispatch").cache_control,
        Some(CacheControl::ephemeral_5m()),
        "the dispatched clone must carry the injected marker",
    );
    // The caller-visible request shape carries no cache_control.
    assert_eq!(
        base_req().cache_control,
        None,
        "the caller-visible request shape must stay un-injected",
    );
}

#[tokio::test]
async fn stream_path_also_injects() {
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let _ = router
        .stream(base_req())
        .await
        .expect("ok")
        .collect::<Vec<_>>()
        .await;
    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
}

// -- steady-state would-trim advisory (NON-MUTATING recording) ---------

/// A bulky tool_result message (`tokens` tokens at ~4 bytes/token).
fn tool_result_msg(tokens: usize) -> Message {
    let payload = "x".repeat(tokens * 4);
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: serde_json::json!(payload),
            is_error: None,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn text_msg(role: Role, text: &str) -> Message {
    Message {
        refusal: None,
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A long tool-heavy request well above the steady-state trigger, with a
/// head, several bulky old tool turns, and a small recent tail -- so the
/// trimmer proposes a would-cut candidate.
fn long_tool_request() -> ChatRequest {
    let mut messages = vec![
        text_msg(Role::User, "system framing turn one"),
        text_msg(Role::Assistant, "acknowledged"),
    ];
    for _ in 0..12 {
        messages.push(text_msg(Role::Assistant, "calling a tool"));
        messages.push(tool_result_msg(12_000));
    }
    for i in 0..6 {
        messages.push(text_msg(Role::User, &format!("recent turn {i}")));
    }
    ChatRequest {
        model: "m".into(),
        messages: messages.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn would_trim_recorded_for_long_request() {
    // A long request with a would-cut candidate records the freed-token
    // count `d` and a finite break-even K* (anthropic catch-all is a
    // verified write-premium row).
    let (router, _captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let plan = propose_steady_state_trim(&long_tool_request(), &SteadyStateTrimParams::default())
        .expect("trimmer proposes a cut for this request");
    assert_eq!(
        dispatched.meta.would_trim_tokens,
        Some(plan.candidate.d),
        "would_trim_tokens must equal the candidate's freed-token count",
    );
    assert!(
        dispatched.meta.would_trim_break_even_k.is_some(),
        "a verified write-premium row must yield a finite break-even K*",
    );
}

#[tokio::test]
async fn would_trim_records_nothing_for_short_request() {
    // A short request has no would-cut candidate, so both advisory columns
    // stay None (recorded as NULL).
    let (router, _captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(base_req(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(dispatched.meta.would_trim_tokens, None);
    assert_eq!(dispatched.meta.would_trim_break_even_k, None);
}

#[tokio::test]
async fn would_trim_provider_catch_all_row_prices_normally_via_baked_match() {
    // An openai-compat target with no specific cell resolves to the
    // provider's `"*"` catch-all -- a REAL baked-table match (tier 2), so
    // it prices normally. K* suppression is reserved for a `Disabled` /
    // `Missing` merge result (see
    // `record_would_trim_folds_missing_baked_row_to_no_break_even`).
    let (router, _captured) = rig(openai_entry(), true, 0);
    let dispatched = router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let plan = propose_steady_state_trim(&long_tool_request(), &SteadyStateTrimParams::default())
        .expect("trimmer proposes a cut for this request");
    assert_eq!(
        dispatched.meta.would_trim_tokens,
        Some(plan.candidate.d),
        "the catch-all row must record the freed-token count",
    );
    assert!(
        dispatched.meta.would_trim_break_even_k.is_some(),
        "a baked-matched provider catch-all prices",
    );
}

#[tokio::test]
async fn would_trim_recording_does_not_mutate_outbound_request() {
    // CRITICAL non-mutation invariant: the outbound bytes are identical
    // whether or not the recording helper fired. A long request (helper
    // DOES fire) must dispatch byte-identical to the same request built
    // without the recording path -- the helper never calls
    // apply_trim_plan. Compare the captured outbound clone against a fresh
    // copy with only the dispatch-time field changes the helper does NOT
    // own (model id rewrite + the auto-cache marker), proving the message
    // payloads were untouched.
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");
    // The helper recorded a candidate (so it definitely ran).
    assert!(
        dispatched.meta.would_trim_tokens.is_some(),
        "the recording helper must have run for this long request",
    );

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    // The outbound messages are byte-identical to the un-trimmed input:
    // the recording NEVER substituted a placeholder (no apply_trim_plan).
    let sent_messages = serde_json::to_value(&up.messages).expect("serialize sent");
    let original_messages =
        serde_json::to_value(&long_tool_request().messages).expect("serialize original");
    assert_eq!(
        sent_messages, original_messages,
        "would-trim recording must not change the outbound message payloads",
    );
}

#[tokio::test]
async fn would_trim_recorded_on_stream_path_too() {
    // The shared helper is exercised from the streaming path as well as
    // the non-streaming path (mirrors `stream_path_also_injects`).
    let (router, _captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .stream_with_options(long_tool_request(), RouterOptions::new())
        .await;
    let _ = dispatched.result.expect("ok").collect::<Vec<_>>().await;
    assert!(
        dispatched.meta.would_trim_tokens.is_some(),
        "the streaming dispatch path must also record the would-trim advisory",
    );
}

// -- near-lossless would-trim advisory (dedup / supersession / path) ---

/// An assistant `tool_use` turn with the given id and JSON input, for
/// pairing with [`tool_result_of`] (Anthropic-shape tool linkage).
fn tool_use_of(id: &str, input: serde_json::Value) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
            id: id.into(),
            name: "Tool".into(),
            input,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A user `tool_result` turn linked to `tool_use_id`, carrying JSON
/// `content`. Pairs with [`tool_use_of`] via the shared id.
fn tool_result_of(tool_use_id: &str, content: serde_json::Value) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: tool_use_id.into(),
            content,
            is_error: None,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A request built to exercise the near-lossless dedup + supersession
/// heuristics together: a protected head, an oversized filler TEXT turn
/// (clears the estimated-token trigger alone; plain text is never a
/// near-lossless scan unit -- only `ToolResult.content` / `ToolUse.input`
/// are -- so it cannot pollute the attribution counts), three tool
/// call/result pairs sharing path "/a" (t1/v1, t2/v2, t3/v1), and a
/// protected recent tail. Over path "/a" the LATEST result (t3, v1) is
/// the supersession survivor: t2 (v2) differs from it and is elided as
/// stale; t1 (v1) equals it and survives supersession, but is then the
/// FIRST of an exact-duplicate pair, so dedup elides the later copy (t3)
/// instead. Mirrors context_trim.rs's own
/// `supersession_takes_precedence_over_dedup_and_each_unit_marked_once`.
fn near_lossless_attribution_request() -> ChatRequest {
    let v1 = serde_json::json!("V1".repeat(2_000));
    let v2 = serde_json::json!("V2".repeat(2_000));
    let mut messages = vec![
        text_msg(Role::User, "system framing turn one"),
        text_msg(Role::Assistant, "acknowledged"),
        text_msg(Role::User, &"x".repeat(500_000)),
        tool_use_of("t1", serde_json::json!({"file_path": "/a", "call": 1})),
        tool_result_of("t1", v1.clone()),
        tool_use_of("t2", serde_json::json!({"file_path": "/a", "call": 2})),
        tool_result_of("t2", v2),
        tool_use_of("t3", serde_json::json!({"file_path": "/a", "call": 3})),
        tool_result_of("t3", v1),
    ];
    for i in 0..6 {
        messages.push(text_msg(Role::User, &format!("recent turn {i}")));
    }
    ChatRequest {
        model: "m".into(),
        messages: messages.into(),
        ..Default::default()
    }
}

/// Variant of [`rig`] that also installs a `[cache_pricing]` override
/// table, for exercising `would_trim_context_fraction` against a known
/// (non-`None`) context window.
fn rig_with_cache_pricing_override(
    entry: ProviderEntry,
    cache_pricing: BTreeMap<String, crate::catalog::CachePricingOverride>,
) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
    let provider_kind = entry.kind_str();
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: true,
            normalize_tools: true,
            k_gated_emission: false,
        },
        cache_pricing,
        ..Config::default()
    };
    config.providers.insert("p".into(), entry);

    // Mirror `factory::apply_catalog_overlay` (empty overlay, but the
    // SAME `[cache_pricing]` overrides `config` carries) -- see the
    // matching note on `rig` above.
    let baked = crate::catalog::lookup_baked_with_overrides(
        provider_kind,
        "upstream-model",
        None,
        &config.cache_pricing,
    );
    let effective_row = crate::catalog::merge(baked.as_ref(), None);

    let mut router = Router::new(Arc::new(config));
    let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "cap".into(),
        captured: captured.clone(),
        fail_first: 0,
        seen: AtomicUsize::new(0),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let resolved =
        ResolvedModel::new("m", "p", provider, "upstream-model").with_effective_row(effective_row);
    models.insert("m".into(), Arc::new(resolved));
    router.install_resolved_models(models);
    (router, captured)
}

#[tokio::test]
async fn would_trim_near_lossless_attribution_records_dedup_and_supersession() {
    // A known duplicate result (t3 is a later exact copy of t1) and a
    // known supersession (t2 differs from the survivor t3) over path
    // "/a" must be attributed to the correct heuristic, with the path
    // count-pair reflecting all three results resolving to a path.
    let (router, _captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(near_lossless_attribution_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    assert!(
        dispatched
            .meta
            .would_trim_dedup_tokens
            .is_some_and(|t| t > 0),
        "the exact-duplicate result must be attributed to dedup",
    );
    assert!(
        dispatched
            .meta
            .would_trim_supersession_tokens
            .is_some_and(|t| t > 0),
        "the stale differing result must be attributed to supersession",
    );
    assert_eq!(
        dispatched.meta.would_trim_path_units,
        Some(3),
        "all three tool_result units are path-attribution candidates",
    );
    assert_eq!(
        dispatched.meta.would_trim_path_extractable,
        Some(3),
        "all three results resolve to path \"/a\" via their paired tool_use",
    );
    assert!(
        dispatched.meta.would_trim_raw_marks.is_some(),
        "a trigger-clearing request with marks must record the raw-marks blob",
    );
}

#[tokio::test]
async fn near_lossless_pass_does_not_mutate_outbound_request() {
    // CRITICAL non-mutation invariant, exercised against a request whose
    // near-lossless pass definitely finds marks (unlike
    // `would_trim_recording_does_not_mutate_outbound_request`, which only
    // pins the size-baseline plan): outbound bytes must stay
    // byte-identical to the un-elided input. The near-lossless pass is a
    // pure read -- it never calls `apply_trim_plan`.
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(near_lossless_attribution_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");
    assert!(
        dispatched
            .meta
            .would_trim_dedup_tokens
            .is_some_and(|t| t > 0)
            || dispatched
                .meta
                .would_trim_supersession_tokens
                .is_some_and(|t| t > 0),
        "the near-lossless pass must have found marks for this request",
    );

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    let sent_messages = serde_json::to_value(&up.messages).expect("serialize sent");
    let original_messages = serde_json::to_value(&near_lossless_attribution_request().messages)
        .expect("serialize original");
    assert_eq!(
        sent_messages, original_messages,
        "the near-lossless pass must not change the outbound message payloads",
    );
}

#[tokio::test]
async fn would_trim_context_fraction_is_none_when_window_unknown() {
    // The anthropic-api "*" catch-all row has no confirmed context
    // window (`max_context_tokens: None`), so `context_fraction` must
    // fail closed to `None` rather than guess.
    let (router, _captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(dispatched.meta.would_trim_context_fraction, None);
}

#[tokio::test]
async fn would_trim_context_fraction_is_some_when_window_known() {
    // An operator override on the context window turns
    // `context_fraction` into a computed `Some(fraction)`.
    let overrides = BTreeMap::from([(
        "anthropic-api:*".to_string(),
        crate::catalog::CachePricingOverride {
            max_context_tokens: Some(1_000_000),
            ..Default::default()
        },
    )]);
    let (router, captured) = rig_with_cache_pricing_override(anthropic_entry(), overrides);
    let dispatched = router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");
    // Compute the expected fraction against the ACTUAL dispatched clone
    // (post overlay/reduction/auto-cache mutation), since those run
    // before the advisory records and change the serialized byte count.
    let up = captured.lock().first().cloned().expect("one dispatch");
    let expected_fraction = estimate_total_tokens(&up) as f64 / 1_000_000.0;
    assert_eq!(
        dispatched.meta.would_trim_context_fraction,
        Some(expected_fraction),
    );
}

#[tokio::test]
async fn would_trim_recorder_version_stamped_when_trigger_clears() {
    let (router, _captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(
        dispatched.meta.would_trim_recorder_version,
        Some(NEAR_LOSSLESS_RECORDER_VERSION),
        "a trigger-clearing row must be stamped with the recorder version",
    );
}

#[tokio::test]
async fn would_trim_recorder_version_is_none_below_trigger() {
    let (router, _captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(base_req(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");
    assert_eq!(
        dispatched.meta.would_trim_recorder_version, None,
        "a below-trigger row must not be stamped (the pass never ran)",
    );
}

/// Two-entry fallback chain where the two targets make OPPOSITE
/// injection decisions: target 1 (openai-compat, no capability) always
/// fails and injects nothing; target 2 (anthropic-api, capable) serves
/// the request and gets exactly one top-level marker. The marker on
/// target 2 must derive ONLY from the original request, never
/// accumulating from target 1's attempt -- the per-target clone is
/// rebuilt from `req` each hop, so target 2's bytes equal a freshly
/// injected original.
#[tokio::test]
async fn fallback_targets_decide_independently_without_accumulation() {
    let cap_a: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let cap_b: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));

    let mut config = Config {
        retry: RetryPolicy {
            initial_backoff_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config.providers.insert(
        "p-compat".into(),
        ProviderEntry::openai_compat("https://example.invalid/v1", "literal:k"),
    );
    config.providers.insert(
        "p-anthropic".into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(vec!["m-compat".into(), "m-anthropic".into()]),
    );

    // Target 1 always fails (large fail_first) so dispatch falls back
    // to target 2. openai-compat capability is false -> no injection.
    let prov_a: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "p-compat".into(),
        captured: cap_a.clone(),
        fail_first: usize::MAX,
        seen: AtomicUsize::new(0),
    });
    // Target 2 serves the request; anthropic-api is capable -> inject.
    let prov_b: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "p-anthropic".into(),
        captured: cap_b.clone(),
        fail_first: 0,
        seen: AtomicUsize::new(0),
    });

    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m-compat".into(),
        Arc::new(ResolvedModel::new(
            "m-compat",
            "p-compat",
            prov_a,
            "upstream-compat",
        )),
    );
    models.insert(
        "m-anthropic".into(),
        Arc::new(ResolvedModel::new(
            "m-anthropic",
            "p-anthropic",
            prov_b,
            "upstream-anthropic",
        )),
    );

    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);

    let req = ChatRequest {
        model: "alias".into(),
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
        ..Default::default()
    };
    router.complete(req).await.expect("falls back and serves");

    // Target 1: incapable -> dispatched with no injected marker.
    let a = cap_a.lock();
    let up_a = a.first().expect("target 1 dispatched");
    assert_eq!(
        up_a.cache_control, None,
        "incapable openai-compat target must receive no auto-emitted marker",
    );

    // Target 2: capable -> exactly one top-level ephemeral_5m marker,
    // derived only from the original request (not accumulated).
    let b = cap_b.lock();
    let up_b = b.first().expect("target 2 dispatched");
    assert_eq!(
        up_b.cache_control,
        Some(CacheControl::ephemeral_5m()),
        "capable target must get exactly one top-level marker",
    );

    // Non-accumulation: target 2's bytes equal an independently
    // injected copy of the ORIGINAL request (model normalized to the
    // upstream id), proving the clone was rebuilt from `req`, not
    // carried over from target 1's attempt.
    let mut expected = ChatRequest {
        model: "upstream-anthropic".into(),
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
        ..Default::default()
    };
    expected.cache_control = Some(CacheControl::ephemeral_5m());
    assert_eq!(
        serde_json::to_vec(up_b).expect("serialize target 2"),
        serde_json::to_vec(&expected).expect("serialize expected"),
        "target 2 bytes must derive only from the original request",
    );
}

// ---- helper-level unit tests (direct gate-predicate coverage) ----

fn plan(
    caller_breakpoints: usize,
    volatile_high: bool,
    global: bool,
    req: &ChatRequest,
) -> AutoCacheRequestPlan {
    // Build off the real req for the floor, then override the snapshot
    // fields to construct gate situations precisely.
    let mut p = AutoCacheRequestPlan::build(req, global);
    p.has_caller_breakpoints = caller_breakpoints > 0;
    p.caller_breakpoint_count = caller_breakpoints;
    p.volatile_high_veto = volatile_high;
    p
}

/// Gates for a per-block-CAPABLE target (anthropic-api / Converse
/// shaped), with each marker's operator switch given explicitly. Use
/// [`gates_front_unsupported`] for the kinds whose egress cannot carry a
/// per-block marker.
const fn gates(
    capability: Option<CacheCapability>,
    terminal: bool,
    front: bool,
) -> CacheTargetGates {
    CacheTargetGates {
        capability,
        terminal_enabled: terminal,
        front_supported: true,
        front_enabled: front,
    }
}

/// Gates for a target whose egress cannot carry a per-block marker
/// (openai-compat, Bedrock Invoke, openai-responses, gemini), with the
/// operator switch nonetheless opted IN -- the inert-not-honored case.
const fn gates_front_unsupported(capability: Option<CacheCapability>) -> CacheTargetGates {
    CacheTargetGates {
        capability,
        terminal_enabled: true,
        front_supported: false,
        front_enabled: true,
    }
}

#[test]
fn helper_emits_on_clean_capable_request() {
    let mut req = base_req();
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, false), false);
    assert_eq!(out.terminal, CacheInjection::Emitted);
    assert_eq!(req.cache_control, Some(CacheControl::ephemeral_5m()));
}

#[test]
fn helper_fails_closed_on_unknown_capability() {
    let mut req = base_req();
    let p = plan(0, false, true, &req);
    let out = apply_auto_cache_placement(&mut req, &p, gates(None, true, false), false);
    assert_eq!(out.terminal, CacheInjection::SkippedNoCapability);
    assert_eq!(req.cache_control, None);
}

#[test]
fn helper_rolls_back_when_validation_fails() {
    // Craft a situation the black-box gate cannot reach: the plan says
    // "no caller breakpoints" (so the gate proceeds past the
    // SkippedCallerSupplied / cap checks), but the actual attempt_req
    // already carries MAX_BREAKPOINTS caller markers. Injecting the
    // top-level marker pushes the total to MAX+1, so post-injection
    // validate_source fails and the helper restores the original
    // (absent top-level marker).
    let mut req = base_req();
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: "t".into(),
        description: Some("d".into()),
        input_schema: serde_json::json!({"type": "object"}),
        cache_control: Some(CacheControl::ephemeral_5m()),
        defer_loading: None,
        strict: None,
        type_tag: None,
    })]);
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "s".into(),
        cache_control: Some(CacheControl::ephemeral_5m()),
        citations: None,
    }]));
    let part = |t: &str| {
        ContentPart::Known(KnownContentPart::Text {
            text: t.into(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_5m()),
        })
    };
    req.messages = vec![Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![part("a"), part("b")]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }]
    .into();
    // Sanity: the request already sits at MAX_BREAKPOINTS (1 tool + 1
    // system + 2 message parts).
    assert_eq!(
        compute_frozen_floor(&req).caller_breakpoint_count(),
        MAX_BREAKPOINTS,
    );
    // Force the plan to claim no caller breakpoints so the gate
    // proceeds to the validate step -- the only path to the rollback
    // branch given the production no-caller gate.
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, false), false);
    assert_eq!(out.terminal, CacheInjection::ValidationRolledBack);
    assert_eq!(
        req.cache_control, None,
        "rollback must restore the original (absent) top-level marker",
    );
}

#[test]
fn helper_caller_supplied_dominates_all_per_target_skip_reasons() {
    // Arrange: a request that already carries caller breakpoints. This is
    // a request-level fact, so it must dominate every per-target / config
    // skip reason regardless of capability or kill-switch state.
    let mut req = base_req();
    let p = plan(1, false, true, &req);

    // Act + Assert: capability unknown (None) -> caller_supplied, NOT
    // no_capability (the key precedence change).
    let out = apply_auto_cache_placement(&mut req, &p, gates(None, true, true), false);
    assert_eq!(out.front, CacheInjection::SkippedCallerSupplied);
    assert_eq!(out.terminal, CacheInjection::SkippedCallerSupplied);
    assert_eq!(
        req.cache_control, None,
        "caller_supplied path must leave attempt_req.cache_control untouched",
    );

    // Global kill-switch off -> caller still dominates.
    let p_global_off = plan(1, false, false, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p_global_off, gates(cap, true, true), false);
    assert_eq!(out.terminal, CacheInjection::SkippedCallerSupplied);
    assert_eq!(req.cache_control, None);

    // Per-provider kill-switches off -> caller still dominates.
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, false, false), false);
    assert_eq!(out.front, CacheInjection::SkippedCallerSupplied);
    assert_eq!(out.terminal, CacheInjection::SkippedCallerSupplied);
    assert_eq!(req.cache_control, None);

    // Volatile-high veto must not override caller_supplied either.
    let p_volatile = plan(1, true, true, &req);
    let out = apply_auto_cache_placement(&mut req, &p_volatile, gates(cap, true, true), false);
    assert_eq!(out.terminal, CacheInjection::SkippedCallerSupplied);
    assert_eq!(req.cache_control, None);
}

#[test]
fn strategy_str_maps_every_variant_to_stable_token() {
    // Operator-facing contract: these tokens are recorded in the usage
    // DB and matched by the thrash predicate. Pin them exactly.
    assert_eq!(CacheInjection::Emitted.strategy_str(), "auto_emitted");
    assert_eq!(
        CacheInjection::SkippedCallerSupplied.strategy_str(),
        "caller_supplied",
    );
    assert_eq!(
        CacheInjection::SkippedVolatileHigh.strategy_str(),
        "volatile_vetoed",
    );
    assert_eq!(
        CacheInjection::SkippedGlobalDisabled.strategy_str(),
        "auto_skipped:global_disabled",
    );
    assert_eq!(
        CacheInjection::SkippedProviderDisabled.strategy_str(),
        "auto_skipped:provider_disabled",
    );
    assert_eq!(
        CacheInjection::SkippedNoCapability.strategy_str(),
        "auto_skipped:no_capability",
    );
    assert_eq!(
        CacheInjection::SkippedBreakpointCap.strategy_str(),
        "auto_skipped:breakpoint_cap",
    );
    assert_eq!(
        CacheInjection::SkippedNoPlacementRegion.strategy_str(),
        "auto_skipped:no_placement_region",
    );
    assert_eq!(
        CacheInjection::ValidationRolledBack.strategy_str(),
        "auto_skipped:validation_rolled_back",
    );
}

// ---- transactional front + terminal placement (D4) -------------------

/// A request whose system is a two-block `Blocks` array, so the front
/// anchor resolves to `LastSystemBlock { block_index: 1 }`.
fn blocks_system_req() -> ChatRequest {
    let mut req = base_req();
    req.system = Some(SystemContent::Blocks(vec![
        SystemBlock {
            kind: "text".into(),
            text: "first block".into(),
            cache_control: None,
            citations: None,
        },
        SystemBlock {
            kind: "text".into(),
            text: "second block".into(),
            cache_control: None,
            citations: None,
        },
    ]));
    req
}

/// A request whose only front anchor is a custom tool (flat-string system
/// offers no per-block marker field).
fn tools_only_req() -> ChatRequest {
    let mut req = base_req();
    req.system = Some(SystemContent::Text("flat system".into()));
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: "t".into(),
        description: Some("d".into()),
        input_schema: serde_json::json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })]);
    req
}

/// The front marker on `req`, if any: the resolved system block's or the
/// custom tool's `cache_control`.
fn front_marker(req: &ChatRequest) -> Option<CacheControl> {
    if let Some(SystemContent::Blocks(blocks)) = req.system.as_ref()
        && let Some(cc) = blocks.iter().rev().find_map(|b| b.cache_control.clone())
    {
        return Some(cc);
    }
    req.tools
        .as_ref()?
        .iter()
        .rev()
        .find_map(routectl_core::ToolDef::cache_control)
}

#[tokio::test]
async fn clean_anthropic_request_commits_front_and_terminal_together() {
    // Acceptance: a no-marker Anthropic request with a clean prefix gets
    // BOTH markers after one validation. anthropic-api on the default
    // base URL defaults `auto_emit_per_block_breakpoints` on, so the rig
    // needs no override.
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let dispatched = router
        .complete_with_options(blocks_system_req(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        front_marker(up),
        Some(CacheControl::ephemeral_5m()),
        "the front marker must land on the last wire-eligible system block",
    );
    assert_eq!(
        up.cache_control,
        Some(CacheControl::ephemeral_5m()),
        "the terminal marker must still be emitted alongside the front one",
    );
    // The front marker is on the LAST eligible block only -- never both.
    let Some(SystemContent::Blocks(blocks)) = up.system.as_ref() else {
        panic!("system stays in Blocks form");
    };
    assert_eq!(blocks[0].cache_control, None);
    assert_eq!(blocks[1].cache_control, Some(CacheControl::ephemeral_5m()));

    assert_eq!(dispatched.meta.cache_front_decision, Some("auto_emitted"));
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("auto_emitted")
    );
}

#[tokio::test]
async fn volatile_high_prefix_withholds_the_front_marker_with_a_reason() {
    // Acceptance: a high-confidence volatile prefix vetoes the FRONT
    // marker (and the terminal one, unchanged from before), each with the
    // recorded `volatile_vetoed` reason.
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let mut req = blocks_system_req();
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "session 550e8400-e29b-41d4-a716-446655440000 active".into(),
        cache_control: None,
        citations: None,
    }]));
    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        front_marker(up),
        None,
        "a volatile prefix vetoes the front marker"
    );
    assert_eq!(up.cache_control, None);
    assert_eq!(
        dispatched.meta.cache_front_decision,
        Some("volatile_vetoed")
    );
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("volatile_vetoed"),
    );
}

#[test]
fn front_and_terminal_roll_back_together_on_validation_failure() {
    // Acceptance: on validation failure the WHOLE candidate is discarded
    // and BOTH markers record `validation_rollback`. Craft the situation
    // the black-box gate cannot reach: the plan claims no caller
    // breakpoints while the request already carries MAX_BREAKPOINTS, so
    // adding two more busts the cap inside `validate_source`.
    let mut req = blocks_system_req();
    let marked_part = |t: &str| {
        ContentPart::Known(KnownContentPart::Text {
            text: t.into(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_5m()),
        })
    };
    req.messages = vec![Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![
            marked_part("a"),
            marked_part("b"),
            marked_part("c"),
            marked_part("d"),
        ]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }]
    .into();
    assert_eq!(
        compute_frozen_floor(&req).caller_breakpoint_count(),
        MAX_BREAKPOINTS,
    );
    let before = serde_json::to_vec(&req).expect("serialize before");

    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, true), false);

    assert_eq!(out.front, CacheInjection::ValidationRolledBack);
    assert_eq!(out.terminal, CacheInjection::ValidationRolledBack);
    assert_eq!(
        serde_json::to_vec(&req).expect("serialize after"),
        before,
        "a discarded candidate must leave the request byte-identical",
    );
}

#[test]
fn rollback_preserves_overlay_mutations_and_every_cache_slot() {
    // Acceptance: rollback restores tools / system / messages AND the
    // top-level slot while preserving the overlay/strip/minify mutations
    // that ran BEFORE placement. Modeled by mutating the request first
    // (standing in for those earlier steps), then forcing a rollback: the
    // pre-placement mutations must survive and no cache slot may change.
    let mut req = tools_only_req();
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "system after overlay".into(),
        cache_control: None,
        citations: None,
    }]));
    // Pre-placement mutations, as overlay / minify would leave them.
    req.max_tokens = Some(4096);
    req.messages = vec![
        text_msg(Role::User, "minified turn"),
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "p1".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "p2".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "p3".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "p4".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
    ]
    .into();
    let before = serde_json::to_vec(&req).expect("serialize before");

    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, true), false);
    assert_eq!(out.front, CacheInjection::ValidationRolledBack);
    assert_eq!(out.terminal, CacheInjection::ValidationRolledBack);

    // Every pre-placement mutation survived, and no cache slot moved:
    // tools, system blocks, message parts, and the top-level field.
    assert_eq!(
        serde_json::to_vec(&req).expect("serialize after"),
        before,
        "rollback must preserve overlay mutations and restore every cache slot",
    );
    assert_eq!(req.max_tokens, Some(4096));
    assert_eq!(front_marker(&req), None);
    assert_eq!(req.cache_control, None);
}

#[test]
fn front_missing_skips_only_the_front_marker() {
    // Acceptance: a flat-string system with no custom tool offers no
    // front anchor -- the front marker records `no_placement_region`
    // while the terminal marker still emits.
    let mut req = base_req();
    req.system = Some(SystemContent::Text("flat system".into()));
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, true), false);

    assert_eq!(out.front, CacheInjection::SkippedNoPlacementRegion);
    assert_eq!(out.terminal, CacheInjection::Emitted);
    assert_eq!(front_marker(&req), None);
    assert_eq!(req.cache_control, Some(CacheControl::ephemeral_5m()));
}

#[test]
fn terminal_missing_skips_only_the_terminal_marker() {
    // Acceptance: a target with no top-level capability (Bedrock
    // Converse) skips the terminal marker while the opted-in front
    // marker still emits -- the Converse front-only layout (D2).
    let mut req = blocks_system_req();
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(false, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, true), false);

    assert_eq!(out.front, CacheInjection::Emitted);
    assert_eq!(out.terminal, CacheInjection::SkippedNoCapability);
    assert_eq!(front_marker(&req), Some(CacheControl::ephemeral_5m()));
    assert_eq!(
        req.cache_control, None,
        "a Converse target must receive no top-level marker",
    );
}

#[test]
fn both_missing_skips_both_with_their_own_reasons() {
    // Acceptance: independent skips compose -- no front anchor AND no
    // top-level capability records each marker's own distinct reason.
    let mut req = base_req();
    req.system = Some(SystemContent::Text("flat system".into()));
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(false, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, true), false);

    assert_eq!(out.front, CacheInjection::SkippedNoPlacementRegion);
    assert_eq!(out.terminal, CacheInjection::SkippedNoCapability);
    assert_eq!(front_marker(&req), None);
    assert_eq!(req.cache_control, None);
}

#[test]
fn no_placement_region_survives_a_validation_rollback() {
    // Acceptance: on validation failure a marker already skipped for
    // `no_placement_region` keeps THAT reason rather than being
    // overwritten with `validation_rollback`.
    let mut req = base_req();
    req.system = Some(SystemContent::Text("flat system".into()));
    let marked_part = |t: &str| {
        ContentPart::Known(KnownContentPart::Text {
            text: t.into(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_5m()),
        })
    };
    req.messages = vec![Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![
            marked_part("a"),
            marked_part("b"),
            marked_part("c"),
            marked_part("d"),
        ]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }]
    .into();
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, true), false);

    assert_eq!(
        out.front,
        CacheInjection::SkippedNoPlacementRegion,
        "a no_placement_region skip must not be relabeled by the rollback",
    );
    assert_eq!(out.terminal, CacheInjection::ValidationRolledBack);
}

#[test]
fn front_marker_lands_on_the_last_custom_tool_when_system_is_flat() {
    // D3's tools fallback: no wire-eligible system block, so the anchor
    // resolves to the last custom tool definition.
    let mut req = tools_only_req();
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, true), false);

    assert_eq!(out.front, CacheInjection::Emitted);
    let tools = req.tools.as_ref().expect("tools present");
    let ToolDef::Custom(tool) = &tools[0] else {
        panic!("the tool stays a typed custom tool");
    };
    assert_eq!(tool.cache_control, Some(CacheControl::ephemeral_5m()));
}

#[test]
fn placement_fails_closed_when_the_resolved_slot_no_longer_matches() {
    // The slot index is resolved off the ORIGINAL request, above the chain
    // loop; a dispatch-time step could reshape system / tools before
    // placement runs. Placement must then skip rather than mark whichever
    // element now sits at that index. Simulated by building the plan
    // against a two-block system and then shrinking the request's system
    // so the resolved index dangles.
    let source = blocks_system_req();
    let p = plan(0, false, true, &source);
    assert_eq!(
        p.front_slot,
        Some(routectl_core::FrontSlot::LastSystemBlock { block_index: 1 }),
    );

    let mut req = blocks_system_req();
    let Some(SystemContent::Blocks(blocks)) = req.system.as_mut() else {
        panic!("system stays in Blocks form");
    };
    blocks.truncate(1);
    let before = serde_json::to_vec(&req).expect("serialize before");

    let out = apply_auto_cache_placement(
        &mut req,
        &p,
        gates(Some(CacheCapability::new(false, true)), true, true),
        false,
    );
    assert_eq!(
        out.front,
        CacheInjection::SkippedNoPlacementRegion,
        "a dangling slot index must fail closed, never mark a different element",
    );
    assert_eq!(
        serde_json::to_vec(&req).expect("serialize after"),
        before,
        "a failed-closed front placement must leave the request untouched",
    );
}

#[test]
fn placement_skips_a_slot_that_became_wire_ineligible() {
    // Same fail-closed rule for a block still AT the resolved index but no
    // longer wire-eligible (blanked by an earlier dispatch-time step): the
    // egress would drop a marker on a blank block, so placement declines.
    let source = blocks_system_req();
    let p = plan(0, false, true, &source);

    let mut req = blocks_system_req();
    let Some(SystemContent::Blocks(blocks)) = req.system.as_mut() else {
        panic!("system stays in Blocks form");
    };
    blocks[1].text = "   ".into();

    let out = apply_auto_cache_placement(
        &mut req,
        &p,
        gates(Some(CacheCapability::new(false, true)), true, true),
        false,
    );
    assert_eq!(out.front, CacheInjection::SkippedNoPlacementRegion);
    assert_eq!(front_marker(&req), None);
}

#[tokio::test]
async fn cache_auto_decision_line_carries_tokens_only() {
    // Log hygiene: the decision line names the provider, the model, and
    // the stable tokens -- never prompt content. Assert on the actual
    // captured lines, with distinctive content planted in every field the
    // request carries a marker near.
    const SYSTEM_PROBE: &str = "SYSTEM-PROBE-DO-NOT-LOG";
    const MESSAGE_PROBE: &str = "MESSAGE-PROBE-DO-NOT-LOG";
    const TOOL_PROBE: &str = "TOOL-PROBE-DO-NOT-LOG";

    let (router, _captured) = rig(anthropic_entry(), true, 0);
    let mut req = blocks_system_req();
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: SYSTEM_PROBE.into(),
        cache_control: None,
        citations: None,
    }]));
    req.messages = vec![text_msg(Role::User, MESSAGE_PROBE)].into();
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: TOOL_PROBE.into(),
        description: Some(TOOL_PROBE.into()),
        input_schema: serde_json::json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })]);

    let (result, lines) = routectl_testkit::capture_lines(router.complete(req)).await;
    result.expect("ok");

    let decision_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("cache_auto_decision"))
        .collect();
    assert!(
        !decision_lines.is_empty(),
        "the decision line must be emitted",
    );
    for line in &decision_lines {
        for probe in [SYSTEM_PROBE, MESSAGE_PROBE, TOOL_PROBE] {
            assert!(
                !line.contains(probe),
                "cache_auto_decision leaked request content ({probe}): {line}",
            );
        }
        assert!(
            line.contains("front_decision") && line.contains("terminal_decision"),
            "the decision line must carry both per-marker tokens: {line}",
        );
    }
}

#[test]
fn front_gate_off_withholds_only_the_front_marker() {
    // The new knob is an independent kill: with per-block emission off,
    // the terminal marker is unchanged from today's behavior.
    let mut req = blocks_system_req();
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, false), false);

    assert_eq!(out.front, CacheInjection::SkippedProviderDisabled);
    assert_eq!(out.terminal, CacheInjection::Emitted);
    assert_eq!(front_marker(&req), None);
    assert_eq!(req.cache_control, Some(CacheControl::ephemeral_5m()));
}

#[test]
fn explicit_opt_in_is_inert_where_the_wire_cannot_carry_the_marker() {
    // FAIL-CLOSED: an operator `auto_emit_per_block_breakpoints = true` on
    // a kind whose egress drops (or 400s on) a per-block marker must NOT
    // emit one. Emitting would ship a marker the wire discards AND record
    // a false `auto_emitted` in the decision ledger.
    let mut req = blocks_system_req();
    let p = plan(0, false, true, &req);
    let before = serde_json::to_vec(&req).expect("serialize before");

    let out = apply_auto_cache_placement(
        &mut req,
        &p,
        gates_front_unsupported(Some(CacheCapability::new(true, true))),
        false,
    );

    assert_eq!(
        out.front,
        CacheInjection::SkippedNoCapability,
        "an unsupported per-block wire must record a skip, never auto_emitted",
    );
    assert_eq!(front_marker(&req), None, "no front marker may be placed");
    // The terminal marker is unaffected -- it has its own capability gate.
    assert_eq!(out.terminal, CacheInjection::Emitted);
    assert_eq!(req.cache_control, Some(CacheControl::ephemeral_5m()));

    // Nothing but the top-level slot moved.
    let mut expected: ChatRequest =
        serde_json::from_slice(&before).expect("deserialize the pre-placement request");
    expected.cache_control = Some(CacheControl::ephemeral_5m());
    assert_eq!(
        serde_json::to_vec(&req).expect("serialize after"),
        serde_json::to_vec(&expected).expect("serialize expected"),
    );
}

#[test]
fn global_kill_switch_withholds_both_markers() {
    // The global `[cache]` switch is the master kill: an operator with
    // auto-emit off today must not start receiving a front marker.
    let mut req = blocks_system_req();
    let p = plan(0, false, false, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = apply_auto_cache_placement(&mut req, &p, gates(cap, true, true), false);

    assert_eq!(out.front, CacheInjection::SkippedGlobalDisabled);
    assert_eq!(out.terminal, CacheInjection::SkippedGlobalDisabled);
    assert_eq!(front_marker(&req), None);
    assert_eq!(req.cache_control, None);
}

#[test]
fn injected_front_marker_is_never_at_the_messages_position() {
    // D1's ordering contract at the placement seam: the marker this
    // helper writes always sits before the messages, so the reducer's
    // mutable-suffix domain is unchanged. Pinned for BOTH anchor shapes.
    for mut req in [blocks_system_req(), tools_only_req()] {
        let p = plan(0, false, true, &req);
        let cap = Some(CacheCapability::new(true, true));
        let out = apply_auto_cache_placement(&mut req, &p, gates(cap, false, true), false);
        assert_eq!(out.front, CacheInjection::Emitted);

        let positions: Vec<_> = compute_frozen_floor(&req).positions().to_vec();
        assert!(
            !positions.contains(&routectl_core::BreakpointPosition::Messages),
            "an auto-emitted marker must never occupy the Messages position",
        );
    }
}

#[tokio::test]
async fn completion_and_streaming_decide_identically() {
    // Acceptance: ONE shared helper backs both dispatch surfaces, so
    // equal inputs produce equal decisions AND byte-identical bytes.
    let (complete_router, complete_captured) = rig(anthropic_entry(), true, 0);
    let complete = complete_router
        .complete_with_options(blocks_system_req(), RouterOptions::new())
        .await;
    complete.result.expect("ok");

    let (stream_router, stream_captured) = rig(anthropic_entry(), true, 0);
    let streamed = stream_router
        .stream_with_options(blocks_system_req(), RouterOptions::new())
        .await;
    let _ = streamed.result.expect("ok").collect::<Vec<_>>().await;

    assert_eq!(
        complete.meta.cache_front_decision,
        streamed.meta.cache_front_decision,
    );
    assert_eq!(
        complete.meta.cache_terminal_decision,
        streamed.meta.cache_terminal_decision,
    );
    assert_eq!(complete.meta.cache_front_decision, Some("auto_emitted"));

    let a = serde_json::to_vec(complete_captured.lock().first().expect("complete dispatch"))
        .expect("serialize completion");
    let b = serde_json::to_vec(stream_captured.lock().first().expect("stream dispatch"))
        .expect("serialize stream");
    assert_eq!(
        a, b,
        "the two dispatch surfaces must send byte-identical bytes for equal inputs",
    );
}

#[tokio::test]
async fn front_and_terminal_retries_are_byte_identical() {
    // Acceptance: same-target retries send byte-identical bytes with the
    // front marker in play (the plan is immutable and request-derived).
    let (router, captured) = rig(anthropic_entry(), true, 1);
    router
        .complete(blocks_system_req())
        .await
        .expect("ok after retry");
    let captured = captured.lock();
    assert_eq!(captured.len(), 2, "expected one failed + one ok attempt");
    let a = serde_json::to_vec(&captured[0]).expect("serialize attempt 0");
    let b = serde_json::to_vec(&captured[1]).expect("serialize attempt 1");
    assert_eq!(a, b, "retried attempt bodies must be byte-identical");
    assert_eq!(
        front_marker(&captured[0]),
        Some(CacheControl::ephemeral_5m())
    );
}

#[tokio::test]
async fn caller_marked_request_with_a_front_anchor_is_byte_identical() {
    // Acceptance (pinned): a caller-marked request is byte-identical to
    // today even though it now offers a front anchor -- caller markers
    // always win, and neither marker is added.
    let (router, captured) = rig(anthropic_entry(), true, 0);
    let mut req = blocks_system_req();
    req.cache_control = Some(CacheControl::ephemeral_1h());
    let mut before = req.clone();
    before.model = "upstream-model".into();
    let before_bytes = serde_json::to_vec(&before).expect("serialize before");

    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        serde_json::to_vec(up).expect("serialize after"),
        before_bytes,
        "a caller-marked request must dispatch byte-identical (modulo upstream model id)",
    );
    assert_eq!(
        dispatched.meta.cache_front_decision,
        Some("caller_supplied")
    );
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("caller_supplied"),
    );
}

#[tokio::test]
async fn caller_marked_credential_bound_request_is_byte_identical() {
    // Acceptance (pinned): a caller-marked request on a CREDENTIAL-BOUND
    // target -- an `oauth://` subscription seat on the default Anthropic
    // base, the population whose per-block default is ON -- is
    // byte-identical to today. This is the Claude Code shape: the client
    // supplies its own per-block markers, so `caller_supplied` withholds
    // BOTH auto markers and the subscription-bound bytes are never
    // rewritten.
    let (router, captured) = rig(ProviderEntry::anthropic_api("oauth://anthropic"), true, 0);
    let mut req = blocks_system_req();
    let Some(SystemContent::Blocks(blocks)) = req.system.as_mut() else {
        panic!("system stays in Blocks form");
    };
    // The caller marks the FIRST block -- deliberately not the block the
    // front anchor would pick, so a stray auto marker would be visible.
    blocks[0].cache_control = Some(CacheControl::ephemeral_1h());

    let mut before = req.clone();
    before.model = "upstream-model".into();
    let before_bytes = serde_json::to_vec(&before).expect("serialize before");

    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        serde_json::to_vec(up).expect("serialize after"),
        before_bytes,
        "a credential-bound caller-marked request must dispatch byte-identical \
         (modulo upstream model id)",
    );
    assert_eq!(
        dispatched.meta.cache_front_decision,
        Some("caller_supplied")
    );
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("caller_supplied"),
    );
}

/// Parse a single `[providers.p]` entry out of TOML. Used for the entries
/// with no builder, so an operator-authored opt-in is exercised through
/// the REAL deserializer rather than a hand-built struct literal.
fn entry_from_toml(toml_text: &str) -> ProviderEntry {
    let cfg: Config = toml::from_str(toml_text).expect("parse provider entry");
    cfg.providers.get("p").expect("provider p").clone()
}

#[tokio::test]
async fn opted_in_openai_compat_gets_no_front_marker_on_the_wire() {
    // Acceptance (fail-closed): the openai-compat egress DROPS a per-block
    // marker with a WARN and, under `strict_translation`, rejects the whole
    // request. An explicit opt-in must therefore be inert -- no front
    // marker, and the recorded decision must not claim `auto_emitted`.
    let entry = entry_from_toml(
        r#"
[providers.p]
kind = "openai-compat"
base_url = "https://example.invalid/v1"
api_key_ref = "literal:k"
auto_emit_per_block_breakpoints = true
"#,
    );
    assert!(
        entry.per_block_breakpoints_enabled(),
        "the operator opted in, so the switch reads true",
    );

    let (router, captured) = rig(entry, true, 0);
    // openai-compat has no top-level capability either, so the dispatched
    // request must be byte-identical to the input.
    let mut before = blocks_system_req();
    before.model = "upstream-model".into();
    let before_bytes = serde_json::to_vec(&before).expect("serialize before");

    let dispatched = router
        .complete_with_options(blocks_system_req(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(front_marker(up), None, "no front marker may reach the wire");
    assert_eq!(
        serde_json::to_vec(up).expect("serialize after"),
        before_bytes,
        "an opted-in openai-compat request must be byte-identical to today",
    );
    assert_eq!(
        dispatched.meta.cache_front_decision,
        Some("auto_skipped:no_capability"),
        "the ledger must record the skip, never a false auto_emitted",
    );
}

#[test]
fn front_marker_never_ships_to_an_opted_in_openai_compat_egress() {
    // The consequences the skip prevents, pinned at the EGRESS itself so
    // the fail-closed rule stays tied to real wire behavior rather than to
    // our model of it: in default mode the marker is DROPPED (silently
    // wasted), and under `strict_translation` the request 400s outright.
    use routectl_providers::openai_compat::request::normalize;
    use routectl_providers::openai_compat::{HistoryReasoning, ReasoningDialect};

    let mut marked = blocks_system_req();
    let Some(SystemContent::Blocks(blocks)) = marked.system.as_mut() else {
        panic!("system stays in Blocks form");
    };
    blocks[1].cache_control = Some(CacheControl::ephemeral_5m());

    // Default mode: accepted, but the marker does not survive.
    let body = normalize(
        "p",
        &marked,
        ReasoningDialect::default(),
        HistoryReasoning::default(),
        None,
        false,
    )
    .expect("default mode drops the marker rather than failing");
    let rendered = serde_json::to_string(&body).expect("serialize body");
    assert!(
        !rendered.contains("cache_control"),
        "the openai-compat wire carries no per-block marker: {rendered}",
    );

    // Strict mode: the same marker is a hard 400.
    let strict = normalize(
        "p",
        &marked,
        ReasoningDialect::default(),
        HistoryReasoning::default(),
        None,
        true,
    );
    assert!(
        strict.is_err(),
        "under strict_translation a per-block marker rejects the whole request",
    );
}

#[tokio::test]
async fn fallback_target_starts_from_the_canonical_request_after_a_no_capability_skip() {
    // Acceptance: a hop through an ineligible target never contaminates the
    // next. Target 1 is an openai-compat entry (no top-level capability,
    // per-block default off) that always fails; target 2 is the capable
    // anthropic entry that serves. Target 2's bytes must equal a
    // freshly-marked copy of the ORIGINAL request.
    let cap_a: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let cap_b: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));

    let mut config = Config {
        retry: RetryPolicy {
            initial_backoff_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config.providers.insert(
        "p-compat".into(),
        ProviderEntry::openai_compat("https://example.invalid/v1", "literal:k"),
    );
    config.providers.insert(
        "p-anthropic".into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(vec!["m-compat".into(), "m-anthropic".into()]),
    );

    let prov_a: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "p-compat".into(),
        captured: cap_a.clone(),
        fail_first: usize::MAX,
        seen: AtomicUsize::new(0),
    });
    let prov_b: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "p-anthropic".into(),
        captured: cap_b.clone(),
        fail_first: 0,
        seen: AtomicUsize::new(0),
    });

    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m-compat".into(),
        Arc::new(ResolvedModel::new(
            "m-compat",
            "p-compat",
            prov_a,
            "upstream-compat",
        )),
    );
    models.insert(
        "m-anthropic".into(),
        Arc::new(ResolvedModel::new(
            "m-anthropic",
            "p-anthropic",
            prov_b,
            "upstream-anthropic",
        )),
    );

    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);

    let mut req = blocks_system_req();
    req.model = "alias".into();
    router.complete(req).await.expect("falls back and serves");

    // Target 1: neither marker (no capability, per-block default off).
    let a = cap_a.lock();
    let up_a = a.first().expect("target 1 dispatched");
    assert_eq!(front_marker(up_a), None);
    assert_eq!(up_a.cache_control, None);

    // Target 2: both markers, derived only from the original request.
    let mut expected = blocks_system_req();
    expected.model = "upstream-anthropic".into();
    let Some(SystemContent::Blocks(blocks)) = expected.system.as_mut() else {
        panic!("system stays in Blocks form");
    };
    blocks[1].cache_control = Some(CacheControl::ephemeral_5m());
    expected.cache_control = Some(CacheControl::ephemeral_5m());

    let b = cap_b.lock();
    let up_b = b.first().expect("target 2 dispatched");
    assert_eq!(
        serde_json::to_vec(up_b).expect("serialize target 2"),
        serde_json::to_vec(&expected).expect("serialize expected"),
        "the fallback target's bytes must derive only from the canonical request",
    );
}

#[test]
fn a_rolled_back_target_leaves_the_next_target_a_pristine_canonical_request() {
    // Acceptance (rollback COMBINED with fallback): target 1 places a marker
    // candidate that FAILS source validation, so its whole candidate is
    // discarded; the fallback target then re-derives from the canonical request
    // and emits normally. The sibling tests cover rollback alone and fallback
    // alone; this one pins that a discarded candidate leaves no residue for the
    // hop that follows.
    //
    // Driven through `apply_auto_cache_placement` per target rather than through
    // `complete`, because the production request-level gate withholds BOTH
    // markers on any request already carrying caller breakpoints
    // (`SkippedCallerSupplied`), so a request whose injected markers bust the
    // cap is unreachable from the dispatch path. Everything else mirrors the
    // chain loop exactly: ONE plan built above the loop and shared by both
    // targets, a fresh `canonical.clone()` per target, and per-target gates --
    // which is precisely what makes target 1 inject two markers (over the cap)
    // and target 2 inject one (within it).
    let mut canonical = blocks_system_req();
    let marked_part = |t: &str| {
        ContentPart::Known(KnownContentPart::Text {
            text: t.into(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_5m()),
        })
    };
    // Three caller markers: the front + terminal pair takes the validated
    // sequence to five, one past MAX_BREAKPOINTS -- the cheapest
    // `validate_source` invariant to break on a candidate. The terminal marker
    // alone lands exactly at the cap.
    canonical.messages = vec![Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![marked_part("a"), marked_part("b"), marked_part("c")]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }]
    .into();
    assert_eq!(
        compute_frozen_floor(&canonical).caller_breakpoint_count(),
        MAX_BREAKPOINTS - 1,
    );
    let canonical_bytes = serde_json::to_vec(&canonical).expect("serialize canonical");

    // One plan for the whole chain walk, as the dispatch loop builds it, forced
    // to claim a clean prefix so placement reaches the validate step.
    let shared_plan = plan(0, false, true, &canonical);
    let cap = Some(CacheCapability::new(true, true));

    // Target 1: both regions enabled -> two injections -> over the cap ->
    // the whole candidate is discarded.
    let mut attempt_one = canonical.clone();
    let first = apply_auto_cache_placement(
        &mut attempt_one,
        &shared_plan,
        gates(cap, true, true),
        false,
    );

    assert_eq!(first.front, CacheInjection::ValidationRolledBack);
    assert_eq!(first.terminal, CacheInjection::ValidationRolledBack);
    assert_eq!(
        serde_json::to_vec(&attempt_one).expect("serialize target 1"),
        canonical_bytes,
        "a rolled-back target must dispatch the canonical bytes unchanged",
    );

    // Target 2: the fallback hop. Front region disabled for this target, so one
    // injection lands exactly at the cap and validates -- but only if this
    // attempt started from the canonical request rather than from target 1's
    // discarded candidate.
    let mut attempt_two = canonical.clone();
    let second = apply_auto_cache_placement(
        &mut attempt_two,
        &shared_plan,
        gates(cap, true, false),
        false,
    );

    assert_eq!(second.front, CacheInjection::SkippedProviderDisabled);
    assert_eq!(
        second.terminal,
        CacheInjection::Emitted,
        "the fallback target must see a pristine prefix, not the rolled-back candidate",
    );
    let mut expected = canonical.clone();
    expected.cache_control = Some(CacheControl::ephemeral_5m());
    assert_eq!(
        serde_json::to_vec(&attempt_two).expect("serialize target 2"),
        serde_json::to_vec(&expected).expect("serialize expected"),
        "target 2's bytes are the canonical request plus its own terminal marker",
    );
    assert_eq!(front_marker(&attempt_two), None);
    assert_eq!(
        serde_json::to_vec(&canonical).expect("serialize canonical after"),
        canonical_bytes,
        "neither target may mutate the original request",
    );

    // Negative control, so the assertion above is attributable to the clean
    // start rather than to the cap arithmetic being lenient: had target 2
    // inherited a residual front marker from target 1's discarded candidate,
    // its own terminal injection would bust the cap and roll back too.
    let mut contaminated = canonical.clone();
    assert!(
        place_front_marker(
            &mut contaminated,
            shared_plan.front_slot.expect("blocks system offers a slot")
        ),
        "the fixture must offer a front slot for the control to be meaningful",
    );
    let contaminated_out = apply_auto_cache_placement(
        &mut contaminated,
        &shared_plan,
        gates(cap, true, false),
        false,
    );
    assert_eq!(
        contaminated_out.terminal,
        CacheInjection::ValidationRolledBack,
        "a residue-carrying start rolls back -- which is what target 2 must NOT do",
    );
}

#[cfg(feature = "bedrock")]
mod converse_front_only {
    use super::*;
    use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};

    /// A Bedrock entry at `api_shape`, with the per-block knob set to
    /// `per_block`.
    fn bedrock_entry(api_shape: BedrockApiShapeConfig, per_block: Option<bool>) -> ProviderEntry {
        ProviderEntry::Bedrock {
            region: "us-east-1".into(),
            api_shape,
            creds: BedrockCredsConfig::DefaultChain,
            user_agent: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: Vec::new(),
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: per_block,
            reduction_enabled: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    #[tokio::test]
    async fn opted_in_converse_gets_the_front_marker_only() {
        // Acceptance: an opted-in no-marker Converse target receives the
        // per-block front marker (which the existing egress translation
        // turns into a `cachePoint`) and NO top-level marker.
        let (router, captured) = rig(
            bedrock_entry(BedrockApiShapeConfig::Converse, Some(true)),
            true,
            0,
        );
        let dispatched = router
            .complete_with_options(blocks_system_req(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");

        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            front_marker(up),
            Some(CacheControl::ephemeral_5m()),
            "an opted-in Converse target must get the front marker",
        );
        assert_eq!(
            up.cache_control, None,
            "Converse gets no terminal marker (a top-level field is inert there)",
        );
        assert_eq!(dispatched.meta.cache_front_decision, Some("auto_emitted"));
        assert_eq!(
            dispatched.meta.cache_terminal_decision,
            Some("auto_skipped:no_capability"),
        );
    }

    /// The canonical front marker reaches the Converse wire as a
    /// `cachePoint` block through the EXISTING per-block translation --
    /// zero routectl-providers changes.
    #[tokio::test]
    async fn converse_front_marker_becomes_a_cache_point_on_the_wire() {
        let cfg = routectl_providers::bedrock::BedrockConfig {
            id: "b".into(),
            region: "us-east-1".into(),
            model_id: "anthropic.claude-sonnet-4".into(),
            api_shape: routectl_providers::bedrock::BedrockApiShape::Converse,
            creds: routectl_providers::bedrock::BedrockCreds::DefaultChain,
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            allowed_betas: Vec::new(),
            allowed_body_fields: Vec::new(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
        };
        let cache_points = |req: &ChatRequest| {
            let body = routectl_providers::bedrock::converse::normalize_request(&cfg, req)
                .expect("converse body");
            body.get("system")
                .and_then(serde_json::Value::as_array)
                .expect("converse system array")
                .iter()
                .filter(|b| b.get("cachePoint").is_some())
                .count()
        };
        // Negative control: the un-marked canonical request produces NO
        // cachePoint, so the assertion below is attributable to placement.
        assert_eq!(cache_points(&blocks_system_req()), 0);

        let (router, captured) = rig(
            bedrock_entry(BedrockApiShapeConfig::Converse, Some(true)),
            true,
            0,
        );
        router.complete(blocks_system_req()).await.expect("ok");

        let up = captured.lock().first().cloned().expect("one dispatch");
        assert_eq!(
            cache_points(&up),
            1,
            "the front marker must reach the Converse wire as exactly one cachePoint block",
        );
    }

    #[tokio::test]
    async fn non_opted_in_converse_is_unchanged() {
        // Converse stays opt-in: with the knob absent, neither marker is
        // emitted and the request is byte-identical to the input.
        let (router, captured) = rig(
            bedrock_entry(BedrockApiShapeConfig::Converse, None),
            true,
            0,
        );
        let mut before = blocks_system_req();
        before.model = "upstream-model".into();
        let before_bytes = serde_json::to_vec(&before).expect("serialize before");

        let dispatched = router
            .complete_with_options(blocks_system_req(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");

        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(
            serde_json::to_vec(up).expect("serialize after"),
            before_bytes,
            "a non-opted-in Converse target must be byte-identical to today",
        );
        assert_eq!(
            dispatched.meta.cache_front_decision,
            Some("auto_skipped:provider_disabled"),
        );
    }

    /// Bedrock Invoke stands down: the per-block knob defaults off there
    /// and is inert, so the dispatched bytes keep exactly today's shape
    /// (the terminal marker only, which the Invoke egress lowers itself).
    #[tokio::test]
    async fn invoke_is_byte_identical_across_the_inline_corpus() {
        for req in [
            base_req(),
            blocks_system_req(),
            tools_only_req(),
            long_tool_request(),
        ] {
            let (router, captured) =
                rig(bedrock_entry(BedrockApiShapeConfig::Invoke, None), true, 0);
            let mut expected = req.clone();
            expected.model = "upstream-model".into();
            // Invoke DOES support the top-level marker (the egress lowers
            // it), so today's terminal marker still lands -- what must not
            // change is the absence of any per-block front marker.
            expected.cache_control = Some(CacheControl::ephemeral_5m());
            let expected_bytes = serde_json::to_vec(&expected).expect("serialize expected");

            let dispatched = router
                .complete_with_options(req, RouterOptions::new())
                .await;
            dispatched.result.expect("ok");
            let captured = captured.lock();
            let up = captured.first().expect("one dispatch");
            assert_eq!(
                serde_json::to_vec(up).expect("serialize after"),
                expected_bytes,
                "Bedrock Invoke must stay byte-identical (no front marker)",
            );
            assert_eq!(
                dispatched.meta.cache_front_decision,
                Some("auto_skipped:provider_disabled"),
            );
        }
    }

    /// Invoke with an EXPLICIT opt-in: the documented-inert knob must stay
    /// inert at the placement seam too. Invoke has no front-marker path
    /// (its egress lowers the TOP-LEVEL marker to per-block itself), so the
    /// bytes match the default-off Invoke case exactly and the front
    /// decision records the wire skip rather than `auto_emitted`.
    #[tokio::test]
    async fn explicit_opt_in_on_invoke_stays_inert() {
        let (router, captured) = rig(
            bedrock_entry(BedrockApiShapeConfig::Invoke, Some(true)),
            true,
            0,
        );
        let mut expected = blocks_system_req();
        expected.model = "upstream-model".into();
        // Today's Invoke shape: the terminal marker only.
        expected.cache_control = Some(CacheControl::ephemeral_5m());
        let expected_bytes = serde_json::to_vec(&expected).expect("serialize expected");

        let dispatched = router
            .complete_with_options(blocks_system_req(), RouterOptions::new())
            .await;
        dispatched.result.expect("ok");

        let captured = captured.lock();
        let up = captured.first().expect("one dispatch");
        assert_eq!(front_marker(up), None, "Invoke must get no front marker");
        assert_eq!(
            serde_json::to_vec(up).expect("serialize after"),
            expected_bytes,
            "an explicit opt-in must not change the Invoke wire shape",
        );
        assert_eq!(
            dispatched.meta.cache_front_decision,
            Some("auto_skipped:no_capability"),
            "an opted-in-but-unsupported wire records the skip, not auto_emitted",
        );
    }
}

// -----------------------------------------------------------------
// Overlay end-to-end: a REAL on-disk overlay round-trip, merged via
// the REAL `factory::apply_catalog_overlay` (not a hand-built
// `EffectiveRow`), dispatched through the REAL Router -- proving the
// seam this module's `record_would_trim` reads (`ResolvedModel::
// effective_row`) actually changes behavior when a loader-shaped
// overlay is present, not just that the pure `merge` unit resolves
// correctly in isolation.
// -----------------------------------------------------------------

/// Save `cells` to a tempfile then load it straight back, so the
/// returned `CatalogOverlay` came from a REAL disk round-trip (the
/// same `catalog_overlay::save` / `load` pair the config loader and
/// the (future) migrator use), not an in-memory struct literal.
fn overlay_from_disk(
    cells: BTreeMap<String, Option<crate::catalog_overlay::OverlayCell>>,
) -> crate::catalog_overlay::CatalogOverlay {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("catalog_overlay.json");
    crate::catalog_overlay::save(&path, 0, cells).expect("save");
    crate::catalog_overlay::load(&path).expect("load")
}

/// Build a router exactly like `rig`, except the resolved model's
/// `effective_row` is stamped by the REAL `factory::apply_catalog_overlay`
/// post-pass (the same call `build_router_from_config_with_overlay`
/// makes) instead of a hand-rolled merge -- so `overlay` must carry
/// through `[models.m]` / `[providers.p]` resolution exactly as
/// production config would.
fn rig_with_overlay(
    entry: ProviderEntry,
    overlay: crate::catalog_overlay::CatalogOverlay,
) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: true,
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
    config.models.insert(
        "m".into(),
        crate::config::ModelEntry::new("p", "upstream-model"),
    );
    let config = Arc::new(config);

    let mut router = Router::new(config.clone());
    let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "cap".into(),
        captured: captured.clone(),
        fail_first: 0,
        seen: AtomicUsize::new(0),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m".into(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream-model")),
    );
    let models = crate::factory::apply_catalog_overlay(models, &config, &overlay);
    router.install_resolved_models(models);
    (router, captured)
}

#[tokio::test]
async fn overlay_override_through_real_load_path_changes_would_trim_pricing() {
    // Arrange: baseline (no overlay) vs. an overlay cell overriding the
    // anthropic-api catch-all's `wm`, round-tripped through disk.
    let (baseline_router, _c) = rig(anthropic_entry(), true, 0);
    let baseline = baseline_router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    baseline.result.expect("ok");
    let baseline_k = baseline
        .meta
        .would_trim_break_even_k
        .expect("baseline (baked catch-all) must price");

    let mut cells = BTreeMap::new();
    cells.insert(
        "anthropic-api:*".to_string(),
        Some(crate::catalog_overlay::OverlayCell {
            source: crate::catalog_overlay::OverlaySource::User,
            verified_at: "2026-07-01".to_string(),
            wm: Some(9.5),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            max_output_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );
    let overlay = overlay_from_disk(cells);

    // Act: dispatch the IDENTICAL request through the overlay-stamped
    // router.
    let (router, _captured) = rig_with_overlay(anthropic_entry(), overlay);
    let dispatched = router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");
    let overridden_k = dispatched
        .meta
        .would_trim_break_even_k
        .expect("overlay-priced target must still price");

    // Assert: the overlay's wm actually moved the priced outcome --
    // this fails if `record_would_trim` ever falls back to the baked
    // row instead of reading `ResolvedModel::effective_row`.
    assert_ne!(
        baseline_k, overridden_k,
        "an overlay cell overriding a baked field must change the priced break-even K* \
             through the real load -> merge -> dispatch path",
    );
}

#[tokio::test]
async fn overlay_null_disable_through_real_load_path_folds_to_conservative_sentinel() {
    // Arrange: a null overlay cell (JSON `null`, round-tripped through
    // disk) for the same selector the baseline test prices normally.
    let mut cells = BTreeMap::new();
    cells.insert("anthropic-api:*".to_string(), None);
    let overlay = overlay_from_disk(cells);

    // Act
    let (router, _captured) = rig_with_overlay(anthropic_entry(), overlay);
    let dispatched = router
        .complete_with_options(long_tool_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    // Assert: disabled folds to the SAME conservative sentinel as a
    // catalog miss -- no break-even K -- while the freed-token count
    // (independent of pricing trust) still records.
    assert_eq!(
        dispatched.meta.would_trim_break_even_k, None,
        "a null-disabled overlay cell must fold to the conservative sentinel \
             through the real load -> merge -> dispatch path",
    );
    assert!(
        dispatched.meta.would_trim_tokens.is_some(),
        "the freed-token count records regardless of pricing trust",
    );
}
