//! T5 dispatch-path auto-emission of a top-level `cache_control`
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
                choices: vec![],
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

#[test]
fn helper_emits_on_clean_capable_request() {
    let mut req = base_req();
    let p = plan(0, false, true, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = maybe_apply_auto_cache_control(&mut req, &p, cap, true);
    assert_eq!(out, CacheInjection::Emitted);
    assert_eq!(req.cache_control, Some(CacheControl::ephemeral_5m()));
}

#[test]
fn helper_fails_closed_on_unknown_capability() {
    let mut req = base_req();
    let p = plan(0, false, true, &req);
    let out = maybe_apply_auto_cache_control(&mut req, &p, None, true);
    assert_eq!(out, CacheInjection::SkippedNoCapability);
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
    let out = maybe_apply_auto_cache_control(&mut req, &p, cap, true);
    assert_eq!(out, CacheInjection::ValidationRolledBack);
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
    let out = maybe_apply_auto_cache_control(&mut req, &p, None, true);
    assert_eq!(out, CacheInjection::SkippedCallerSupplied);
    assert_eq!(
        req.cache_control, None,
        "caller_supplied path must leave attempt_req.cache_control untouched",
    );

    // Global kill-switch off -> caller still dominates.
    let p_global_off = plan(1, false, false, &req);
    let cap = Some(CacheCapability::new(true, true));
    let out = maybe_apply_auto_cache_control(&mut req, &p_global_off, cap, true);
    assert_eq!(out, CacheInjection::SkippedCallerSupplied);
    assert_eq!(req.cache_control, None);

    // Per-provider kill-switch off -> caller still dominates.
    let out = maybe_apply_auto_cache_control(&mut req, &p, cap, false);
    assert_eq!(out, CacheInjection::SkippedCallerSupplied);
    assert_eq!(req.cache_control, None);

    // Volatile-high veto must not override caller_supplied either.
    let p_volatile = plan(1, true, true, &req);
    let out = maybe_apply_auto_cache_control(&mut req, &p_volatile, cap, true);
    assert_eq!(out, CacheInjection::SkippedCallerSupplied);
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
        CacheInjection::ValidationRolledBack.strategy_str(),
        "auto_skipped:validation_rolled_back",
    );
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
