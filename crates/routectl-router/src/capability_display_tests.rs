//! Drift guard: the pure display resolver's precedence order must agree
//! with the router's consolidated within-target precedence matrix
//! (`capability_precedence_matrix_tests`). One test per rule of the
//! settled chain `override > learned > verified-working > seed > prior >
//! unknown`, phrased over the resolver's read-only inputs. The
//! router matrix pins the SIDE-EFFECTING dispatch seam; this pins the
//! EXTRACTED read-only resolver against the same rules, so the display
//! surface can never silently diverge from routing behavior.

use super::*;

use routectl_core::capability::{EvidenceSource, FailurePhase, Verdict};

use crate::override_registry::{OverrideProvenance, OverrideVerdict};

/// A learned negative seen on live traffic, in the given phase.
fn learned_negative(phase: FailurePhase) -> Option<(Verdict, EvidenceSource)> {
    Some((Verdict::LearnedBroken(phase), EvidenceSource::Live))
}

/// A resident verified-working positive seen on live traffic.
fn verified() -> Option<(Verdict, EvidenceSource)> {
    Some((Verdict::VerifiedWorking, EvidenceSource::Live))
}

fn route_away() -> Option<(OverrideVerdict, OverrideProvenance)> {
    Some((OverrideVerdict::RouteAway, OverrideProvenance::Override))
}

fn force_supported() -> Option<(OverrideVerdict, OverrideProvenance)> {
    Some((
        OverrideVerdict::ForceSupported,
        OverrideProvenance::Override,
    ))
}

// --- override hard-drop > learned ---

#[test]
fn override_route_away_beats_acting_learned_negative() {
    // Router rule: an override RouteAway hard-drops ahead of any learned
    // signal (FilterSource::Override wins).
    let dv = resolve_display_verdict(route_away(), learned_negative(FailurePhase::F1), None, None);
    assert_eq!(dv.verdict, FORCED_UNSUPPORTED);
    assert_eq!(dv.supported, Some(false));
    assert_eq!(dv.source, Some(SOURCE_OVERRIDE));
}

// --- force_supported masks learned AND prior ---

#[test]
fn force_supported_masks_both_learned_negative_and_catalog_prior() {
    // Router rule: force_supported short-circuits the cell to allow before
    // the learned consult, and the prior pass skips it too (router returns
    // None). The display side asserts it supported via the override.
    let dv = resolve_display_verdict(
        force_supported(),
        learned_negative(FailurePhase::F1),
        None,
        Some(false),
    );
    assert_eq!(dv.verdict, FORCED_SUPPORTED);
    assert_eq!(dv.supported, Some(true));
    assert_eq!(dv.source, Some(SOURCE_OVERRIDE));
}

// --- learned (F1 and F2) > prior ---

#[test]
fn learned_f1_negative_outranks_catalog_prior() {
    let dv = resolve_display_verdict(None, learned_negative(FailurePhase::F1), None, Some(false));
    assert_eq!(
        dv.verdict,
        Verdict::LearnedBroken(FailurePhase::F1).as_str()
    );
    assert_eq!(dv.supported, Some(false));
    assert_eq!(dv.source, Some(SOURCE_LIVE));
}

#[test]
fn learned_f2_negative_outranks_catalog_prior() {
    let dv = resolve_display_verdict(None, learned_negative(FailurePhase::F2), None, Some(false));
    assert_eq!(
        dv.verdict,
        Verdict::LearnedBroken(FailurePhase::F2).as_str()
    );
    assert_eq!(dv.supported, Some(false));
    assert_eq!(dv.source, Some(SOURCE_LIVE));
}

// --- prior=false soft-tails; prior=true allows; None permissive ---

#[test]
fn prior_false_alone_resolves_to_prior_source() {
    // Router rule: a Some(false) prior with no higher signal soft-tails
    // with FilterSource::Prior. The display cell carries the prior source
    // and unsupported polarity.
    let dv = resolve_display_verdict(None, None, None, Some(false));
    assert_eq!(dv.verdict, Verdict::Assumed(false).as_str());
    assert_eq!(dv.supported, Some(false));
    assert_eq!(dv.source, Some(SOURCE_PRIOR));
}

#[test]
fn prior_true_resolves_supported_from_prior() {
    let dv = resolve_display_verdict(None, None, None, Some(true));
    assert_eq!(dv.verdict, Verdict::Assumed(true).as_str());
    assert_eq!(dv.supported, Some(true));
    assert_eq!(dv.source, Some(SOURCE_PRIOR));
}

#[test]
fn absent_prior_resolves_unknown_with_no_source() {
    let dv = resolve_display_verdict(None, None, None, None);
    assert_eq!(dv.verdict, Verdict::Unknown.as_str());
    assert_eq!(dv.supported, None);
    assert_eq!(dv.source, None);
}

// --- verified-working > prior ---

#[test]
fn verified_working_masks_catalog_prior() {
    // Router rule: a resident VerifiedWorking positive masks a Some(false)
    // prior (the prior pass skips a verified cell). The display cell is
    // supported from the learned layer, never the prior.
    let dv = resolve_display_verdict(None, verified(), None, Some(false));
    assert_eq!(dv.verdict, Verdict::VerifiedWorking.as_str());
    assert_eq!(dv.supported, Some(true));
    assert_eq!(dv.source, Some(SOURCE_LIVE));
}

// --- override hard-drop > verified-working ---

#[test]
fn override_route_away_beats_resident_verified_working() {
    let dv = resolve_display_verdict(route_away(), verified(), None, None);
    assert_eq!(dv.verdict, FORCED_UNSUPPORTED);
    assert_eq!(dv.supported, Some(false));
    assert_eq!(dv.source, Some(SOURCE_OVERRIDE));
}

// --- verified-working > unknown ---

#[test]
fn verified_working_with_no_prior_is_supported() {
    let dv = resolve_display_verdict(None, verified(), None, None);
    assert_eq!(dv.verdict, Verdict::VerifiedWorking.as_str());
    assert_eq!(dv.supported, Some(true));
    assert_eq!(dv.source, Some(SOURCE_LIVE));
}

// --- evidence source flows through to the tag ---

#[test]
fn probe_evidence_tags_the_cell_probe() {
    let dv = resolve_display_verdict(
        None,
        Some((Verdict::VerifiedWorking, EvidenceSource::Probe)),
        None,
        None,
    );
    assert_eq!(dv.source, Some(SOURCE_PROBE));
}

// --- the routing action mirrors the dispatch filter ---

fn negative(phase: FailurePhase, source: EvidenceSource) -> LearnedActing {
    LearnedActing {
        verified: false,
        acting: true,
        lapsed: false,
        phase,
        source,
    }
}

fn action(
    override_cell: Option<(OverrideVerdict, OverrideProvenance)>,
    learned: Option<LearnedActing>,
    prior: Option<bool>,
    strip_applies: bool,
    capability_enabled: bool,
) -> &'static str {
    let display = resolve_display_verdict(
        override_cell,
        learned.map(|l| {
            let verdict = if l.verified {
                Verdict::VerifiedWorking
            } else {
                Verdict::LearnedBroken(l.phase)
            };
            (verdict, l.source)
        }),
        None,
        prior,
    );
    resolve_display_action(ActionInputs {
        display,
        learned,
        prior,
        strip_applies,
        capability_enabled,
        beta_flag: false,
        seed: None,
    })
}

#[test]
fn display_action_rows_mirror_the_dispatch_filter() {
    let live_f1 = negative(FailurePhase::F1, EvidenceSource::Live);
    let live_f2 = negative(FailurePhase::F2, EvidenceSource::Live);
    let live_f3 = negative(FailurePhase::F3, EvidenceSource::Live);
    let probe_f3 = negative(FailurePhase::F3, EvidenceSource::Probe);
    let lapsed = LearnedActing {
        lapsed: true,
        ..live_f1
    };
    let uncorroborated = LearnedActing {
        acting: false,
        ..live_f1
    };
    let positive = LearnedActing {
        verified: true,
        ..live_f1
    };
    let cases: &[(&str, &'static str, &'static str)] = &[
        (
            "override route-away drops",
            action(route_away(), Some(live_f1), None, true, true),
            ACTION_DROP,
        ),
        (
            "override drops with the kill switch off",
            action(route_away(), None, None, false, false),
            ACTION_DROP,
        ),
        (
            "force-supported allows",
            action(force_supported(), Some(live_f1), Some(false), true, true),
            ACTION_ALLOW,
        ),
        (
            "droppable F1 negative strips",
            action(None, Some(live_f1), None, true, true),
            ACTION_STRIP,
        ),
        (
            "essential F1 negative routes away",
            action(None, Some(live_f1), None, false, true),
            ACTION_ROUTE_AWAY,
        ),
        (
            "F2 negative never strips",
            action(None, Some(live_f2), None, true, true),
            ACTION_ROUTE_AWAY,
        ),
        (
            "probe F3 negative routes away",
            action(None, Some(probe_f3), None, true, true),
            ACTION_ROUTE_AWAY,
        ),
        (
            "live F3 negative is advisory",
            action(None, Some(live_f3), None, true, true),
            ACTION_NONE,
        ),
        (
            "advisory negative falls to a false prior",
            action(None, Some(live_f3), Some(false), true, true),
            ACTION_ROUTE_AWAY,
        ),
        (
            "lapsed negative re-probes",
            action(None, Some(lapsed), None, true, true),
            ACTION_REPROBE,
        ),
        (
            "uncorroborated negative falls to the prior",
            action(None, Some(uncorroborated), Some(false), true, true),
            ACTION_ROUTE_AWAY,
        ),
        (
            "verified positive allows",
            action(None, Some(positive), Some(false), true, true),
            ACTION_ALLOW,
        ),
        (
            "false prior routes away",
            action(None, None, Some(false), true, true),
            ACTION_ROUTE_AWAY,
        ),
        (
            "true prior takes no action",
            action(None, None, Some(true), true, true),
            ACTION_NONE,
        ),
        (
            "kill switch off silences learned",
            action(None, Some(live_f1), None, true, false),
            ACTION_NONE,
        ),
        (
            "kill switch off silences prior",
            action(None, None, Some(false), true, false),
            ACTION_NONE,
        ),
        (
            "no signal takes no action",
            action(None, None, None, true, true),
            ACTION_NONE,
        ),
    ];
    for (name, got, want) in cases {
        assert_eq!(got, want, "{name}");
    }
}

// --- the shipped beta seed sits below learned and above the prior ---

#[test]
fn seed_precedence_rows_mirror_the_withheld_beta_pass() {
    let withhold = Some(SeedCell::Withhold);
    let cleared = Some(SeedCell::Cleared);
    let cases: &[(&str, DisplayVerdict, &str, Option<&str>)] = &[
        (
            "override beats the seed",
            resolve_display_verdict(force_supported(), None, withhold, None),
            FORCED_SUPPORTED,
            Some(SOURCE_OVERRIDE),
        ),
        (
            "a learned positive beats the seed",
            resolve_display_verdict(None, verified(), withhold, None),
            Verdict::VerifiedWorking.as_str(),
            Some(SOURCE_LIVE),
        ),
        (
            "a learned negative beats a cleared seed",
            resolve_display_verdict(None, learned_negative(FailurePhase::F1), cleared, None),
            Verdict::LearnedBroken(FailurePhase::F1).as_str(),
            Some(SOURCE_LIVE),
        ),
        (
            "the seed beats a supporting prior",
            resolve_display_verdict(None, None, withhold, Some(true)),
            Verdict::LearnedBroken(FailurePhase::F1).as_str(),
            Some(SOURCE_SEED),
        ),
        (
            "a cleared seed beats an opposing prior",
            resolve_display_verdict(None, None, cleared, Some(false)),
            Verdict::Cleared.as_str(),
            Some(SOURCE_SEED),
        ),
        (
            "no seed leaves the prior",
            resolve_display_verdict(None, None, None, Some(false)),
            Verdict::Assumed(false).as_str(),
            Some(SOURCE_PRIOR),
        ),
        (
            "no signal at all is unknown",
            resolve_display_verdict(None, None, None, None),
            Verdict::Unknown.as_str(),
            None,
        ),
    ];
    for (name, got, verdict, source) in cases {
        assert_eq!(got.verdict, *verdict, "{name}");
        assert_eq!(got.source, *source, "{name}");
    }
}

#[test]
fn a_seed_cell_is_unsupported_and_a_cleared_seed_is_supported() {
    let seeded = resolve_display_verdict(None, None, Some(SeedCell::Withhold), None);
    let cleared = resolve_display_verdict(None, None, Some(SeedCell::Cleared), None);

    assert_eq!(seeded.supported, Some(false));
    assert_eq!(cleared.supported, Some(true));
}

/// The action for a beta-key cell, resolving the display from the same inputs.
fn beta_action(
    override_cell: Option<(OverrideVerdict, OverrideProvenance)>,
    learned: Option<LearnedActing>,
    seed: Option<SeedCell>,
    capability_enabled: bool,
) -> &'static str {
    let display = resolve_display_verdict(
        override_cell,
        learned.map(|l| {
            let verdict = if l.verified {
                Verdict::VerifiedWorking
            } else {
                Verdict::LearnedBroken(l.phase)
            };
            (verdict, l.source)
        }),
        seed,
        None,
    );
    resolve_display_action(ActionInputs {
        display,
        learned,
        prior: None,
        strip_applies: true,
        capability_enabled,
        beta_flag: true,
        seed,
    })
}

#[test]
fn beta_action_rows_mirror_the_withheld_beta_pass() {
    let live_f1 = negative(FailurePhase::F1, EvidenceSource::Live);
    let lapsed = LearnedActing {
        lapsed: true,
        ..live_f1
    };
    let uncorroborated = LearnedActing {
        acting: false,
        ..live_f1
    };
    let positive = LearnedActing {
        verified: true,
        ..live_f1
    };
    let withhold = Some(SeedCell::Withhold);
    let cleared = Some(SeedCell::Cleared);
    let cases: &[(&str, &'static str, &'static str)] = &[
        (
            "a learned beta negative withholds, never strips",
            beta_action(None, Some(live_f1), None, true),
            ACTION_WITHHOLD,
        ),
        (
            "a route-away override on a beta key withholds, never drops",
            beta_action(route_away(), None, None, true),
            ACTION_WITHHOLD,
        ),
        (
            "a force-supported override sends over the seed",
            beta_action(force_supported(), None, withhold, true),
            ACTION_ALLOW,
        ),
        (
            "a learned positive sends over the seed",
            beta_action(None, Some(positive), withhold, true),
            ACTION_ALLOW,
        ),
        (
            "a lapsed negative re-probes over the seed",
            beta_action(None, Some(lapsed), withhold, true),
            ACTION_REPROBE,
        ),
        (
            "an uncorroborated negative falls to the seed",
            beta_action(None, Some(uncorroborated), withhold, true),
            ACTION_WITHHOLD,
        ),
        (
            "the seed alone withholds",
            beta_action(None, None, withhold, true),
            ACTION_WITHHOLD,
        ),
        (
            "the seed withholds with the kill switch off",
            beta_action(None, None, withhold, false),
            ACTION_WITHHOLD,
        ),
        (
            "the kill switch off silences learned and leaves the seed",
            beta_action(None, Some(positive), withhold, false),
            ACTION_WITHHOLD,
        ),
        (
            "a cleared seed sends",
            beta_action(None, None, cleared, true),
            ACTION_ALLOW,
        ),
        (
            "a learned negative over a cleared seed withholds",
            beta_action(None, Some(live_f1), cleared, true),
            ACTION_WITHHOLD,
        ),
        (
            "an unseeded beta key with no signal takes no action",
            beta_action(None, None, None, true),
            ACTION_NONE,
        ),
    ];
    for (name, got, want) in cases {
        assert_eq!(got, want, "{name}");
    }
}
