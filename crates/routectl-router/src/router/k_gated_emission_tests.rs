//! K-gated emission: the per-chain-entry consult that withholds BOTH
//! auto-cache markers when a session's CALIBRATED per-turn reuse floor sits
//! below the target row's emission break-even `K*`.
//!
//! Every assertion here pins one of the gate's acceptance properties, and
//! every branch of the gate fails toward EMIT -- so most of these tests are
//! "this situation must NOT suppress".
//!
//! The pricing row is the baked `anthropic-api` cell for `upstream-model`
//! (`wm = 1.25`, `rm = 0.1`, `auto_cacher = false`), whose emission
//! break-even is `(1.25 - 1) / (1 - 0.1) = 0.278`. Tests derive `K*` from
//! [`emission_break_even_k`] rather than hardcoding it, so a catalog
//! re-price cannot silently invalidate the comparison they assert.

use super::*;

use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::config::{CacheConfig, ProviderEntry};
use crate::k_estimator::Confidence;
use crate::resolved::ResolvedModel;
use crate::router::chain::into_one_dispatch_target;
use async_trait::async_trait;
use parking_lot::Mutex as ParkingMutex;
use routectl_core::{
    CacheControl, ChatChunk, ChatResponse, Choice, Message, MessageContent, Provider, Role,
    SystemBlock, SystemContent,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const SESSION: &str = "sess-k-gate";
const PROVIDER_KIND: &str = "anthropic-api";
/// Served nickname -- the label the sample write and this consult both key
/// their model dimension on.
const NICKNAME: &str = "m";
/// Upstream wire id -- keys pricing, never the K triple.
const UPSTREAM: &str = "upstream-model";

/// Captures every dispatched request; fails the first `fail_first` attempts
/// with a retryable 503 so multi-attempt idempotence is observable.
struct CapturingProvider {
    id: String,
    captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
    fail_first: usize,
    seen: AtomicUsize,
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
        Ok(ChatResponse {
            id: "ok".into(),
            model,
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Text("ok".into()),
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

/// The baked effective row for `(anthropic-api, upstream-model)`, mirroring
/// `factory::apply_catalog_overlay` (empty overlay, no `[cache_pricing]`).
fn baked_row() -> EffectiveRow {
    let baked = crate::catalog::lookup_baked_with_overrides(
        PROVIDER_KIND,
        UPSTREAM,
        None,
        &BTreeMap::new(),
    );
    crate::catalog::merge(baked.as_ref(), None)
}

/// The emission break-even `K*` of the fixture's pricing row.
fn k_star() -> f64 {
    emission_break_even_k(
        baked_row()
            .priced()
            .expect("baked anthropic cell is priced"),
    )
    .expect("the fixture row carries a write premium")
}

/// Wrap `row` as a `Present` merge result, so a test can price against a
/// hand-built row (e.g. one flagged `auto_cacher`).
fn present(row: CatalogRow) -> EffectiveRow {
    EffectiveRow::Present {
        row,
        source: Source::Baked,
        verified_at: "fixture".to_string(),
    }
}

/// A router with one anthropic-api provider and one resolved model on the
/// given effective row. `k_gated` drives the new kill switch; `global_emit`
/// drives the global auto-emit master switch; `fail_first` drives
/// multi-attempt idempotence.
fn rig(
    k_gated: bool,
    global_emit: bool,
    effective_row: EffectiveRow,
    fail_first: usize,
) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: global_emit,
            normalize_tools: true,
            k_gated_emission: k_gated,
        },
        // Zero backoff keeps the multi-attempt test fast.
        retry: RetryPolicy {
            initial_backoff_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config
        .providers
        .insert("p".into(), ProviderEntry::anthropic_api("literal:k"));

    let mut router = Router::new(Arc::new(config));
    let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "cap".into(),
        captured: captured.clone(),
        fail_first,
        seen: AtomicUsize::new(0),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        NICKNAME.into(),
        Arc::new(
            ResolvedModel::new(NICKNAME, "p", provider, UPSTREAM).with_effective_row(effective_row),
        ),
    );
    router.install_resolved_models(models);
    (router, captured)
}

/// Record `n` samples under the served-model triple, `reuse` on each.
///
/// Stamps the run just behind the wall clock, one second apart, because the
/// estimator tallies only samples younger than the row TTL measured against
/// the querying dispatch's own `SystemTime::now()`. A fixed epoch anchor
/// would leave every sample outside the window and read back `Cold`.
fn record_samples(router: &Router, n: u64, reuse: bool) {
    let base = SystemTime::now();
    for i in 0..n {
        router.record_k_sample(
            Some(SESSION),
            PROVIDER_KIND,
            NICKNAME,
            u64::from(reuse),
            base - Duration::from_secs(n - i),
        );
    }
}

/// A dispatch-shaped request carrying a front placement region (a two-block
/// system) and `session` as the inbound session key.
fn req_with_session(session: Option<&str>) -> ChatRequest {
    let mut req = ChatRequest {
        model: NICKNAME.into(),
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
        system: Some(SystemContent::Blocks(vec![
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
        ])),
        ..Default::default()
    };
    req.routectl_internal.inbound_session_key = session.map(str::to_string);
    req
}

/// The front marker actually present on a dispatched request.
fn front_marker(req: &ChatRequest) -> Option<CacheControl> {
    let SystemContent::Blocks(blocks) = req.system.as_ref()? else {
        return None;
    };
    blocks.iter().rev().find_map(|b| b.cache_control.clone())
}

/// A target on the fixture row, as the chain expansion would produce it
/// (nickname set, provider kind stamped). Used by the tests that drive the
/// gate predicate directly rather than through a dispatch.
fn target_on(row: EffectiveRow) -> DispatchTarget {
    let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "cap".into(),
        captured: Arc::new(ParkingMutex::new(Vec::new())),
        fail_first: 0,
        seen: AtomicUsize::new(0),
    });
    let mut target = into_one_dispatch_target(Arc::new(
        ResolvedModel::new(NICKNAME, "p", provider, UPSTREAM).with_effective_row(row),
    ));
    target.provider_kind = Some(PROVIDER_KIND);
    target
}

/// The eligible plan: no caller breakpoints, global auto-emit on, a front
/// slot resolved off the fixture request.
fn eligible_plan(req: &ChatRequest) -> AutoCacheRequestPlan {
    AutoCacheRequestPlan::build(req, true)
}

// ---- 1. Calibrated low K suppresses BOTH markers and records the token ----

#[tokio::test]
async fn calibrated_low_k_session_withholds_both_markers_and_records_the_token() {
    let (router, captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let dispatched = router
        .complete_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await;
    dispatched
        .result
        .expect("dispatch succeeds -- suppression only withholds markers");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(front_marker(up), None, "the front marker must be withheld");
    assert_eq!(
        up.cache_control, None,
        "the terminal marker must be withheld",
    );
    assert_eq!(
        dispatched.meta.cache_front_decision,
        Some("auto_skipped:k_below_break_even"),
    );
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("auto_skipped:k_below_break_even"),
    );
}

#[tokio::test]
async fn streaming_dispatch_suppresses_at_the_same_seam() {
    // The gate is wired at BOTH dispatch sites; a streaming request under
    // the same evidence must reach the same verdict.
    let (router, captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let dispatched = router
        .stream_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await;
    let _stream = dispatched.result.expect("stream opens");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(front_marker(up), None);
    assert_eq!(up.cache_control, None);
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("auto_skipped:k_below_break_even"),
    );
}

// ---- 2. Calibrated high K emits the baseline markers byte-unchanged ----

#[tokio::test]
async fn calibrated_high_k_session_emits_bytes_identical_to_the_switch_off_run() {
    // The control run has the switch OFF, so its bytes ARE the baseline bytes.
    let (control, control_captured) = rig(false, true, baked_row(), 0);
    control
        .complete_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await
        .result
        .expect("ok");
    let expected = serde_json::to_vec(control_captured.lock().first().expect("control dispatched"))
        .expect("serialize control");

    let (router, captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, true);
    let dispatched = router
        .complete_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(
        serde_json::to_vec(up).expect("serialize gated run"),
        expected,
        "a high-K session must dispatch the baseline bytes verbatim",
    );
    assert_eq!(dispatched.meta.cache_front_decision, Some("auto_emitted"));
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("auto_emitted")
    );
}

// ---- 3. `Low` confidence emits (the thin-sample clamp must not suppress) ----

#[tokio::test]
async fn low_confidence_thin_sample_window_still_emits() {
    // Three all-miss samples sit below CALIBRATED_MIN_TRIALS, so the
    // estimator force-clamps k_floor to 0.0 -- numerically below K*, which a
    // bare compare would read as evidence of no reuse.
    let (router, captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 3, false);

    let estimate = router.k_estimator.estimate(
        &k_query_key(Some(SESSION), Some(PROVIDER_KIND), NICKNAME)
            .query(Duration::from_mins(5), SystemTime::now()),
    );
    assert_eq!(
        estimate.confidence,
        Confidence::Low,
        "the fixture must be thin-sampled for this test to mean anything",
    );
    assert!(
        estimate.k_floor < k_star(),
        "the clamped floor is numerically below K*, so only the confidence gate saves it",
    );

    let dispatched = router
        .complete_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(front_marker(up), Some(CacheControl::ephemeral_5m()));
    assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
    assert_eq!(dispatched.meta.cache_front_decision, Some("auto_emitted"));
}

// ---- 4. Cold, no session key, and no nickname all emit ----

#[tokio::test]
async fn cold_estimator_emits() {
    let (router, captured) = rig(true, true, baked_row(), 0);

    let dispatched = router
        .complete_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("auto_emitted")
    );
}

#[tokio::test]
async fn keyless_request_emits_even_with_a_low_window_recorded() {
    // The window exists under SESSION, but this request carries no session
    // key, so no triple identifies it and no evidence can apply.
    let (router, captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let dispatched = router
        .complete_with_options(req_with_session(None), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("auto_emitted")
    );
}

#[test]
fn a_target_with_no_nickname_never_suppresses() {
    // The sample write skips a nickname-less target entirely, so its window
    // is permanently cold; the gate must short-circuit rather than key the
    // read on some fallback label.
    let (router, _captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let req = req_with_session(Some(SESSION));
    let plan = eligible_plan(&req);
    let mut target = target_on(baked_row());
    target.nickname = None;

    assert!(!router.k_emission_suppressed(&plan, &target, Some(SESSION)));
}

// ---- 5. Switch off emits the baseline markers even with the estimator
// populated low ----

#[tokio::test]
async fn switch_off_emits_the_baseline_markers_with_a_calibrated_low_window_present() {
    let (router, captured) = rig(false, true, baked_row(), 0);
    record_samples(&router, 12, false);

    // Guard: the evidence that WOULD suppress is present and calibrated.
    let estimate = router.k_estimator.estimate(
        &k_query_key(Some(SESSION), Some(PROVIDER_KIND), NICKNAME)
            .query(Duration::from_mins(5), SystemTime::now()),
    );
    assert_eq!(estimate.confidence, Confidence::Calibrated);
    assert!(estimate.k_floor < k_star());

    let dispatched = router
        .complete_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    let captured = captured.lock();
    let up = captured.first().expect("one dispatch");
    assert_eq!(front_marker(up), Some(CacheControl::ephemeral_5m()));
    assert_eq!(up.cache_control, Some(CacheControl::ephemeral_5m()));
    assert_eq!(dispatched.meta.cache_front_decision, Some("auto_emitted"));
}

// ---- 6. Same-target retries are byte-identical under suppression ----

#[tokio::test]
async fn same_target_retries_are_byte_identical_under_suppression() {
    // One consult per chain entry, above the retry loop: the second attempt
    // re-sends the bytes the first prepared.
    let (router, captured) = rig(true, true, baked_row(), 1);
    record_samples(&router, 12, false);

    router
        .complete_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await
        .result
        .expect("the retry succeeds");

    let captured = captured.lock();
    assert_eq!(captured.len(), 2, "one failure plus one retry");
    let first = serde_json::to_vec(&captured[0]).expect("serialize attempt 1");
    let second = serde_json::to_vec(&captured[1]).expect("serialize attempt 2");
    assert_eq!(
        first, second,
        "retries of one target must be byte-identical"
    );
    assert_eq!(front_marker(&captured[1]), None);
    assert_eq!(captured[1].cache_control, None);
}

// ---- 7. Fallback to a different target re-evaluates from its own consult ----

#[tokio::test]
async fn a_fallback_target_re_evaluates_suppression_from_its_own_consult() {
    // Per-TARGET idempotence, pinned: target 1's triple carries an all-miss
    // calibrated window and is suppressed; target 2's triple is untouched, so
    // it emits. A request-level verdict would wrongly suppress both.
    const SUPPRESSED: &str = "m-suppressed";
    const LIVE: &str = "m-live";

    let mut config = Config {
        cache: CacheConfig {
            auto_emit_top_level_breakpoint: true,
            normalize_tools: true,
            k_gated_emission: true,
        },
        retry: RetryPolicy {
            initial_backoff_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config
        .providers
        .insert("p".into(), ProviderEntry::anthropic_api("literal:k"));
    config.aliases.insert(
        "alias".into(),
        crate::config::AliasValue::Chain(vec![SUPPRESSED.into(), LIVE.into()]),
    );

    let cap_first: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let cap_second: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let prov_first: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "first".into(),
        captured: cap_first.clone(),
        fail_first: usize::MAX,
        seen: AtomicUsize::new(0),
    });
    let prov_second: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "second".into(),
        captured: cap_second.clone(),
        fail_first: 0,
        seen: AtomicUsize::new(0),
    });

    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        SUPPRESSED.into(),
        Arc::new(
            ResolvedModel::new(SUPPRESSED, "p", prov_first, UPSTREAM)
                .with_effective_row(baked_row()),
        ),
    );
    models.insert(
        LIVE.into(),
        Arc::new(
            ResolvedModel::new(LIVE, "p", prov_second, UPSTREAM).with_effective_row(baked_row()),
        ),
    );

    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);

    // Only the FIRST target's triple carries the all-miss window.
    let base = SystemTime::now();
    for i in 0..12u64 {
        router.record_k_sample(
            Some(SESSION),
            PROVIDER_KIND,
            SUPPRESSED,
            0,
            base - Duration::from_secs(12 - i),
        );
    }

    let mut req = req_with_session(Some(SESSION));
    req.model = "alias".into();
    router.complete(req).await.expect("falls back and serves");

    let first = cap_first.lock();
    let up_first = first.first().expect("target 1 dispatched");
    assert_eq!(front_marker(up_first), None, "target 1 is suppressed");
    assert_eq!(up_first.cache_control, None);

    let second = cap_second.lock();
    let up_second = second.first().expect("target 2 dispatched");
    assert_eq!(
        front_marker(up_second),
        Some(CacheControl::ephemeral_5m()),
        "target 2 re-evaluates against its own triple and emits",
    );
    assert_eq!(up_second.cache_control, Some(CacheControl::ephemeral_5m()));
}

// ---- 8. An auto-cacher row never suppresses ----

#[test]
fn an_auto_cacher_row_never_suppresses_however_low_the_window() {
    // A provider that caches for free must never have a marker withheld on
    // economic grounds. Asserted against the ECONOMICS (the row's
    // `auto_cacher` flag) rather than against the baseline per-provider
    // defaults, which could change without changing the economics. The
    // `auto_cacher` rows in the shipped catalog carry `wm > 1.0`, so a
    // write-premium check alone would not cover them.
    let (router, _captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let req = req_with_session(Some(SESSION));
    let plan = eligible_plan(&req);

    let mut priced = baked_row().priced().expect("baked cell is priced").clone();
    assert!(
        priced.wm > 1.0,
        "the control row must carry a write premium",
    );

    // Control: the identical row, not an auto-cacher, DOES suppress.
    assert!(
        router.k_emission_suppressed(&plan, &target_on(present(priced.clone())), Some(SESSION)),
        "the control must suppress, or the auto-cacher assertion proves nothing",
    );

    priced.auto_cacher = true;
    assert!(
        !router.k_emission_suppressed(&plan, &target_on(present(priced)), Some(SESSION)),
        "an auto-cacher row must never suppress",
    );
}

#[test]
fn an_untrusted_merge_result_never_suppresses() {
    // `Disabled` / `Missing` carry no trusted multipliers, so there is no
    // honest K* to compare against -- the conservative answer is to emit.
    let (router, _captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let req = req_with_session(Some(SESSION));
    let plan = eligible_plan(&req);

    for row in [EffectiveRow::Disabled, EffectiveRow::Missing] {
        assert!(
            !router.k_emission_suppressed(&plan, &target_on(row), Some(SESSION)),
            "an untrusted pricing cell must never suppress",
        );
    }
}

// ---- 9. Suppression sits below caller_supplied and global_disabled ----

#[tokio::test]
async fn caller_supplied_outranks_suppression() {
    let (router, captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let mut req = req_with_session(Some(SESSION));
    req.cache_control = Some(CacheControl::ephemeral_1h());
    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    assert_eq!(
        dispatched.meta.cache_front_decision,
        Some("caller_supplied")
    );
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("caller_supplied"),
    );
    // The caller's own marker survives untouched.
    let captured = captured.lock();
    assert_eq!(
        captured.first().expect("one dispatch").cache_control,
        Some(CacheControl::ephemeral_1h()),
    );
}

#[tokio::test]
async fn global_disabled_outranks_suppression() {
    let (router, _captured) = rig(true, false, baked_row(), 0);
    record_samples(&router, 12, false);

    let dispatched = router
        .complete_with_options(req_with_session(Some(SESSION)), RouterOptions::new())
        .await;
    dispatched.result.expect("ok");

    assert_eq!(
        dispatched.meta.cache_front_decision,
        Some("auto_skipped:global_disabled"),
    );
    assert_eq!(
        dispatched.meta.cache_terminal_decision,
        Some("auto_skipped:global_disabled"),
    );
}

#[test]
fn placement_checks_suppression_only_after_the_shared_request_level_reasons() {
    // The precedence is pinned at the placement seam too, independent of the
    // gate's own short-circuits: even handed `k_suppressed = true`, a
    // caller-supplied or globally-disabled request records ITS token.
    let req = req_with_session(Some(SESSION));

    let mut caller = AutoCacheRequestPlan::build(&req, true);
    caller.has_caller_breakpoints = true;
    caller.caller_breakpoint_count = 1;
    let gates = CacheTargetGates {
        capability: Some(crate::config::CacheCapability::new(true, true)),
        terminal_enabled: true,
        front_supported: true,
        front_enabled: true,
    };

    let mut attempt = req.clone();
    let out = apply_auto_cache_placement(&mut attempt, &caller, gates, true);
    assert_eq!(out.front, CacheInjection::SkippedCallerSupplied);
    assert_eq!(out.terminal, CacheInjection::SkippedCallerSupplied);

    let global_off = AutoCacheRequestPlan::build(&req, false);
    let mut attempt = req.clone();
    let out = apply_auto_cache_placement(&mut attempt, &global_off, gates, true);
    assert_eq!(out.front, CacheInjection::SkippedGlobalDisabled);
    assert_eq!(out.terminal, CacheInjection::SkippedGlobalDisabled);

    // With both request-level reasons clear, the suppression token lands and
    // the request is dispatched un-injected.
    let eligible = AutoCacheRequestPlan::build(&req, true);
    let mut attempt = req.clone();
    let before = serde_json::to_vec(&attempt).expect("serialize before");
    let out = apply_auto_cache_placement(&mut attempt, &eligible, gates, true);
    assert_eq!(out.front, CacheInjection::SkippedKBelowBreakEven);
    assert_eq!(out.terminal, CacheInjection::SkippedKBelowBreakEven);
    assert_eq!(
        serde_json::to_vec(&attempt).expect("serialize after"),
        before,
        "a suppressed placement must leave the clone untouched",
    );
}

// ---- 10. The ineligible path short-circuits with zero allocations ----

#[test]
fn the_ineligible_path_allocates_nothing_before_any_key_is_built() {
    // Every short-circuit reads a field already in hand; the K triple is
    // built only once the target is eligible. The measurement is
    // thread-confined (see `crate::alloc_probe`) so a parallel test's
    // allocations cannot land in the tally.
    let (router, _captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let req = req_with_session(Some(SESSION));
    let target = target_on(baked_row());
    let eligible = eligible_plan(&req);

    let mut caller = AutoCacheRequestPlan::build(&req, true);
    caller.has_caller_breakpoints = true;
    let global_off = AutoCacheRequestPlan::build(&req, false);

    let mut no_nickname = target_on(baked_row());
    no_nickname.nickname = None;

    let (off_router, _) = rig(false, true, baked_row(), 0);

    for (label, router, plan, target, session) in [
        ("switch off", &off_router, &eligible, &target, Some(SESSION)),
        (
            "caller breakpoints",
            &router,
            &caller,
            &target,
            Some(SESSION),
        ),
        (
            "global disabled",
            &router,
            &global_off,
            &target,
            Some(SESSION),
        ),
        ("no session key", &router, &eligible, &target, None),
        (
            "no nickname",
            &router,
            &eligible,
            &no_nickname,
            Some(SESSION),
        ),
    ] {
        let (verdict, allocs) = crate::alloc_probe::count_allocs(|| {
            router.k_emission_suppressed(plan, target, session)
        });
        assert!(!verdict, "{label} must not suppress");
        assert_eq!(allocs, 0, "{label} must short-circuit without allocating");
    }

    // Negative control: the ELIGIBLE path does build a key, so the probe is
    // measuring something real rather than reporting a constant zero.
    let (verdict, allocs) = crate::alloc_probe::count_allocs(|| {
        router.k_emission_suppressed(&eligible, &target, Some(SESSION))
    });
    assert!(verdict, "the eligible fixture suppresses");
    assert!(
        allocs > 0,
        "the eligible path builds a key, so the probe must observe allocations",
    );
}

// ---- 11. One key derivation across all four sites ----

#[test]
fn the_consult_reads_the_triple_the_sample_write_wrote() {
    // The fourth site of the shared derivation. The window is written under
    // the served NICKNAME; a consult that keyed the model dimension on the
    // upstream wire id would read a permanently-cold triple and never
    // suppress.
    let (router, _captured) = rig(true, true, baked_row(), 0);
    record_samples(&router, 12, false);

    let derived = k_query_key(Some(SESSION), Some(PROVIDER_KIND), NICKNAME);
    assert!(
        router
            .k_session_store
            .get(&derived.store_key().expect("keyed request"))
            .is_some(),
        "the sample write landed under the derived triple",
    );
    assert!(
        router
            .k_session_store
            .get(&crate::k_estimator::KSessionKey {
                session_key: SESSION.into(),
                provider_kind: PROVIDER_KIND.into(),
                model: UPSTREAM.into(),
            })
            .is_none(),
        "nothing is written under the upstream wire id",
    );

    let req = req_with_session(Some(SESSION));
    assert!(
        router.k_emission_suppressed(&eligible_plan(&req), &target_on(baked_row()), Some(SESSION)),
        "the consult must read the nickname-keyed window the write created",
    );
}

#[test]
fn the_derived_triple_matches_the_ledger_rebuild_reference() {
    // The rebuild path builds its own `KSessionKey` from ledger columns
    // (`k_estimator::rebuild`), which is the reference the shared derivation
    // must agree with -- otherwise a warm-rebuilt store and a live store
    // disagree about the same session and the gate reads whichever the
    // process happens to hold.
    let derived = k_query_key(Some(SESSION), Some(PROVIDER_KIND), NICKNAME)
        .store_key()
        .expect("keyed request");
    let rebuild_reference = crate::k_estimator::KSessionKey {
        session_key: SESSION.into(),
        provider_kind: PROVIDER_KIND.into(),
        model: NICKNAME.into(),
    };

    assert_eq!(derived, rebuild_reference);
}

// ---- 12. Emergent stickiness: an all-miss Calibrated window stays below K* ----

#[test]
fn an_all_miss_calibrated_window_keeps_k_floor_below_the_break_even() {
    // The guardrail that replaces an explicit suppression latch: a suppressed
    // session writes no cache, so every subsequent sample observes no reuse,
    // which drives the Wilson lower bound (and with it `k_floor`) toward zero
    // and KEEPS the session suppressed. If a future estimator change lifted
    // an all-miss floor above K*, suppression would silently stop latching.
    let (router, _captured) = rig(true, true, baked_row(), 0);
    let k_star = k_star();

    for extra in [0u64, 8, 20] {
        record_samples(&router, 12 + extra, false);
        let estimate = router.k_estimator.estimate(
            &k_query_key(Some(SESSION), Some(PROVIDER_KIND), NICKNAME)
                .query(Duration::from_mins(5), SystemTime::now()),
        );
        assert_eq!(
            estimate.confidence,
            Confidence::Calibrated,
            "an all-miss window of this size must classify Calibrated",
        );
        assert!(
            estimate.k_floor < k_star,
            "all-miss k_floor {} must stay below K* {k_star}",
            estimate.k_floor,
        );
    }
}

// ---- 13. The token is in the vocabulary ----

#[test]
fn the_suppression_variant_maps_to_its_stable_token() {
    assert_eq!(
        CacheInjection::SkippedKBelowBreakEven.strategy_str(),
        "auto_skipped:k_below_break_even",
    );
}
