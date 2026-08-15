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
    let body = format!(
        "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://x\"\n\
         api_key_ref = \"literal:k\"\n\
         [window_gate]\n\
         enabled = {gate_enabled}\n"
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
