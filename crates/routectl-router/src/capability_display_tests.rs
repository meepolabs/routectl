//! The display projections of the seam's display mode: one row per tier and
//! polarity, pinning the verdict token, polarity, source tag and shown layer
//! a read-only surface renders next to the action the seam decided. The
//! action itself is pinned against dispatch by the walk and withheld-beta
//! parity tests; here it rides along so a shown signal that does not act (an
//! advisory or uncorroborated negative, the kill switch off) is visibly
//! distinct from the tier that decided.

use super::*;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};

use crate::capability_precedence::{
    ACTION_ALLOW, ACTION_DROP, ACTION_NONE, ACTION_REPROBE, ACTION_ROUTE_AWAY, ACTION_STRIP,
    ACTION_WITHHOLD, CellContext, DisplayInputs, Layer, LearnedInput, SeedCell, resolve_display,
};
use crate::override_registry::OverrideVerdict;

const ROUTE_AWAY: Option<OverrideVerdict> = Some(OverrideVerdict::RouteAway);
const FORCE: Option<OverrideVerdict> = Some(OverrideVerdict::ForceSupported);
const WITHHOLD: Option<SeedCell> = Some(SeedCell::Withhold);
const CLEARED: Option<SeedCell> = Some(SeedCell::Cleared);

const FEATURE: CellContext = CellContext::from_facts(true, false, false, true);
const DROPPABLE: CellContext = CellContext::from_facts(true, false, true, true);
const FEATURE_OFF: CellContext = CellContext::from_facts(false, false, false, true);
const BETA: CellContext = CellContext::from_facts(true, true, true, true);
const BETA_OFF: CellContext = CellContext::from_facts(false, true, true, true);
const BETA_UNREPRESENTABLE: CellContext = CellContext::from_facts(true, true, true, false);

const fn acting(phase: FailurePhase, source: EvidenceSource) -> Option<LearnedInput> {
    Some(LearnedInput::Acting {
        phase,
        tier: SignalTier::SelfIdentifying,
        source,
    })
}

const LIVE_F1: Option<LearnedInput> = acting(FailurePhase::F1, EvidenceSource::Live);
const LIVE_F2: Option<LearnedInput> = acting(FailurePhase::F2, EvidenceSource::Live);
const LIVE_F3: Option<LearnedInput> = acting(FailurePhase::F3, EvidenceSource::Live);
const PROBE_F1: Option<LearnedInput> = acting(FailurePhase::F1, EvidenceSource::Probe);
const LAPSED: Option<LearnedInput> = Some(LearnedInput::Lapsed {
    source: EvidenceSource::Live,
});
const VERIFIED: Option<LearnedInput> = Some(LearnedInput::Verified {
    source: EvidenceSource::Live,
});
const VERIFIED_PROBE: Option<LearnedInput> = Some(LearnedInput::Verified {
    source: EvidenceSource::Probe,
});
const UNCORROBORATED: Option<LearnedInput> = Some(LearnedInput::Uncorroborated {
    source: EvidenceSource::Live,
});

const fn inputs(ctx: CellContext) -> DisplayInputs {
    DisplayInputs {
        pinned: false,
        override_verdict: None,
        learned: None,
        seed: None,
        prior: None,
        ctx,
    }
}

const BROKEN_TOKEN: &str = Verdict::LearnedBroken(FailurePhase::F1).as_str();
const VERIFIED_TOKEN: &str = Verdict::VerifiedWorking.as_str();
const CLEARED_TOKEN: &str = Verdict::Cleared.as_str();
const ASSUMED_FALSE: &str = Verdict::Assumed(false).as_str();
const ASSUMED_TRUE: &str = Verdict::Assumed(true).as_str();
const UNKNOWN: &str = Verdict::Unknown.as_str();

type Projection = (
    &'static str,
    Option<bool>,
    Option<&'static str>,
    Option<Layer>,
    &'static str,
);

#[test]
#[rustfmt::skip]
fn each_tier_projects_its_verdict_source_and_layer() {
    let rows: &[(&str, DisplayInputs, Projection)] = &[
        // --- operator tiers ---
        ("a pinned beta flag shows the pin over a learned negative",
         DisplayInputs { pinned: true, learned: LIVE_F1, seed: WITHHOLD, ..inputs(BETA) },
         (FORCED_SUPPORTED, Some(true), Some(SOURCE_PIN), Some(Layer::Pin), ACTION_ALLOW)),
        ("a pin beats representability",
         DisplayInputs { pinned: true, ..inputs(BETA_UNREPRESENTABLE) },
         (FORCED_SUPPORTED, Some(true), Some(SOURCE_PIN), Some(Layer::Pin), ACTION_ALLOW)),
        ("an unrepresentable beta flag shows representability over force_supported",
         DisplayInputs { override_verdict: FORCE, ..inputs(BETA_UNREPRESENTABLE) },
         (VERDICT_UNREPRESENTABLE, Some(false), Some(SOURCE_REPRESENTABILITY),
          Some(Layer::Representability), ACTION_WITHHOLD)),
        ("a route-away override beats an acting negative",
         DisplayInputs { override_verdict: ROUTE_AWAY, learned: LIVE_F1, ..inputs(FEATURE) },
         (FORCED_UNSUPPORTED, Some(false), Some(SOURCE_OVERRIDE), Some(Layer::Override), ACTION_DROP)),
        ("a route-away override beats a verified positive",
         DisplayInputs { override_verdict: ROUTE_AWAY, learned: VERIFIED, ..inputs(FEATURE) },
         (FORCED_UNSUPPORTED, Some(false), Some(SOURCE_OVERRIDE), Some(Layer::Override), ACTION_DROP)),
        ("a route-away override acts with the kill switch off",
         DisplayInputs { override_verdict: ROUTE_AWAY, ..inputs(FEATURE_OFF) },
         (FORCED_UNSUPPORTED, Some(false), Some(SOURCE_OVERRIDE), Some(Layer::Override), ACTION_DROP)),
        ("a route-away override withholds a beta flag",
         DisplayInputs { override_verdict: ROUTE_AWAY, ..inputs(BETA) },
         (FORCED_UNSUPPORTED, Some(false), Some(SOURCE_OVERRIDE), Some(Layer::Override), ACTION_WITHHOLD)),
        ("force_supported masks a negative and a false prior",
         DisplayInputs { override_verdict: FORCE, learned: LIVE_F1, prior: Some(false), ..inputs(FEATURE) },
         (FORCED_SUPPORTED, Some(true), Some(SOURCE_OVERRIDE), Some(Layer::Override), ACTION_ALLOW)),
        ("force_supported sends over the seed",
         DisplayInputs { override_verdict: FORCE, seed: WITHHOLD, ..inputs(BETA) },
         (FORCED_SUPPORTED, Some(true), Some(SOURCE_OVERRIDE), Some(Layer::Override), ACTION_ALLOW)),
        // --- learned ---
        ("an F1 negative outranks a false prior",
         DisplayInputs { learned: LIVE_F1, prior: Some(false), ..inputs(FEATURE) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_ROUTE_AWAY)),
        ("an F2 negative outranks a true prior and never strips",
         DisplayInputs { learned: LIVE_F2, prior: Some(true), ..inputs(DROPPABLE) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_ROUTE_AWAY)),
        ("a droppable F1 negative strips",
         DisplayInputs { learned: LIVE_F1, ..inputs(DROPPABLE) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_STRIP)),
        ("a probe negative is tagged probe",
         DisplayInputs { learned: PROBE_F1, ..inputs(FEATURE) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_PROBE), Some(Layer::Learned), ACTION_ROUTE_AWAY)),
        ("a verified positive masks a false prior",
         DisplayInputs { learned: VERIFIED, prior: Some(false), ..inputs(FEATURE) },
         (VERIFIED_TOKEN, Some(true), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_ALLOW)),
        ("a probe positive is tagged probe",
         DisplayInputs { learned: VERIFIED_PROBE, ..inputs(FEATURE) },
         (VERIFIED_TOKEN, Some(true), Some(SOURCE_PROBE), Some(Layer::Learned), ACTION_ALLOW)),
        ("a lapsed negative re-probes",
         DisplayInputs { learned: LAPSED, prior: Some(false), ..inputs(DROPPABLE) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_REPROBE)),
        ("a learned beta negative withholds over a cleared seed",
         DisplayInputs { learned: LIVE_F1, seed: CLEARED, ..inputs(BETA) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_WITHHOLD)),
        ("a learned beta positive sends over the seed",
         DisplayInputs { learned: VERIFIED, seed: WITHHOLD, ..inputs(BETA) },
         (VERIFIED_TOKEN, Some(true), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_ALLOW)),
        // --- a shown learned signal with no opinion ---
        ("an advisory live F3 negative is shown while the prior decides",
         DisplayInputs { learned: LIVE_F3, prior: Some(false), ..inputs(FEATURE) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_ROUTE_AWAY)),
        ("an uncorroborated negative is shown while the seed decides",
         DisplayInputs { learned: UNCORROBORATED, seed: WITHHOLD, ..inputs(BETA) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_WITHHOLD)),
        ("the kill switch off shows a negative that takes no action",
         DisplayInputs { learned: LIVE_F1, ..inputs(FEATURE_OFF) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_NONE)),
        // --- seed ---
        ("the seed withholds over a true prior",
         DisplayInputs { seed: WITHHOLD, prior: Some(true), ..inputs(BETA) },
         (BROKEN_TOKEN, Some(false), Some(SOURCE_SEED), Some(Layer::Seed), ACTION_WITHHOLD)),
        ("a cleared seed sends over a false prior",
         DisplayInputs { seed: CLEARED, prior: Some(false), ..inputs(BETA) },
         (CLEARED_TOKEN, Some(true), Some(SOURCE_SEED), Some(Layer::Seed), ACTION_ALLOW)),
        ("the seed withholds with the kill switch off",
         DisplayInputs { seed: WITHHOLD, learned: VERIFIED, ..inputs(BETA_OFF) },
         (VERIFIED_TOKEN, Some(true), Some(SOURCE_LIVE), Some(Layer::Learned), ACTION_WITHHOLD)),
        // --- prior ---
        ("a false prior routes away",
         DisplayInputs { prior: Some(false), ..inputs(FEATURE) },
         (ASSUMED_FALSE, Some(false), Some(SOURCE_PRIOR), Some(Layer::Prior), ACTION_ROUTE_AWAY)),
        ("a true prior takes no action",
         DisplayInputs { prior: Some(true), ..inputs(FEATURE) },
         (ASSUMED_TRUE, Some(true), Some(SOURCE_PRIOR), Some(Layer::Prior), ACTION_NONE)),
        ("the kill switch off shows a prior that takes no action",
         DisplayInputs { prior: Some(false), ..inputs(FEATURE_OFF) },
         (ASSUMED_FALSE, Some(false), Some(SOURCE_PRIOR), Some(Layer::Prior), ACTION_NONE)),
        ("a prior on a beta key is shown with no action",
         DisplayInputs { prior: Some(false), ..inputs(BETA) },
         (ASSUMED_FALSE, Some(false), Some(SOURCE_PRIOR), Some(Layer::Prior), ACTION_NONE)),
        // --- no signal ---
        ("no signal is unknown", inputs(FEATURE),
         (UNKNOWN, None, None, None, ACTION_NONE)),
        ("an unseeded beta key with no signal is unknown", inputs(BETA),
         (UNKNOWN, None, None, None, ACTION_NONE)),
    ];
    for (name, display_inputs, expected) in rows {
        let resolution = resolve_display(*display_inputs);

        assert_eq!(
            (
                resolution.display_verdict(),
                resolution.display_supported(),
                resolution.display_source(),
                resolution.display_layer(),
                resolution.as_str(),
            ),
            *expected,
            "{name}"
        );
    }
}
