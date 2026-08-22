//! What a raised `max_output_tokens` ceiling does to the legacy-path
//! `thinking.budget_tokens` -- the SPEND coupling of an injected ceiling.
//!
//! Reasoning tokens are billed as output, so a ceiling that arrives from the
//! catalog rather than from the operator's own config can move real money. The
//! coupling is narrow but real: `budget_from_level` supplies an exact budget
//! for every level in the standard vocabulary and is ceiling-INDEPENDENT, so
//! the derived budget only tracks the ceiling on the three paths that reach the
//! `[1024, max_tokens - 1]` window clamp -- `effort: "max"` (whose table entry
//! exceeds both ceilings, so the clamp decides), `reasoning.enabled = true` with
//! no effort or budget (half the ceiling), and an EXPLICIT
//! `reasoning.max_tokens` above the prior ceiling (clamped to `ceiling - 1`, so
//! raising the ceiling un-clamps it).
//!
//! `effort: "max"` additionally requires the model's `effort_levels` to include
//! `"max"`: the tests here leave `RoutectlInternal::effort_levels` empty, which
//! is the pass-through case, whereas the shipped `[models.X]` default of
//! `["low", "medium", "high"]` clamps `max` down to `high` (24576 flat, no
//! ceiling coupling at all).
//!
//! These are pinned because the fill is silent at the request boundary: an
//! operator who never wrote a ceiling has no local artifact naming the figure
//! their thinking budget is now a fraction of.

use routectl_core::{ChatRequest, ReasoningConfig, RoutectlInternal};

use super::super::types::ThinkingConfig;
use super::build_thinking;

/// The two ceilings a Claude Opus selector resolves to before and after the
/// catalog fill: the egress's own hardcoded baseline, and the figure the baked
/// table confirms for the 4.6-4.8 globs.
const BASELINE_CEILING: u32 = 64_000;
const FILLED_CEILING: u32 = 128_000;

/// The legacy-path budget for a request that omits `max_tokens` and carries
/// `reasoning`, under a router-injected ceiling of `ceiling`. `effort_levels`
/// is left EMPTY (the pass-through case) so the requested effort reaches
/// `budget_from_level` unclamped -- see
/// [`effort_max_under_the_default_effort_levels_is_clamped_to_high`] for what
/// the shipped default does instead.
fn derived_budget(ceiling: u32, reasoning: ReasoningConfig) -> u32 {
    derived_budget_with_levels(ceiling, reasoning, &[])
}

/// [`derived_budget`] with the model's `[models.X] effort_levels` allowlist
/// spelled out.
fn derived_budget_with_levels(
    ceiling: u32,
    reasoning: ReasoningConfig,
    effort_levels: &[&str],
) -> u32 {
    let mut internal = RoutectlInternal::default();
    internal.max_output_tokens = ceiling;
    internal.effort_levels = effort_levels.iter().map(|l| (*l).to_string()).collect();
    let req = ChatRequest {
        max_tokens: None,
        reasoning: Some(reasoning),
        routectl_internal: internal,
        ..Default::default()
    };
    match build_thinking(&req, false).expect("reasoning activates thinking") {
        ThinkingConfig::Enabled { budget_tokens, .. } => budget_tokens,
        other => panic!("expected the legacy Enabled shape, got {other:?}"),
    }
}

#[test]
fn effort_max_derives_a_budget_that_tracks_the_injected_ceiling() {
    // `max` has a table entry (128000), but it exceeds both ceilings, so
    // Anthropic's `budget < max_tokens` clamp is what decides -- making this
    // the path where a catalog-filled ceiling doubles the thinking budget, and
    // with it the reasoning tokens billed as output. Requires the model's
    // effort_levels to admit `max`; `derived_budget` leaves the list empty,
    // which is the pass-through case.
    let baseline = derived_budget(
        BASELINE_CEILING,
        ReasoningConfig {
            effort: Some("max".into()),
            ..Default::default()
        },
    );
    let filled = derived_budget(
        FILLED_CEILING,
        ReasoningConfig {
            effort: Some("max".into()),
            ..Default::default()
        },
    );

    assert_eq!(baseline, BASELINE_CEILING - 1);
    assert_eq!(filled, FILLED_CEILING - 1);
    assert_eq!(
        filled,
        baseline * 2 + 1,
        "a catalog-filled ceiling doubles the derived budget at effort=max"
    );
}

#[test]
fn effort_max_under_the_default_effort_levels_is_clamped_to_high() {
    // The precondition on the effort=max path: `max` only reaches the window
    // clamp when the model's effort_levels admit it. Under the shipped
    // `[models.X]` default the clamp rewrites it to `high`, whose exact table
    // budget is ceiling-independent -- so most configs see no effort=max spend
    // change from the fill at all, and the spend note has to say so.
    const DEFAULT_LEVELS: [&str; 3] = ["low", "medium", "high"];
    let at_max = || ReasoningConfig {
        effort: Some("max".into()),
        ..Default::default()
    };

    assert_eq!(
        derived_budget_with_levels(BASELINE_CEILING, at_max(), &DEFAULT_LEVELS),
        24_576,
        "the default allowlist clamps max -> high, taking the flat table budget"
    );
    assert_eq!(
        derived_budget_with_levels(FILLED_CEILING, at_max(), &DEFAULT_LEVELS),
        24_576,
        "and that budget does not move with the ceiling"
    );
}

#[test]
fn bare_enabled_reasoning_derives_half_the_injected_ceiling() {
    // No effort and no explicit budget: the half-of-max fallback is the other
    // ceiling-proportional path, so it doubles with the fill too.
    let bare = || ReasoningConfig {
        enabled: Some(true),
        ..Default::default()
    };

    assert_eq!(derived_budget(BASELINE_CEILING, bare()), 32_000);
    assert_eq!(derived_budget(FILLED_CEILING, bare()), 64_000);
}

#[test]
fn a_standard_effort_level_derives_the_same_budget_under_either_ceiling() {
    // The bound on the blast radius, and the reason the spend note names only
    // the ceiling-tracking paths above: every level in the standard vocabulary
    // resolves through the exact effort->budget table, which the ceiling does
    // not enter. Without this, "the fill changes thinking spend" would read as
    // applying to every reasoning request.
    for level in ["minimal", "low", "medium", "high", "xhigh"] {
        let with_level = |ceiling| {
            derived_budget(
                ceiling,
                ReasoningConfig {
                    effort: Some(level.to_string()),
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            with_level(BASELINE_CEILING),
            with_level(FILLED_CEILING),
            "effort={level} must derive a ceiling-independent budget"
        );
    }
}

#[test]
fn an_explicit_caller_budget_within_both_windows_is_unmoved_by_the_injected_ceiling() {
    // A caller whose own budget already fits under BOTH ceilings is never
    // re-derived, so a filled ceiling cannot raise its spend. The bound
    // matters: `clamp_budget_to_legacy_window` caps an explicit budget at
    // `max_tokens - 1`, so this holds only inside the lower window -- see the
    // regression case below for a budget above it.
    let explicit = || ReasoningConfig {
        max_tokens: Some(20_000),
        ..Default::default()
    };

    assert_eq!(derived_budget(BASELINE_CEILING, explicit()), 20_000);
    assert_eq!(derived_budget(FILLED_CEILING, explicit()), 20_000);
}

#[test]
fn an_explicit_caller_budget_above_the_prior_ceiling_tracks_the_injected_ceiling() {
    // The third ceiling-tracking path, and the one easiest to miss: an explicit
    // budget ABOVE the pre-fill ceiling was being clamped down to
    // `baseline - 1`, and the fill un-clamps it to the caller's full ask. The
    // caller's number did not change; what routectl sends upstream did, and
    // those reasoning tokens bill as output.
    let explicit = || ReasoningConfig {
        max_tokens: Some(100_000),
        ..Default::default()
    };

    assert_eq!(
        derived_budget(BASELINE_CEILING, explicit()),
        BASELINE_CEILING - 1,
        "under the baseline ceiling the window clamp caps the explicit ask"
    );
    assert_eq!(
        derived_budget(FILLED_CEILING, explicit()),
        100_000,
        "the filled ceiling admits the caller's full ask, raising real spend"
    );
}
