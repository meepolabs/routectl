//! A probe guard settles against the generation it was ADMITTED under.
//!
//! A re-probe is admitted, the request dispatches, and the settlement lands
//! later. A reload can complete in between. The settlement then describes a
//! probe issued against a catalog revision the daemon has left, so applying it
//! would clear or back off an entry the live generation still believes -- and
//! emit a cleared event whose row a later boot would replay. Each guard
//! therefore captures its generation at admission and settles through the
//! generation-aware API.

use super::runtime_gate::{
    LearnedProbeGuard, ProbeAdmission, ProbeAdmissionSet, SameCapabilitySettlement,
};
use crate::learned_capability::{
    DEFAULT_MAX_ENTRIES, LearnedCapabilityRegistry, ProbeOutcome, RoutingDecision,
};
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROVIDER: &str = "openai-compat";

/// The capability key the guard fixtures probe.
///
/// Catalog-SCOPED on purpose: that is the class the barrier refuses from a stale
/// generation. A wire-shape key is always admitted by design, so it could not
/// discriminate a refusal from an acceptance.
fn probe_key() -> String {
    "web_search".to_string()
}

/// Advance the generation WITHOUT pruning.
///
/// The full boundary transition also evicts the catalog-scoped entries, which
/// would remove the very entry these tests inspect -- so "still resident" could
/// not tell a refused settlement from a pruned one. Advancing alone reproduces
/// the staleness the guard must detect while leaving the entry observable.
fn advance_without_pruning(reg: &LearnedCapabilityRegistry) {
    reg.advance_generation();
}

fn registry() -> Arc<LearnedCapabilityRegistry> {
    Arc::new(LearnedCapabilityRegistry::new(
        Duration::from_hours(1),
        Duration::from_mins(1),
        DEFAULT_MAX_ENTRIES,
    ))
}

/// A registry holding one acting negative with an in-flight probe slot claimed,
/// which is the state a guard is armed over.
fn registry_with_claimed_probe() -> (Arc<LearnedCapabilityRegistry>, Instant) {
    let reg = registry();
    let t0 = Instant::now();
    reg.observe(
        "nick",
        &probe_key(),
        PROVIDER,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        t0,
    );
    // Lapse the decay so the read admits a re-probe and latches `in_flight`.
    let t_probe = t0 + Duration::from_hours(1) + Duration::from_secs(1);
    assert_eq!(
        reg.acting_negative_for("nick", &probe_key(), PROVIDER, t_probe),
        RoutingDecision::ProbeAdmitted,
        "the fixture must hold a claimed probe slot",
    );
    (reg, t_probe)
}

/// An admission granted under `generation`.
fn admission_at(generation: u64) -> ProbeAdmission {
    ProbeAdmission {
        state_key: "nick".to_string(),
        feature: probe_key(),
        provider_kind: PROVIDER,
        generation,
    }
}

/// Whether the negative is still resident.
fn resident(reg: &LearnedCapabilityRegistry) -> bool {
    !reg.snapshot().is_empty()
}

/// A SUCCESS settlement arriving after a reload must clear nothing and emit no
/// cleared event -- the probe proved something about a catalog revision the
/// daemon has left.
#[test]
fn a_stale_success_settlement_clears_nothing_and_emits_nothing() {
    let (reg, _t) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(admitted_generation)],
        "complete",
    );

    // The reload lands between admission and settlement.
    advance_without_pruning(&reg);

    let cleared = guard.settle_success();

    assert!(
        cleared.is_empty(),
        "a stale settlement must emit no cleared event",
    );
    assert!(
        resident(&reg),
        "a stale success must not clear the live entry",
    );
}

/// The positive control: with NO reload in between, the same settlement clears
/// the entry and emits its event. Without this the test above would pass against
/// a guard that never settles anything.
#[test]
fn a_live_success_settlement_clears_and_emits() {
    let (reg, _t) = registry_with_claimed_probe();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(reg.generation())],
        "complete",
    );

    let cleared = guard.settle_success();

    assert_eq!(cleared.len(), 1, "a live success emits one cleared event");
    assert!(!resident(&reg), "and clears the entry");
}

/// A same-capability REJECTION settlement arriving after a reload must not
/// refresh the entry's backoff.
#[test]
fn a_stale_rejection_settlement_does_not_refresh_backoff() {
    let (reg, _t) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(admitted_generation)],
        "complete",
    );
    let expires_before = reg.snapshot()[0].expires_at;

    advance_without_pruning(&reg);
    let matched = guard.settle_same_capability("nick", &probe_key(), PROVIDER);

    assert_eq!(
        matched,
        SameCapabilitySettlement::Stale,
        "the guard recognized its admission and refused it as stale",
    );
    assert_eq!(
        reg.snapshot()[0].expires_at,
        expires_before,
        "a stale rejection must not extend the entry's backoff",
    );
}

/// Positive control for the rejection arm.
#[test]
fn a_live_rejection_settlement_refreshes_backoff() {
    let (reg, _t) = registry_with_claimed_probe();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(reg.generation())],
        "complete",
    );
    let expires_before = reg.snapshot()[0].expires_at;

    let matched = guard.settle_same_capability("nick", &probe_key(), PROVIDER);

    assert_eq!(matched, SameCapabilitySettlement::Applied);
    assert!(
        reg.snapshot()[0].expires_at > expires_before,
        "a live rejection refreshes the backoff",
    );
}

/// A guard DROPPED after a reload records no transient error: the in-flight slot
/// belongs to a generation that no longer exists.
#[test]
fn a_stale_drop_records_no_transient_error() {
    let (reg, t_probe) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    {
        let _guard = LearnedProbeGuard::armed(
            Arc::clone(&reg),
            vec![admission_at(admitted_generation)],
            "complete",
        );
        advance_without_pruning(&reg);
    }

    // A recorded OtherError bumps the consecutive-failed-probe counter, which
    // lengthens the next backoff. A stale drop must leave it alone.
    let entry = &reg.snapshot()[0];
    assert_eq!(
        entry.observations, 1,
        "a stale drop must not record an outcome against the live entry",
    );
    // And the entry is still available to re-probe on the live generation.
    assert!(matches!(
        reg.acting_negative_for(
            "nick",
            "web_search",
            PROVIDER,
            t_probe + Duration::from_secs(1)
        ),
        RoutingDecision::ProbeAdmitted | RoutingDecision::RouteAway { .. },
    ));
}

/// An UNREACHED admission whose set drops after a reload likewise records
/// nothing.
#[test]
fn a_stale_unreached_admission_set_drop_records_nothing() {
    let (reg, _t) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    {
        let _set = ProbeAdmissionSet::new(
            Arc::clone(&reg),
            vec![admission_at(admitted_generation)],
            "complete",
        );
        advance_without_pruning(&reg);
    }

    assert_eq!(
        reg.snapshot()[0].observations,
        1,
        "a stale unreached settlement must record nothing",
    );
}

/// The count_tokens walk settles its own probe, and must apply the same rule.
#[test]
fn a_stale_count_tokens_settlement_is_inert() {
    let (reg, t_probe) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();

    advance_without_pruning(&reg);

    // The count_tokens site settles directly through the generation-aware API.
    let settled = reg.record_probe_outcome_in_generation(
        admitted_generation,
        "nick",
        &probe_key(),
        PROVIDER,
        ProbeOutcome::Success,
        t_probe,
    );

    assert_eq!(
        settled,
        crate::learned_capability::GenerationOutcome::Stale,
        "a stale count_tokens settlement must be refused",
    );
    assert!(resident(&reg), "and must not clear the live entry");
}

/// A STALE same-capability settlement leaves every downstream consequence
/// unchanged, while still releasing its admission.
///
/// The retired boolean returned `true` for both applied and stale, so the
/// production caller bumped the probe-failure metric, recorded the cross-lane
/// F1Seen marker, and inserted the request-local dedupe key -- three observable
/// effects of a settlement that recorded nothing. The admission is released in
/// both cases (the slot is request-local; leaking it latches the pair forever),
/// which is precisely why release cannot stand in for "something happened".
#[test]
fn a_stale_same_capability_settlement_reports_stale_and_records_nothing() {
    let (reg, _t) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(admitted_generation)],
        "complete",
    );
    let expires_before = reg.snapshot()[0].expires_at;

    advance_without_pruning(&reg);
    let settled = guard.settle_same_capability("nick", &probe_key(), PROVIDER);

    assert_eq!(
        settled,
        SameCapabilitySettlement::Stale,
        "a stale settlement must be reported distinctly from an applied one",
    );
    assert_eq!(
        reg.snapshot()[0].expires_at,
        expires_before,
        "and must not refresh the backoff it did not book",
    );
    // The admission WAS released: a second settlement finds nothing held, which
    // is how the slot avoids latching.
    assert_eq!(
        guard.settle_same_capability("nick", &probe_key(), PROVIDER),
        SameCapabilitySettlement::NoMatch,
        "the stale settlement must still have released its admission",
    );
}

/// The live positive control: an applied settlement reports `Applied` and
/// refreshes the backoff, so the test above cannot pass against a guard that
/// settles nothing at all.
#[test]
fn a_live_same_capability_settlement_reports_applied() {
    let (reg, _t) = registry_with_claimed_probe();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(reg.generation())],
        "complete",
    );
    let expires_before = reg.snapshot()[0].expires_at;

    let settled = guard.settle_same_capability("nick", &probe_key(), PROVIDER);

    assert_eq!(settled, SameCapabilitySettlement::Applied);
    assert!(
        reg.snapshot()[0].expires_at > expires_before,
        "an applied settlement refreshes the backoff",
    );
}

/// A rejection for a capability this guard never held is `NoMatch`, so it falls
/// through to the ordinary observe path rather than being treated as a
/// settlement.
#[test]
fn a_rejection_for_an_unheld_capability_reports_no_match() {
    let (reg, _t) = registry_with_claimed_probe();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(reg.generation())],
        "complete",
    );

    assert_eq!(
        guard.settle_same_capability("nick", "computer_use", PROVIDER),
        SameCapabilitySettlement::NoMatch,
    );
}
