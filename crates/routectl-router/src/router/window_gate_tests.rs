//! Tests for the proactive context-window gate: the reserve-aware skip rule,
//! the three never-skip-the-last layers, the unknown-window keep, the kill
//! switch, and the bounded skip WARN.
//!
//! Every window in these tests is derived from the fixture request's own
//! estimate, so the assertions pin the rule rather than a byte count the
//! estimator could drift under.

use super::*;

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{ChatChunk, ChatResponse, Message, MessageContent, Provider, Result, Role};

use crate::config::{Config, ModelEntry};
use crate::resolved::ResolvedModel;
use crate::router::chain::into_one_dispatch_target;

/// Marker text carried in the fixture request body. No log line the gate
/// emits may contain it.
const REQUEST_BODY_MARKER: &str = "fixture-prompt-body-marker";

/// Minimal provider stub: these tests drive the filter seam directly and
/// never dispatch, so none of its methods run.
struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(routectl_core::Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("window gate tests never dispatch")
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("window gate tests never dispatch")
    }
}

fn router(gate_enabled: bool) -> Router {
    router_with(gate_enabled, true)
}

/// A router with both switches set explicitly. `[calibration] enabled` is
/// accepted and has no effect, which the calibration rows below pin.
fn router_with(gate_enabled: bool, calibration_enabled: bool) -> Router {
    let body = format!(
        "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://x\"\n\
         api_key_ref = \"literal:k\"\n\
         [window_gate]\n\
         enabled = {gate_enabled}\n\
         [calibration]\n\
         enabled = {calibration_enabled}\n"
    );
    let config: Config = toml::from_str(&body).expect("config parses");
    Router::new(Arc::new(config))
}

/// A request large enough that the derived windows below are far from the
/// estimator's own granularity.
fn oversized_request() -> ChatRequest {
    let filler = REQUEST_BODY_MARKER.repeat(2_000);
    ChatRequest {
        model: "alias".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(filler),
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

/// Estimated input tokens for `req`, the figure the gate compares.
fn estimated_tokens(req: &ChatRequest) -> u64 {
    crate::context_trim::estimate_total_tokens(req)
}

/// A window the estimate CLEARLY exceeds: half the estimate.
fn clearly_too_small(req: &ChatRequest) -> u32 {
    u32::try_from(estimated_tokens(req) / 2).expect("fixture estimate fits u32")
}

/// The smallest window that still holds the estimate plus the headroom,
/// with no output reserve (the fixture request sets no `max_tokens` and the
/// fixture targets carry no ceiling). A gate that skips this target has
/// lost its headroom rule.
fn just_inside_margin(req: &ChatRequest) -> u32 {
    let estimate = estimated_tokens(req);
    u32::try_from(estimate + HEADROOM_TOKENS).expect("fixture estimate fits u32")
}

/// A window no plausible margin makes too small: eight times the estimate.
/// Used as the surviving alternative wherever the property under test is
/// about the OTHER target.
fn comfortably_large(req: &ChatRequest) -> u32 {
    u32::try_from(estimated_tokens(req) * 8).expect("fixture estimate fits u32")
}

/// A dispatch target on provider `p` stamped from `entry` through the
/// factory's own per-model stamp. Each nickname sends its own name as its
/// upstream, so two fixture targets are two calibration lanes.
fn target(nickname: &str, entry: &ModelEntry) -> DispatchTarget {
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = crate::factory::apply_model_knobs(
        ResolvedModel::new(nickname, "p", provider, nickname),
        nickname,
        entry,
    );
    into_one_dispatch_target(Arc::new(model))
}

/// A target whose `[models.X] context_window` is `window` tokens.
fn target_with_window(nickname: &str, window: u32) -> DispatchTarget {
    target(
        nickname,
        &ModelEntry::new("p", nickname).with_context_window(window),
    )
}

/// A target whose `[models.X]` entry sets no window.
fn target_without_window(nickname: &str) -> DispatchTarget {
    target(nickname, &ModelEntry::new("p", nickname))
}

fn nicknames(chain: &[DispatchTarget]) -> Vec<&str> {
    chain
        .iter()
        .map(|t| t.nickname.as_deref().unwrap_or(""))
        .collect()
}

#[test]
fn clear_overflow_is_skipped_while_an_alternative_remains() {
    let router = router(true);
    let req = oversized_request();
    let chain = vec![
        target_with_window("small", clearly_too_small(&req)),
        target_with_window("large", comfortably_large(&req)),
    ];

    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(nicknames(&kept), vec!["large"]);
    assert_eq!(router.metrics.window_gate_skips_total(), 1);
}

#[test]
fn a_target_inside_the_safety_margin_is_kept() {
    // The safety property: the estimate is approximate, so a target the
    // request MIGHT still fit is always attempted. The sibling's window is
    // UNCONFIRMED rather than near-boundary, so it always survives -- a gate
    // that wrongly skips the near-boundary target has an alternative to fall
    // to and the never-skip-the-last layers cannot mask the aggression.
    let router = router(true);
    let req = oversized_request();
    let chain = vec![
        target_with_window("near", just_inside_margin(&req)),
        target_without_window("unconfirmed"),
    ];

    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(nicknames(&kept), vec!["near", "unconfirmed"]);
    assert_eq!(router.metrics.window_gate_skips_total(), 0);
}

#[test]
fn a_single_oversized_target_is_never_skipped() {
    let router = router(true);
    let req = oversized_request();
    let chain = vec![target_with_window("only", clearly_too_small(&req))];

    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(nicknames(&kept), vec!["only"]);
    assert_eq!(
        router.metrics.window_gate_skips_total(),
        0,
        "the last target is refused before any estimate is computed",
    );
}

#[test]
fn every_target_overflowing_returns_the_original_chain() {
    let router = router(true);
    let req = oversized_request();
    let chain = vec![
        target_with_window("small", clearly_too_small(&req)),
        target_with_window("smaller", clearly_too_small(&req) / 2),
    ];

    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(
        nicknames(&kept),
        vec!["small", "smaller"],
        "order preserved, no target dropped, no error invented",
    );
    assert_eq!(router.metrics.window_gate_skips_total(), 0);
}

/// A target whose entry sets no `context_window` is kept however large the
/// request, and one whose configured window is too small is skipped.
///
/// Mutation check: make `ResolvedModel::context_window_tokens` ignore the
/// configured window (return `None`) -> red (nothing is skipped).
#[test]
fn an_unconfigured_window_keeps_the_target_and_a_configured_small_one_is_skipped() {
    let router = router(true);
    let req = oversized_request();
    let chain = vec![
        target_without_window("unset"),
        target_with_window("small", clearly_too_small(&req)),
    ];

    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(
        nicknames(&kept),
        vec!["unset"],
        "only the configured too-small window is skipped",
    );
    assert_eq!(router.metrics.window_gate_skips_total(), 1);
}

#[test]
fn a_config_without_a_window_gate_block_keeps_every_target() {
    // Arrange: no [window_gate] table, so the shipped default applies. The
    // same chain under `router(true)` skips "small" (see
    // clear_overflow_is_skipped_while_an_alternative_remains).
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://x\"\n\
         api_key_ref = \"literal:k\"\n",
    )
    .expect("config parses");
    let router = Router::new(Arc::new(config));
    let req = oversized_request();
    let chain = vec![
        target_with_window("small", clearly_too_small(&req)),
        target_with_window("large", comfortably_large(&req)),
    ];

    // Act
    let kept = router.filter_chain_by_window(chain, &req);

    // Assert
    assert_eq!(nicknames(&kept), vec!["small", "large"]);
    assert_eq!(router.metrics.window_gate_skips_total(), 0);
}

#[test]
fn the_kill_switch_leaves_the_chain_untouched() {
    let router = router(false);
    let req = oversized_request();
    let chain = vec![
        target_with_window("small", clearly_too_small(&req)),
        target_with_window("large", comfortably_large(&req)),
    ];

    let events = routectl_testkit::capture_events(|| {
        let kept = router.filter_chain_by_window(chain, &req);
        assert_eq!(nicknames(&kept), vec!["small", "large"]);
    });

    assert_eq!(router.metrics.window_gate_skips_total(), 0);
    assert!(
        events.is_empty(),
        "a disabled gate emits nothing; got {events:?}",
    );
}

#[test]
fn repeated_oversized_requests_warn_once_while_the_counter_tracks_every_skip() {
    // A private throttle, so this bound is asserted against a stamp no
    // sibling test has already claimed.
    let throttle = SkipWarnThrottle::new();
    let router = router(true);
    let req = oversized_request();
    let requests = 5;

    let events = routectl_testkit::capture_events(|| {
        for _ in 0..requests {
            let chain = vec![
                target_with_window("small", clearly_too_small(&req)),
                target_with_window("large", comfortably_large(&req)),
            ];
            let kept = router.filter_chain_by_window_with(chain, &req, &throttle);
            assert_eq!(nicknames(&kept), vec!["large"]);
        }
    });

    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("window_gate_skip"))
        .collect();
    assert_eq!(
        warns.len(),
        1,
        "the WARN is bounded per interval per process across requests; got {warns:?}",
    );
    assert_eq!(router.metrics.window_gate_skips_total(), requests);
    let warn = warns[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("state_key"), Some("small"));
    assert_eq!(
        warn.field("window_tokens").map(str::to_string),
        Some(clearly_too_small(&req).to_string()),
    );
    assert_eq!(
        warn.field("skips_total").map(str::to_string),
        Some(1.to_string()),
        "the WARN reports the total at the moment it fired",
    );
    let rendered = format!("{} {:?}", warn.message, warn.fields);
    assert!(
        !rendered.contains(REQUEST_BODY_MARKER),
        "no request content may reach the log line; got {rendered}",
    );
}

#[test]
fn the_skip_rule_counts_estimate_reserve_and_headroom_against_the_window() {
    let rows: [(&str, u64, u64, u64, bool); 6] = [
        (
            "8k window, no reserve, exactly full",
            7_168,
            0,
            8_192,
            false,
        ),
        ("8k window, no reserve, one over", 7_169, 0, 8_192, true),
        (
            "8k window, reserve fills it exactly",
            3_072,
            4_096,
            8_192,
            false,
        ),
        ("8k window, reserve one over", 3_073, 4_096, 8_192, true),
        (
            "an estimate at u64::MAX saturates to a skip",
            u64::MAX,
            0,
            8_192,
            true,
        ),
        (
            "a reserve at u64::MAX saturates to a skip",
            0,
            u64::MAX,
            8_192,
            true,
        ),
    ];

    for (name, estimate, reserve, window, expected) in rows {
        assert_eq!(
            exceeds_window(estimate, reserve, window),
            expected,
            "{name}"
        );
    }
}

/// The reserve is the request's `max_tokens`, else the configured ceiling,
/// else the default the egress fills when it must send `max_tokens`, else 0.
///
/// Mutation check: drop the default-fill arm of `output_reserve` (return 0
/// when no ceiling is configured) -> red on the anthropic-api and
/// bedrock-invoke default rows.
#[test]
fn the_output_reserve_prefers_the_request_then_the_ceiling_then_the_egress_fill() {
    use routectl_core::wire_check::EgressKind;
    use routectl_providers::anthropic_api::DEFAULT_MAX_OUTPUT_TOKENS;
    struct ReserveRow {
        name: &'static str,
        max_tokens: Option<u32>,
        ceiling: u32,
        egress: Option<EgressKind>,
        reserve: u64,
    }
    let fill = u64::from(DEFAULT_MAX_OUTPUT_TOKENS);
    let row = |name, max_tokens, ceiling, egress, reserve| ReserveRow {
        name,
        max_tokens,
        ceiling,
        egress,
        reserve,
    };
    let rows = [
        row(
            "request max_tokens beats the configured ceiling",
            Some(1_000),
            64_000,
            Some(EgressKind::AnthropicApi),
            1_000,
        ),
        row(
            "request max_tokens above the configured ceiling still wins",
            Some(128_000),
            64_000,
            Some(EgressKind::AnthropicApi),
            128_000,
        ),
        row(
            "request max_tokens on an egress that never fills",
            Some(1_000),
            0,
            Some(EgressKind::OpenaiCompat),
            1_000,
        ),
        row(
            "configured ceiling when the request sets none",
            None,
            32_000,
            Some(EgressKind::AnthropicApi),
            32_000,
        ),
        row(
            "anthropic-api with no configured ceiling reserves the egress fill",
            None,
            0,
            Some(EgressKind::AnthropicApi),
            fill,
        ),
        row(
            "bedrock-invoke with no configured ceiling reserves the egress fill",
            None,
            0,
            Some(EgressKind::BedrockInvoke),
            fill,
        ),
        row(
            "openai-compat with no configured ceiling reserves nothing",
            None,
            0,
            Some(EgressKind::OpenaiCompat),
            0,
        ),
        row("an unknown egress reserves nothing", None, 0, None, 0),
    ];
    assert_eq!(fill, 64_000, "the egress default is the documented 64000");

    for row in rows {
        let mut req = oversized_request();
        req.max_tokens = row.max_tokens;
        let mut target = target_with_window("t", 200_000);
        target.max_output_tokens = row.ceiling;
        target.egress_kind = row.egress;

        assert_eq!(output_reserve(&req, &target), row.reserve, "{}", row.name);
    }
}

/// An anthropic-api target with no configured ceiling is gated with the
/// 64000 tokens its egress will send: a request that fits only when no
/// output is reserved is skipped, through the real chain expansion that
/// derives the egress kind from `[providers.X]`.
#[test]
fn an_anthropic_target_without_a_ceiling_reserves_the_egress_fill() {
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.p]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"literal:k\"\n\
         [window_gate]\n\
         enabled = true\n",
    )
    .expect("config parses");
    let mut router = Router::new(Arc::new(config));
    let window: u32 = 200_000;
    let fits_without_reserve = u64::from(window) - HEADROOM_TOKENS - 1_000;
    let req = request_with_estimate(fits_without_reserve, None);
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let models = BTreeMap::from([
        (
            "windowed".to_string(),
            Arc::new(crate::factory::apply_model_knobs(
                ResolvedModel::new("windowed", "p", Arc::clone(&provider), "windowed"),
                "windowed",
                &ModelEntry::new("p", "windowed").with_context_window(window),
            )),
        ),
        (
            "unconfirmed".to_string(),
            Arc::new(ResolvedModel::new(
                "unconfirmed",
                "p",
                provider,
                "unconfirmed",
            )),
        ),
    ]);
    router.install_resolved_models(models);
    let chain = router.expand_chain_to_targets(
        vec![
            Arc::clone(&router.resolved_models["windowed"]),
            Arc::clone(&router.resolved_models["unconfirmed"]),
        ],
        None,
    );
    assert_eq!(
        chain[0].egress_kind,
        Some(routectl_core::wire_check::EgressKind::AnthropicApi),
        "premise: chain expansion derives the egress kind",
    );

    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(nicknames(&kept), vec!["unconfirmed"]);
}

#[test]
fn the_warn_throttle_refuses_a_second_claim_inside_the_interval() {
    let throttle = SkipWarnThrottle::new();
    let now = 1_000_000;

    assert!(throttle.claim(now));
    assert!(!throttle.claim(now));
    assert!(!throttle.claim(now + SKIP_WARN_INTERVAL_SECS - 1));
    assert!(throttle.claim(now + SKIP_WARN_INTERVAL_SECS));
}

// ---------------------------------------------------------------------------
// Learned per-lane correction of the estimate the gate compares.
// ---------------------------------------------------------------------------

/// Provider kind of every calibratable target below. Real token: the lane key
/// carries whatever `DispatchTarget::provider_kind` holds.
const LANE_KIND: &str = "openai-compat";

/// The correction the fed evidence below reduces to: the upstream counted
/// twice what the byte-length estimate predicted.
const UNDER_COUNT_PERMILLE: u64 = 2_000;

/// A target on a confirmed window that also carries a provider kind, so it
/// forms a lane. `into_one_dispatch_target` leaves the kind unset, which is
/// itself the no-lane case exercised further down.
fn calibratable_target(nickname: &str, window: u32) -> DispatchTarget {
    let mut target = target_with_window(nickname, window);
    target.provider_kind = Some(LANE_KIND);
    target
}

/// Feed one lane enough balanced evidence to produce a correction, at the
/// ratio implied by `estimated` / `actual`. Goes through the SAME public
/// recording entry point production uses, so the key the gate later queries
/// on is proven to match the key the live write landed under. `upstream` is
/// the lane's wire id, which for the fixture targets equals the nickname.
fn feed_lane(router: &Router, upstream: Option<&str>, estimated: u64, actual: u64) {
    for i in 0..9 {
        router.record_calibration_sample(
            Some(LANE_KIND),
            upstream,
            Some(&format!("caller-{}", i % 3)),
            estimated,
            actual,
            SystemTime::now(),
        );
    }
}

/// A window the CORRECTED estimate exceeds while the RAW estimate fits it.
/// The one fixture that separates a corrected gate from the static one:
/// `raw + headroom <= w < raw * factor + headroom`, which holds for any
/// factor above one since `w` is the smallest window the raw estimate fits.
fn only_the_corrected_estimate_overflows(req: &ChatRequest) -> u32 {
    just_inside_margin(req)
}

/// The gate's verdict on a two-target chain whose FIRST target sits exactly
/// at the raw-estimate margin boundary and whose second always survives.
///
/// The ONE shared assertion helper behind every "behaves exactly as the
/// static gate" claim below: it returns the kept nicknames plus the skip
/// count, so a corrected run and an uncorrected run are compared as data
/// rather than as four separately-written expectations.
fn verdict_on_boundary_chain(router: &Router, req: &ChatRequest) -> (Vec<String>, u64) {
    let chain = vec![
        calibratable_target("boundary", only_the_corrected_estimate_overflows(req)),
        calibratable_target("roomy", comfortably_large(req)),
    ];
    let kept = router.filter_chain_by_window(chain, req);
    let names = nicknames(&kept).iter().map(|n| (*n).to_string()).collect();
    (names, router.metrics.window_gate_skips_total())
}

/// What the static gate does on that chain: keeps both targets, counts no
/// skip. Every `None` cause must reproduce this exactly.
fn static_gate_verdict() -> (Vec<String>, u64) {
    (vec!["boundary".to_string(), "roomy".to_string()], 0)
}

#[test]
fn a_calibrated_lane_skips_a_target_the_raw_estimate_would_have_kept() {
    // The feature, in one assertion. The raw estimate fits this window; the
    // lane's learned correction says the real count is twice that, so the
    // target genuinely cannot hold the request and is skipped.
    let router = router(true);
    let req = oversized_request();
    feed_lane(
        &router,
        Some("boundary"),
        estimated_tokens(&req),
        estimated_tokens(&req) * UNDER_COUNT_PERMILLE / 1_000,
    );

    let (kept, skips) = verdict_on_boundary_chain(&router, &req);

    assert_eq!(kept, vec!["roomy".to_string()]);
    assert_eq!(skips, 1);
    assert_ne!(
        (kept, skips),
        static_gate_verdict(),
        "the fixture must actually separate a corrected gate from the static one",
    );
}

#[test]
fn a_cold_lane_behaves_exactly_as_the_static_gate() {
    // No evidence at all.
    let router = router(true);
    let req = oversized_request();

    assert_eq!(
        verdict_on_boundary_chain(&router, &req),
        static_gate_verdict()
    );
}

#[test]
fn a_thin_lane_behaves_exactly_as_the_static_gate() {
    // Evidence at the correcting ratio, but below the reduction's floors:
    // one caller, two samples.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    for _ in 0..2 {
        router.record_calibration_sample(
            Some(LANE_KIND),
            Some("boundary"),
            Some("one-caller"),
            raw,
            raw * UNDER_COUNT_PERMILLE / 1_000,
            SystemTime::now(),
        );
    }

    assert_eq!(
        verdict_on_boundary_chain(&router, &req),
        static_gate_verdict()
    );
}

#[test]
fn a_stale_lane_behaves_exactly_as_the_static_gate() {
    // Enough balanced evidence, all of it long expired.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    let long_ago = SystemTime::UNIX_EPOCH;
    for i in 0..9 {
        router.record_calibration_sample(
            Some(LANE_KIND),
            Some("boundary"),
            Some(&format!("caller-{}", i % 3)),
            raw,
            raw * UNDER_COUNT_PERMILLE / 1_000,
            long_ago,
        );
    }

    assert_eq!(
        verdict_on_boundary_chain(&router, &req),
        static_gate_verdict()
    );
}

#[test]
fn a_lane_whose_ratio_is_out_of_range_behaves_exactly_as_the_static_gate() {
    // Enough balanced fresh evidence, reducing to an implausible ratio. The
    // refusal (rather than a clamp to the band's edge) is what sends this
    // lane back to the static gate instead of letting it still move it.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    feed_lane(&router, Some("boundary"), raw, raw * 50);

    assert_eq!(
        verdict_on_boundary_chain(&router, &req),
        static_gate_verdict()
    );
}

#[test]
fn calibration_enabled_false_still_applies_a_learned_factor() {
    // The key parses and is ignored: a lane with evidence that corrects moves
    // the gate exactly as it does with the key absent.
    let router = router_with(true, false);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    feed_lane(
        &router,
        Some("boundary"),
        raw,
        raw * UNDER_COUNT_PERMILLE / 1_000,
    );

    let (kept, skips) = verdict_on_boundary_chain(&router, &req);

    assert_eq!(kept, vec!["roomy".to_string()]);
    assert_eq!(skips, 1);
}

#[test]
fn a_lane_with_too_few_callers_behaves_exactly_as_the_static_gate() {
    // Enough fresh samples at the correcting ratio, from only two callers:
    // the distinct-caller floor refuses the factor.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    for i in 0..9 {
        router.record_calibration_sample(
            Some(LANE_KIND),
            Some("boundary"),
            Some(&format!("caller-{}", i % 2)),
            raw,
            raw * UNDER_COUNT_PERMILLE / 1_000,
            SystemTime::now(),
        );
    }

    assert_eq!(
        verdict_on_boundary_chain(&router, &req),
        static_gate_verdict()
    );
}

#[test]
fn a_dispatch_without_an_upstream_records_no_lane() {
    // The write side's missing half: no wire upstream, no lane, and no
    // other label stands in for it.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);

    feed_lane(&router, None, raw, raw * UNDER_COUNT_PERMILLE / 1_000);

    assert!(
        router.calibration_store.is_empty(),
        "a dispatch with no upstream must create no lane",
    );
}

#[test]
fn a_nickname_less_target_reads_its_upstream_lane() {
    // The read side never needed the nickname: the lane is the target's
    // upstream, which a target always carries.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    feed_lane(
        &router,
        Some("boundary"),
        raw,
        raw * UNDER_COUNT_PERMILLE / 1_000,
    );
    let mut nameless = calibratable_target("boundary", only_the_corrected_estimate_overflows(&req));
    nameless.nickname = None;
    let chain = vec![
        nameless,
        calibratable_target("roomy", comfortably_large(&req)),
    ];

    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(nicknames(&kept), vec!["roomy"]);
    assert_eq!(router.metrics.window_gate_skips_total(), 1);
}

#[test]
fn a_target_without_a_provider_kind_forms_no_lane() {
    // The other half of the key. `into_one_dispatch_target` leaves the kind
    // unset on the legacy construction path, and an unset kind must refuse
    // the lane rather than key on the upstream alone.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    feed_lane(
        &router,
        Some("boundary"),
        raw,
        raw * UNDER_COUNT_PERMILLE / 1_000,
    );

    let chain = vec![
        target_with_window("boundary", only_the_corrected_estimate_overflows(&req)),
        target_with_window("roomy", comfortably_large(&req)),
    ];
    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(nicknames(&kept), vec!["boundary", "roomy"]);
    assert_eq!(router.metrics.window_gate_skips_total(), 0);
}

#[test]
fn each_chain_target_is_corrected_by_its_own_lane() {
    // The factor is per-lane and each target IS a lane, so one raw
    // serialization feeds a per-target corrected figure. Two targets on the
    // same window: only the one whose lane learned an under-count is skipped.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    let window = only_the_corrected_estimate_overflows(&req);
    feed_lane(
        &router,
        Some("corrected"),
        raw,
        raw * UNDER_COUNT_PERMILLE / 1_000,
    );

    let chain = vec![
        calibratable_target("corrected", window),
        calibratable_target("uncorrected", window),
    ];
    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(nicknames(&kept), vec!["uncorrected"]);
    assert_eq!(router.metrics.window_gate_skips_total(), 1);
}

#[test]
fn an_over_counting_lane_admits_a_target_the_static_gate_would_skip() {
    // The other direction, pinned at the gate rather than in the reduction: a
    // lane whose real token count runs BELOW the estimate shrinks the
    // corrected figure, so a target the static gate skipped is kept.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    // Half the estimate, i.e. a 500-permille factor.
    feed_lane(&router, Some("halved"), raw, raw / 2);
    // A window the RAW estimate overflows (raw + headroom > w) while the
    // halved one fits it (raw/2 + headroom <= w, as raw is far above twice
    // the headroom): the window equal to the raw estimate.
    let window = u32::try_from(raw).expect("fixture estimate fits u32");

    // A target on that window with no lane of its own IS skipped, which is
    // what makes the kept assertion below attributable to the correction.
    let control = router.filter_chain_by_window(
        vec![
            calibratable_target("plain", window),
            calibratable_target("roomy", comfortably_large(&req)),
        ],
        &req,
    );
    assert_eq!(nicknames(&control), vec!["roomy"]);

    let chain = vec![
        calibratable_target("halved", window),
        calibratable_target("roomy", comfortably_large(&req)),
    ];
    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(
        nicknames(&kept),
        vec!["halved", "roomy"],
        "the corrected estimate fits a window the raw one did not",
    );
}

#[test]
fn the_skip_warn_reports_both_the_raw_and_the_corrected_figure() {
    // The divergence between the two is the only way an operator can see a
    // learned correction move a skip. Neither figure, nor anything else on
    // the line, may carry request content.
    let throttle = SkipWarnThrottle::new();
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    feed_lane(
        &router,
        Some("boundary"),
        raw,
        raw * UNDER_COUNT_PERMILLE / 1_000,
    );

    let events = routectl_testkit::capture_events(|| {
        let chain = vec![
            calibratable_target("boundary", only_the_corrected_estimate_overflows(&req)),
            calibratable_target("roomy", comfortably_large(&req)),
        ];
        let kept = router.filter_chain_by_window_with(chain, &req, &throttle);
        assert_eq!(nicknames(&kept), vec!["roomy"]);
    });

    let warn = events
        .iter()
        .find(|e| e.field("event") == Some("window_gate_skip"))
        .expect("one skip WARN");
    assert_eq!(
        warn.field("estimated_tokens").map(str::to_string),
        Some(raw.to_string()),
    );
    assert_eq!(
        warn.field("corrected_tokens").map(str::to_string),
        Some((raw * UNDER_COUNT_PERMILLE / 1_000).to_string()),
    );
    let rendered = format!("{} {:?}", warn.message, warn.fields);
    assert!(
        !rendered.contains(REQUEST_BODY_MARKER),
        "no request content may reach the log line; got {rendered}",
    );
}

// ---------------------------------------------------------------------------
// Reserve-aware boundary on real window sizes, set through the `[models.X]`
// entry the factory stamps resolved models from.
// ---------------------------------------------------------------------------

/// The output ceiling Anthropic-shape lanes commonly carry.
const CEILING_64K: u32 = 64_000;

/// A request whose estimate is exactly `tokens`. Built by measuring the
/// serialized overhead of an empty message and padding with ASCII, which
/// serializes byte for byte, so the size is exact rather than approximate.
fn request_with_estimate(tokens: u64, max_tokens: Option<u32>) -> ChatRequest {
    let with_text = |text: String| ChatRequest {
        model: "alias".into(),
        max_tokens,
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(text),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    };
    let overhead = serde_json::to_string(&with_text(String::new()))
        .expect("request serializes")
        .len();
    let wanted = usize::try_from(tokens * 4).expect("fixture size fits usize");
    let req = with_text("x".repeat(wanted - overhead));
    assert_eq!(estimated_tokens(&req), tokens, "fixture estimate is exact");
    req
}

/// A target named `nickname` whose context window and output ceiling come
/// from its `[models.X]` entry, stamped the way the factory stamps them.
fn configured_target(nickname: &str, window: u32, ceiling: Option<u32>) -> DispatchTarget {
    let mut entry = ModelEntry::new("p", nickname).with_context_window(window);
    if let Some(ceiling) = ceiling {
        entry = entry.with_max_output_tokens(ceiling);
    }
    let target = target(nickname, &entry);
    assert_eq!(target.model.context_window_tokens(), Some(window));
    target
}

struct BoundaryRow {
    name: &'static str,
    window: u32,
    ceiling: Option<u32>,
    max_tokens: Option<u32>,
    input_tokens: u64,
    kept: bool,
}

#[test]
fn the_gate_keeps_a_target_exactly_at_input_plus_reserve_plus_headroom() {
    const W200K: u32 = 200_000;
    const W1M: u32 = 1_000_000;
    let at = |window: u32, reserve: u32| u64::from(window - reserve) - HEADROOM_TOKENS;
    let rows = [
        BoundaryRow {
            name: "200k, target ceiling reserve, exactly full",
            window: W200K,
            ceiling: Some(CEILING_64K),
            max_tokens: None,
            input_tokens: at(W200K, CEILING_64K),
            kept: true,
        },
        BoundaryRow {
            name: "200k, target ceiling reserve, one over",
            window: W200K,
            ceiling: Some(CEILING_64K),
            max_tokens: None,
            input_tokens: at(W200K, CEILING_64K) + 1,
            kept: false,
        },
        BoundaryRow {
            name: "1M, target ceiling reserve, exactly full",
            window: W1M,
            ceiling: Some(CEILING_64K),
            max_tokens: None,
            input_tokens: at(W1M, CEILING_64K),
            kept: true,
        },
        BoundaryRow {
            name: "1M, target ceiling reserve, one over",
            window: W1M,
            ceiling: Some(CEILING_64K),
            max_tokens: None,
            input_tokens: at(W1M, CEILING_64K) + 1,
            kept: false,
        },
        BoundaryRow {
            name: "1M, 900k input with a 64k reserve fits",
            window: W1M,
            ceiling: Some(CEILING_64K),
            max_tokens: None,
            input_tokens: 900_000,
            kept: true,
        },
        BoundaryRow {
            name: "200k, request max_tokens beats a larger target ceiling, exactly full",
            window: W200K,
            ceiling: Some(128_000),
            max_tokens: Some(CEILING_64K),
            input_tokens: at(W200K, CEILING_64K),
            kept: true,
        },
        BoundaryRow {
            name: "200k, request max_tokens beats a larger target ceiling, one over",
            window: W200K,
            ceiling: Some(128_000),
            max_tokens: Some(CEILING_64K),
            input_tokens: at(W200K, CEILING_64K) + 1,
            kept: false,
        },
        BoundaryRow {
            name: "200k, request max_tokens above the target ceiling wins, exactly full",
            window: W200K,
            ceiling: Some(CEILING_64K),
            max_tokens: Some(128_000),
            input_tokens: at(W200K, 128_000),
            kept: true,
        },
        BoundaryRow {
            name: "200k, request max_tokens above the target ceiling wins, one over",
            window: W200K,
            ceiling: Some(CEILING_64K),
            max_tokens: Some(128_000),
            input_tokens: at(W200K, 128_000) + 1,
            kept: false,
        },
        BoundaryRow {
            name: "200k, no max_tokens and no ceiling reserves nothing, exactly full",
            window: W200K,
            ceiling: None,
            max_tokens: None,
            input_tokens: at(W200K, 0),
            kept: true,
        },
        BoundaryRow {
            name: "200k, no max_tokens and no ceiling reserves nothing, one over",
            window: W200K,
            ceiling: None,
            max_tokens: None,
            input_tokens: at(W200K, 0) + 1,
            kept: false,
        },
    ];

    for row in rows {
        let router = router(true);
        let req = request_with_estimate(row.input_tokens, row.max_tokens);
        let chain = vec![
            configured_target("windowed", row.window, row.ceiling),
            target_without_window("unconfirmed"),
        ];

        let kept = router.filter_chain_by_window(chain, &req);

        let expected = if row.kept {
            vec!["windowed", "unconfirmed"]
        } else {
            vec!["unconfirmed"]
        };
        assert_eq!(nicknames(&kept), expected, "{}", row.name);
    }
}
