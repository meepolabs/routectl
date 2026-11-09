//! A probe guard settles against the generation it was ADMITTED under.
//!
//! A re-probe is admitted, the request dispatches, and the settlement lands
//! later. A reload can complete in between. No key is catalog-scoped, so the
//! settlement still applies to the live entry: a reload between admission and
//! settlement changes nothing the probe proved.

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

/// The capability key the guard fixtures probe: a non-field key.
fn probe_key() -> String {
    "web_search".to_string()
}

/// A reload landing between admission and settlement.
fn advance_generation(reg: &LearnedCapabilityRegistry) {
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
        crate::learned_capability::LearnKey {
            state_key: "nick",
            feature_key_raw: &probe_key(),
            provider_kind: PROVIDER,
        },
        crate::learned_capability::NegativeEvidence {
            tier: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            evidence_class: None,
        },
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
        learned_key: "nick".to_string(),
        feature: probe_key(),
        provider_kind: PROVIDER,
        generation,
    }
}

/// Whether the negative is still resident.
fn resident(reg: &LearnedCapabilityRegistry) -> bool {
    !reg.snapshot().is_empty()
}

/// A SUCCESS settlement arriving after a reload clears the live entry and emits
/// its cleared event.
#[test]
fn a_success_settlement_across_a_reload_clears_and_emits() {
    let (reg, _t) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(admitted_generation)],
        "complete",
    );

    advance_generation(&reg);

    let cleared = guard.settle_success();

    assert_eq!(cleared.len(), 1, "the settlement emits one cleared event");
    assert!(!resident(&reg), "and clears the entry");
}

/// With NO reload in between, the same settlement clears the entry and emits
/// its event.
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

/// A same-capability REJECTION settlement arriving after a reload refreshes the
/// entry's backoff.
#[test]
fn a_rejection_settlement_across_a_reload_refreshes_backoff() {
    let (reg, _t) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(admitted_generation)],
        "complete",
    );
    let expires_before = reg.snapshot()[0].expires_at;

    advance_generation(&reg);
    let matched = guard.settle_same_capability("nick", &probe_key(), PROVIDER);

    assert_eq!(matched, SameCapabilitySettlement::Applied);
    assert!(
        reg.snapshot()[0].expires_at > expires_before,
        "the rejection refreshes the backoff",
    );
}

/// The rejection arm with no reload in between.
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

/// A guard DROPPED after a reload releases the in-flight slot and records no
/// observation: a drop is an OtherError, which only frees the slot.
#[test]
fn a_drop_across_a_reload_frees_the_slot_and_records_no_observation() {
    let (reg, t_probe) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    {
        let _guard = LearnedProbeGuard::armed(
            Arc::clone(&reg),
            vec![admission_at(admitted_generation)],
            "complete",
        );
        advance_generation(&reg);
    }

    assert_eq!(reg.snapshot()[0].observations, 1);
    assert_eq!(
        reg.acting_negative_for(
            "nick",
            "web_search",
            PROVIDER,
            t_probe + Duration::from_secs(1)
        ),
        RoutingDecision::ProbeAdmitted,
        "the drop released the in-flight slot",
    );
}

/// An UNREACHED admission whose set drops after a reload likewise frees the
/// slot and records no observation.
#[test]
fn an_unreached_admission_set_drop_across_a_reload_frees_the_slot() {
    let (reg, t_probe) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    {
        let _set = ProbeAdmissionSet::new(
            Arc::clone(&reg),
            vec![admission_at(admitted_generation)],
            "complete",
        );
        advance_generation(&reg);
    }

    assert_eq!(reg.snapshot()[0].observations, 1);
    assert_eq!(
        reg.acting_negative_for(
            "nick",
            "web_search",
            PROVIDER,
            t_probe + Duration::from_secs(1)
        ),
        RoutingDecision::ProbeAdmitted,
        "the drop released the in-flight slot",
    );
}

/// The count_tokens walk settles its own probe through the generation-aware API,
/// and a reload in between does not refuse it.
#[test]
fn a_count_tokens_settlement_across_a_reload_applies() {
    let (reg, t_probe) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();

    advance_generation(&reg);

    let settled = reg.record_probe_outcome_in_generation(
        admitted_generation,
        "nick",
        &probe_key(),
        PROVIDER,
        ProbeOutcome::Success,
        t_probe,
    );

    assert!(
        matches!(
            settled,
            crate::learned_capability::GenerationOutcome::Applied { .. }
        ),
        "got {settled:?}",
    );
    assert!(!resident(&reg), "the success clears the live entry");
}

/// A same-capability settlement across a reload reports Applied and releases its
/// admission, so a second settlement finds nothing held.
#[test]
fn a_same_capability_settlement_across_a_reload_applies_and_releases() {
    let (reg, _t) = registry_with_claimed_probe();
    let admitted_generation = reg.generation();
    let mut guard = LearnedProbeGuard::armed(
        Arc::clone(&reg),
        vec![admission_at(admitted_generation)],
        "complete",
    );

    advance_generation(&reg);
    let settled = guard.settle_same_capability("nick", &probe_key(), PROVIDER);

    assert_eq!(settled, SameCapabilitySettlement::Applied);
    assert_eq!(
        guard.settle_same_capability("nick", &probe_key(), PROVIDER),
        SameCapabilitySettlement::NoMatch,
        "the settlement released its admission",
    );
}

/// With no reload in between, an applied settlement reports `Applied` and
/// refreshes the backoff.
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
