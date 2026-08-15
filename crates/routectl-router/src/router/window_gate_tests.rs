//! Tests for the proactive context-window gate: the margin predicate, the
//! three never-skip-the-last layers, the unknown-window keep, the kill
//! switch, and the bounded skip WARN.
//!
//! Every window in these tests is derived from the fixture request's own
//! estimate, so the assertions pin the RATIO rather than a byte count the
//! estimator could drift under.

use super::*;

use std::sync::Arc;

use routectl_core::{ChatChunk, ChatResponse, Message, MessageContent, Provider, Result, Role};

use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::config::Config;
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

/// A router with both switches set explicitly. The calibration switch is
/// separate from the gate's own: turning the correction off must leave the
/// static gate fully intact.
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

/// A window the estimate CLEARLY exceeds: half the estimate, so the
/// estimate is well past 3/4 of it.
fn clearly_too_small(req: &ChatRequest) -> u32 {
    u32::try_from(estimated_tokens(req) / 2).expect("fixture estimate fits u32")
}

/// A window just large enough that the estimate sits INSIDE the safety
/// margin: `estimate <= window * 3 / 4`, by the narrowest slack that
/// survives integer truncation. The safety fixture -- a gate that skips
/// this target has lost its margin.
fn just_inside_margin(req: &ChatRequest) -> u32 {
    let estimate = estimated_tokens(req);
    u32::try_from(estimate * 4 / 3 + 4).expect("fixture estimate fits u32")
}

/// A window no plausible margin makes too small: eight times the estimate.
/// Used as the surviving alternative wherever the property under test is
/// about the OTHER target.
fn comfortably_large(req: &ChatRequest) -> u32 {
    u32::try_from(estimated_tokens(req) * 8).expect("fixture estimate fits u32")
}

/// A dispatch target on provider `p` whose effective row is the given
/// merge result.
fn target(nickname: &str, effective_row: EffectiveRow) -> DispatchTarget {
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let model =
        ResolvedModel::new(nickname, "p", provider, "upstream").with_effective_row(effective_row);
    into_one_dispatch_target(Arc::new(model))
}

/// A target whose catalog row confirms `window` tokens.
fn target_with_window(nickname: &str, window: u32) -> DispatchTarget {
    let mut row = CatalogRow::sentinel();
    row.max_context_tokens = Some(window);
    target(
        nickname,
        EffectiveRow::Present {
            row,
            source: Source::Baked,
            verified_at: "seed".to_string(),
        },
    )
}

/// A target whose catalog row exists but leaves the window unset.
fn target_without_window(nickname: &str) -> DispatchTarget {
    let mut row = CatalogRow::sentinel();
    row.max_context_tokens = None;
    target(
        nickname,
        EffectiveRow::Present {
            row,
            source: Source::Baked,
            verified_at: "seed".to_string(),
        },
    )
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

#[test]
fn an_unconfirmed_window_keeps_the_target() {
    let router = router(true);
    let req = oversized_request();
    let chain = vec![
        target_without_window("unset"),
        target("disabled", EffectiveRow::Disabled),
        target("missing", EffectiveRow::Missing),
        target_with_window("small", clearly_too_small(&req)),
    ];

    let kept = router.filter_chain_by_window(chain, &req);

    assert_eq!(
        nicknames(&kept),
        vec!["unset", "disabled", "missing"],
        "only the confirmed-too-small window is skipped",
    );
    assert_eq!(router.metrics.window_gate_skips_total(), 1);
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
fn the_margin_predicate_skips_only_past_three_quarters_of_the_window() {
    assert!(!exceeds_window_margin(0, 1_000));
    assert!(!exceeds_window_margin(750, 1_000));
    assert!(exceeds_window_margin(751, 1_000));
    assert!(exceeds_window_margin(1_000, 1_000));
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
/// on is proven to match the key the live write landed under.
fn feed_lane(router: &Router, nickname: Option<&str>, estimated: u64, actual: u64) {
    for i in 0..9 {
        router.record_calibration_sample(
            Some(LANE_KIND),
            nickname,
            Some(&format!("caller-{}", i % 3)),
            estimated,
            actual,
            SystemTime::now(),
        );
    }
}

/// A window the CORRECTED estimate clearly exceeds while the RAW estimate
/// sits inside the margin. The one fixture that separates a corrected gate
/// from the static one: `raw <= w*3/4 < raw*factor`.
fn only_the_corrected_estimate_overflows(req: &ChatRequest) -> u32 {
    let raw = estimated_tokens(req);
    // `just_inside_margin` is the largest window the raw estimate still fits;
    // the corrected estimate is `factor` times larger, so it overflows it as
    // long as the factor exceeds the 4/3 slack the margin allows.
    just_inside_margin(req)
        .min(u32::try_from(raw * UNDER_COUNT_PERMILLE / 1_000 * 4 / 3).expect("fits u32"))
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
fn the_calibration_kill_switch_behaves_exactly_as_the_static_gate() {
    // A lane with evidence that WOULD correct, and the switch off. Proves the
    // switch stops the correction without touching the gate itself, and that
    // the evidence is retained rather than discarded.
    let router = router_with(true, false);
    let req = oversized_request();
    let raw = estimated_tokens(&req);
    feed_lane(
        &router,
        Some("boundary"),
        raw,
        raw * UNDER_COUNT_PERMILLE / 1_000,
    );

    assert_eq!(
        verdict_on_boundary_chain(&router, &req),
        static_gate_verdict()
    );
    assert!(
        !router.calibration_store.is_empty(),
        "switching the correction off must not discard collected evidence",
    );
}

#[test]
fn a_nickname_less_target_forms_no_lane_on_either_side() {
    // A target lacking a nickname has no lane, so neither the recording path
    // nor the gate's lookup may invent one from another label. Feeding under
    // `None` records nothing at all, and the reverse -- evidence recorded
    // under a real nickname -- must not reach a target that has none.
    let router = router(true);
    let req = oversized_request();
    let raw = estimated_tokens(&req);

    feed_lane(&router, None, raw, raw * UNDER_COUNT_PERMILLE / 1_000);
    assert!(
        router.calibration_store.is_empty(),
        "a nickname-less dispatch must create no lane",
    );

    // Evidence under a real nickname, consulted by a target that has none.
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

    assert_eq!(
        nicknames(&kept),
        vec!["", "roomy"],
        "a nickname-less target is compared against the raw estimate",
    );
    assert_eq!(router.metrics.window_gate_skips_total(), 0);
}

#[test]
fn a_target_without_a_provider_kind_forms_no_lane() {
    // The other half of the key. `into_one_dispatch_target` leaves the kind
    // unset on the legacy construction path, and an unset kind must refuse
    // the lane rather than key on the nickname alone.
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
    // A window the RAW estimate overflows (raw > w*3/4) while the halved one
    // fits it (raw/2 <= w*3/4): the window equal to the raw estimate.
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
