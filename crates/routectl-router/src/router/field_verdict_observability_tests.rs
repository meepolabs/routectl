//! Coverage for the live status/doctor visibility surface: the acting
//! field-verdict provenance filter and the process-lifetime repair
//! counters snapshot.

use super::*;

use std::sync::Arc;
use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};

use crate::config::Config;
use crate::learned_capability::LearnedRegistryEntry;

const THINKING_DISPLAY_PATH: &str = "thinking.enabled.display";
const COMPUTER_USE_DISPLAY_PATH: &str = "computer_use.display_number";

fn bare_router() -> Router {
    Router::new(Arc::new(Config::default()))
}

/// Mints a real field capability key through the sole constructor, rather
/// than spelling the complete key as a literal -- the second spelling that
/// the workspace-wide prefix-uniqueness scan exists to forbid.
fn field_key(path: &str) -> String {
    crate::field_capability::field_capability_key(path).expect("path is a valid field path")
}

fn entry(feature_key: &str, verdict: Verdict, source: EvidenceSource) -> LearnedRegistryEntry {
    let now = Instant::now();
    LearnedRegistryEntry {
        state_key: "anthropic-api:claude-sonnet-4-5".to_string(),
        feature_key: feature_key.to_string(),
        verdict,
        signal_tier: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: now,
        last_seen: now,
        expires_at: now,
        evidence_class: None,
        phase: match verdict {
            Verdict::LearnedBroken(phase) => phase,
            _ => FailurePhase::F2,
        },
        source,
    }
}

/// A field-namespaced `LearnedBroken` row is the only row status/doctor call
/// "acting" -- included with its state key, feature key, phase and source
/// carried through unchanged.
#[test]
fn acting_field_verdicts_includes_a_field_namespaced_learned_broken_row() {
    let expected_key = field_key(THINKING_DISPLAY_PATH);
    let rows = vec![entry(
        &expected_key,
        Verdict::LearnedBroken(FailurePhase::F1),
        EvidenceSource::Live,
    )];

    let acting = acting_field_verdicts(&rows);

    assert_eq!(acting.len(), 1);
    assert_eq!(acting[0].state_key, "anthropic-api:claude-sonnet-4-5");
    assert_eq!(acting[0].feature_key, expected_key);
    assert_eq!(acting[0].phase, FailurePhase::F1);
    assert_eq!(acting[0].source, EvidenceSource::Live);
}

/// A catalog-scoped `LearnedBroken` row (no `field:` prefix) is a different
/// namespace entirely and must not be reported as an acting field verdict.
#[test]
fn acting_field_verdicts_excludes_a_catalog_scoped_negative() {
    let rows = vec![entry(
        "web_search",
        Verdict::LearnedBroken(FailurePhase::F2),
        EvidenceSource::Live,
    )];

    assert!(acting_field_verdicts(&rows).is_empty());
}

/// A field-namespaced row that is NOT `LearnedBroken` (a cleared verdict, a
/// verified positive) is not acting -- surfacing it would claim a drop that
/// is not happening.
#[test]
fn acting_field_verdicts_excludes_non_broken_field_rows() {
    let rows = vec![
        entry(
            &field_key(THINKING_DISPLAY_PATH),
            Verdict::Cleared,
            EvidenceSource::Live,
        ),
        entry(
            &field_key(COMPUTER_USE_DISPLAY_PATH),
            Verdict::VerifiedWorking,
            EvidenceSource::Probe,
        ),
    ];

    assert!(acting_field_verdicts(&rows).is_empty());
}

/// An empty registry snapshot reports no acting verdicts -- the base case a
/// hand-picked-only fixture could otherwise pass vacuously.
#[test]
fn acting_field_verdicts_is_empty_for_an_empty_snapshot() {
    assert!(acting_field_verdicts(&[]).is_empty());
}

/// A mixed snapshot reports exactly the acting rows, in order, leaving the
/// non-acting rows out rather than merely not crashing on them.
#[test]
fn acting_field_verdicts_filters_a_mixed_snapshot_to_only_the_acting_rows() {
    let thinking_key = field_key(THINKING_DISPLAY_PATH);
    let computer_use_key = field_key(COMPUTER_USE_DISPLAY_PATH);
    let rows = vec![
        entry(
            &thinking_key,
            Verdict::LearnedBroken(FailurePhase::F1),
            EvidenceSource::Live,
        ),
        entry(
            "web_search",
            Verdict::LearnedBroken(FailurePhase::F2),
            EvidenceSource::Live,
        ),
        entry(
            &computer_use_key,
            Verdict::LearnedBroken(FailurePhase::F3),
            EvidenceSource::Probe,
        ),
        entry(&thinking_key, Verdict::Cleared, EvidenceSource::Live),
    ];

    let acting = acting_field_verdicts(&rows);

    assert_eq!(acting.len(), 2);
    assert_eq!(acting[0].feature_key, thinking_key);
    assert_eq!(acting[1].feature_key, computer_use_key);
}

/// An F3 negative sourced from live traffic is advisory-only per the
/// learned-capability registry's own routing decision: a probe, not this
/// row alone, settles whether the target is actually routed away.
/// Status/doctor must not report it as acting.
#[test]
fn acting_field_verdicts_excludes_an_f3_live_advisory_field_row() {
    let rows = vec![entry(
        &field_key(THINKING_DISPLAY_PATH),
        Verdict::LearnedBroken(FailurePhase::F3),
        EvidenceSource::Live,
    )];

    assert!(acting_field_verdicts(&rows).is_empty());
}

/// The same F3 negative, sourced from a probe instead, carries routing
/// authority and does act -- the source is what flips the verdict, not the
/// phase alone.
#[test]
fn acting_field_verdicts_includes_an_f3_probe_field_row() {
    let expected_key = field_key(THINKING_DISPLAY_PATH);
    let rows = vec![entry(
        &expected_key,
        Verdict::LearnedBroken(FailurePhase::F3),
        EvidenceSource::Probe,
    )];

    let acting = acting_field_verdicts(&rows);

    assert_eq!(acting.len(), 1);
    assert_eq!(acting[0].feature_key, expected_key);
    assert_eq!(acting[0].phase, FailurePhase::F3);
    assert_eq!(acting[0].source, EvidenceSource::Probe);
}

/// The counters snapshot starts at zero on a freshly constructed router --
/// the base case a fixture that only ever bumps counters could not fail on.
#[test]
fn field_repair_counters_starts_at_zero() {
    let router = bare_router();

    let counters = router.field_repair_counters();

    assert_eq!(counters.repair_attempted, 0);
    assert_eq!(counters.repair_succeeded, 0);
    assert_eq!(counters.verdicts_learned, 0);
    assert_eq!(counters.outstanding_unconfirmed, 0);
    assert_eq!(counters.disproved_requests, 0);
}

/// The counters snapshot reflects the live metrics the moment they move --
/// this is the read path status/doctor rely on, so it must not lag or cache.
#[test]
fn field_repair_counters_reflects_the_live_metrics_after_they_move() {
    let router = bare_router();

    router.metrics.incr_field_repair_attempted();
    router.metrics.incr_field_repair_succeeded();
    router.metrics.incr_field_verdicts_learned();

    let counters = router.field_repair_counters();

    assert_eq!(counters.repair_attempted, 1);
    assert_eq!(counters.repair_succeeded, 1);
    assert_eq!(counters.verdicts_learned, 1);
}

/// The two wrong-repair ALARM halves come from the shared canary registry rather
/// than the metrics counters, so they need their own drive: this reads them back
/// through the same `field_repair_counters()` snapshot status/doctor call, after
/// moving the registry the router actually owns.
///
/// Both halves are asserted NONZERO and are deliberately DIFFERENT numbers, so
/// neither assertion can pass on the other's value and a snapshot that wired one
/// field to the wrong source is caught.
///
/// Mutation checks, each red on its own named assertion: hardcode
/// `outstanding_unconfirmed: 0` in `Router::field_repair_counters` -> red on the
/// outstanding assertion; hardcode `disproved_requests: 0` -> red on the
/// lifetime assertion; SWAP the two right-hand sources between the fields -> red
/// on both, which is what the differing expected values buy (equal fixtures
/// would let a swap pass).
///
/// The same swap one layer out -- between the two `rc_field_*` log field names in
/// the CLI reporter -- is invisible here and in every trace assertion, so it is
/// pinned by a source guard in that module
/// (`each_alarm_log_field_reads_the_counter_it_is_named_for`).
#[test]
fn field_repair_counters_reports_both_nonzero_alarm_halves() {
    let router = bare_router();
    let canaries = router.field_verdicts().canaries();
    let disproved_key = crate::field_verdict::FieldVerdictKey::new(
        "anthropic-api:claude-sonnet-4-5",
        THINKING_DISPLAY_PATH,
        "anthropic-api",
    )
    .expect("a qualified path mints a key");
    let live_key = crate::field_verdict::FieldVerdictKey::new(
        "anthropic-api:claude-opus-4-1",
        COMPUTER_USE_DISPLAY_PATH,
        "anthropic-api",
    )
    .expect("a qualified path mints a key");

    // Identity one: three repaired requests, then a canary DISPROVES the verdict,
    // transferring all three into the lifetime alarm and leaving it holding none.
    for _ in 0..3 {
        drop(
            canaries
                .begin_modified_request(&disproved_key, 1)
                .expect("accounts at the resident incarnation"),
        );
    }
    canaries
        .claim_canary(&disproved_key, 1)
        .expect("claim admitted")
        .settle(crate::field_canary::CanaryOutcome::Regressed);
    // Identity two: two repaired requests nothing has disproved, so these stay
    // OUTSTANDING. A different count from the three above on purpose.
    for _ in 0..2 {
        drop(
            canaries
                .begin_modified_request(&live_key, 1)
                .expect("accounts at the resident incarnation"),
        );
    }

    let counters = router.field_repair_counters();

    assert_eq!(
        counters.outstanding_unconfirmed, 2,
        "the CURRENT half reports the exposure no canary has settled, and does \
         not include the requests the disproof already transferred out",
    );
    assert_eq!(
        counters.disproved_requests, 3,
        "the LIFETIME half reports every request the disproved verdict had \
         modified -- requests affected, not canary attempts",
    );
}
