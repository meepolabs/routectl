//! Drift guard: the pure display resolver's precedence order must agree
//! with the router's consolidated within-target precedence matrix
//! (`capability_precedence_matrix_tests`). One test per rule of the
//! settled chain `override > learned > verified-working > prior >
//! unknown`, phrased over the resolver's three read-only inputs. The
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
    let dv = resolve_display_verdict(route_away(), learned_negative(FailurePhase::F1), None);
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
        Some(false),
    );
    assert_eq!(dv.verdict, FORCED_SUPPORTED);
    assert_eq!(dv.supported, Some(true));
    assert_eq!(dv.source, Some(SOURCE_OVERRIDE));
}

// --- learned (F1 and F2) > prior ---

#[test]
fn learned_f1_negative_outranks_catalog_prior() {
    let dv = resolve_display_verdict(None, learned_negative(FailurePhase::F1), Some(false));
    assert_eq!(
        dv.verdict,
        Verdict::LearnedBroken(FailurePhase::F1).as_str()
    );
    assert_eq!(dv.supported, Some(false));
    assert_eq!(dv.source, Some(SOURCE_LIVE));
}

#[test]
fn learned_f2_negative_outranks_catalog_prior() {
    let dv = resolve_display_verdict(None, learned_negative(FailurePhase::F2), Some(false));
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
    let dv = resolve_display_verdict(None, None, Some(false));
    assert_eq!(dv.verdict, Verdict::Assumed(false).as_str());
    assert_eq!(dv.supported, Some(false));
    assert_eq!(dv.source, Some(SOURCE_PRIOR));
}

#[test]
fn prior_true_resolves_supported_from_prior() {
    let dv = resolve_display_verdict(None, None, Some(true));
    assert_eq!(dv.verdict, Verdict::Assumed(true).as_str());
    assert_eq!(dv.supported, Some(true));
    assert_eq!(dv.source, Some(SOURCE_PRIOR));
}

#[test]
fn absent_prior_resolves_unknown_with_no_source() {
    let dv = resolve_display_verdict(None, None, None);
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
    let dv = resolve_display_verdict(None, verified(), Some(false));
    assert_eq!(dv.verdict, Verdict::VerifiedWorking.as_str());
    assert_eq!(dv.supported, Some(true));
    assert_eq!(dv.source, Some(SOURCE_LIVE));
}

// --- override hard-drop > verified-working ---

#[test]
fn override_route_away_beats_resident_verified_working() {
    let dv = resolve_display_verdict(route_away(), verified(), None);
    assert_eq!(dv.verdict, FORCED_UNSUPPORTED);
    assert_eq!(dv.supported, Some(false));
    assert_eq!(dv.source, Some(SOURCE_OVERRIDE));
}

// --- verified-working > unknown ---

#[test]
fn verified_working_with_no_prior_is_supported() {
    let dv = resolve_display_verdict(None, verified(), None);
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
    );
    assert_eq!(dv.source, Some(SOURCE_PROBE));
}
