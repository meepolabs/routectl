//! Purges must not lease a key still owned by an admitted learned re-probe.

#[test]
fn pending_boundary_rollback_preserves_probe_settlement_ownership() {
    for pending_caller in [false, true] {
        for settlement in [0, 1, 2, 3] {
            let reg = registry();
            let now = Instant::now();
            reg.observe(
                "nick",
                &probe_key(),
                PROVIDER,
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                now,
            );
            let receipt = match reg.with_boundary_cut(|_, _| true, |admitted| *admitted) {
                BoundaryCut::Taken { receipt, .. } => receipt,
                other => panic!("boundary admission failed: {other:?}"),
            };
            let caller = if pending_caller {
                receipt.generation()
            } else {
                reg.generation()
            };
            let (decision, owner) = reg
                .acting_negative_in_generation(
                    caller,
                    "nick",
                    &probe_key(),
                    PROVIDER,
                    now + Duration::from_hours(2),
                )
                .unwrap();
            assert_eq!(decision, RoutingDecision::ProbeAdmitted);
            assert_eq!(owner, reg.generation());
            assert!(matches!(
                reg.rollback_pending_generation(&receipt),
                crate::learned_capability::BoundarySettlement::Applied { .. }
            ));
            let admissions = vec![admission_at(owner)];
            match settlement {
                0 => drop(LearnedProbeGuard::armed(
                    reg.clone(),
                    admissions,
                    "complete",
                )),
                1 => drop(ProbeAdmissionSet::new(reg.clone(), admissions, "stream")),
                2 => {
                    let mut guard = LearnedProbeGuard::armed(reg.clone(), admissions, "complete");
                    assert_eq!(guard.settle_success().len(), 1);
                    assert!(!resident(&reg));
                }
                _ => assert!(matches!(
                    reg.record_probe_outcome_in_generation(
                        owner,
                        "nick",
                        &probe_key(),
                        PROVIDER,
                        ProbeOutcome::OtherError,
                        now,
                    ),
                    GenerationOutcome::Applied { .. }
                )),
            }
            if settlement != 2 {
                assert_reprobe(&reg, now + Duration::from_hours(2));
                assert!(matches!(prepare(&reg), PurgePreparation::Reserved(_)));
            }
        }
    }
}

use super::*;
use crate::learned_capability::{BoundaryCut, GenerationOutcome, PurgeLease, PurgePreparation};

fn prepare(reg: &LearnedCapabilityRegistry) -> PurgePreparation {
    reg.prepare_purge(reg.generation(), "nick", &probe_key(), PROVIDER)
}

fn reserve(reg: &LearnedCapabilityRegistry) -> PurgeLease {
    match prepare(reg) {
        PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected an unowned key to reserve: {other:?}"),
    }
}

fn assert_busy_and_claimed(reg: &LearnedCapabilityRegistry) {
    assert!(matches!(prepare(reg), PurgePreparation::Busy));
    assert!(
        reg.export_entries()[0].in_flight,
        "refusal must not steal the probe slot"
    );
}

fn assert_reprobe(reg: &LearnedCapabilityRegistry, now: Instant) {
    assert!(!reg.export_entries()[0].in_flight);
    assert_eq!(
        reg.acting_negative_in_generation(reg.generation(), "nick", &probe_key(), PROVIDER, now)
            .unwrap()
            .0,
        RoutingDecision::ProbeAdmitted,
    );
    reg.record_probe_outcome_in_generation(
        reg.generation(),
        "nick",
        &probe_key(),
        PROVIDER,
        ProbeOutcome::OtherError,
        now,
    );
}

#[test]
fn purge_during_reached_probe_refuses_then_guard_drop_abandon_and_reprobe_work() {
    let (reg, now) = registry_with_claimed_probe();
    let guard = LearnedProbeGuard::armed(
        reg.clone(),
        vec![admission_at(reg.generation())],
        "complete",
    );
    assert_busy_and_claimed(&reg);
    drop(guard);
    let lease = reserve(&reg);
    // Failed durable persistence: abandonment changes neither observation nor
    // decay and must not strand an in-flight bit left behind by the guard.
    let before = reg.export_entries()[0].clone();
    reg.restore_purge(lease);
    let after = reg.export_entries()[0].clone();
    assert_eq!(before.expires_at, after.expires_at);
    assert_eq!(before.observations, after.observations);
    assert_reprobe(&reg, now);
}

#[test]
fn purge_during_unreached_admission_refuses_then_set_drop_and_lease_drop_allow_reprobe() {
    let (reg, now) = registry_with_claimed_probe();
    let set = ProbeAdmissionSet::new(reg.clone(), vec![admission_at(reg.generation())], "stream");
    assert_busy_and_claimed(&reg);
    drop(set);
    let lease = reserve(&reg);
    drop(lease); // Cancelled purge: Drop releases the lease, not the entry.
    assert_reprobe(&reg, now);
}

#[test]
fn purge_cannot_intercept_successful_probe_settlement() {
    let (reg, _) = registry_with_claimed_probe();
    let mut guard =
        LearnedProbeGuard::armed(reg.clone(), vec![admission_at(reg.generation())], "stream");
    assert_busy_and_claimed(&reg);
    let clears = guard.settle_success();
    assert_eq!(clears.len(), 1);
    assert!(!resident(&reg));
    assert!(matches!(prepare(&reg), PurgePreparation::Absent));
}

#[test]
fn purge_after_rejected_probe_can_commit_and_fresh_learning_can_reprobe() {
    let (reg, _) = registry_with_claimed_probe();
    let mut guard = LearnedProbeGuard::armed(
        reg.clone(),
        vec![admission_at(reg.generation())],
        "complete",
    );
    assert_busy_and_claimed(&reg);
    assert_eq!(
        guard.settle_same_capability("nick", &probe_key(), PROVIDER),
        SameCapabilitySettlement::Applied
    );
    let entry = reg.export_entries()[0].clone();
    assert!(!entry.in_flight);
    assert_eq!(entry.consecutive_failed_probes, 1);
    let lease = reserve(&reg);
    let floor = lease.incarnation();
    assert!(reg.finalize_purge(lease)); // Caller has committed its clear.
    let now = Instant::now();
    let learned = reg.observe_in_generation(
        reg.generation(),
        "nick",
        &probe_key(),
        PROVIDER,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    assert!(
        matches!(learned, GenerationOutcome::Applied { incarnation, .. } if incarnation > floor)
    );
    assert_reprobe(&reg, now + Duration::from_hours(2));
}

#[test]
fn reload_during_claimed_probe_prunes_old_truth_without_a_purge_lease_or_latched_slot() {
    let (reg, _) = registry_with_claimed_probe();
    let guard = LearnedProbeGuard::armed(
        reg.clone(),
        vec![admission_at(reg.generation())],
        "complete",
    );
    assert_busy_and_claimed(&reg);
    let receipt = match reg.with_boundary_cut(|_, _| true, |admitted| *admitted) {
        BoundaryCut::Taken { receipt, .. } => receipt,
        other => panic!("probe ownership must not invent an open purge lease: {other:?}"),
    };
    assert!(matches!(
        reg.commit_boundary_transition(&receipt),
        crate::learned_capability::BoundarySettlement::Applied { pruned: 1, .. }
    ));
    drop(guard); // Old catalog admission is stale, not an owner of new truth.
    assert!(matches!(prepare(&reg), PurgePreparation::Absent));
    let now = Instant::now();
    reg.observe_in_generation(
        reg.generation(),
        "nick",
        &probe_key(),
        PROVIDER,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    assert_reprobe(&reg, now + Duration::from_hours(2));
}

#[test]
fn purge_first_blocks_both_guarded_and_ungated_probe_admission_until_abandonment() {
    let reg = registry();
    let now = Instant::now();
    reg.observe(
        "nick",
        &probe_key(),
        PROVIDER,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let lease = reserve(&reg);
    let lapsed = now + Duration::from_hours(2);
    assert!(
        reg.acting_negative_in_generation(reg.generation(), "nick", &probe_key(), PROVIDER, lapsed)
            .is_none()
    );
    assert!(matches!(
        reg.acting_negative_for("nick", &probe_key(), PROVIDER, lapsed),
        RoutingDecision::RouteAway { .. }
    ));
    assert!(!reg.export_entries()[0].in_flight);
    reg.restore_purge(lease);
    assert_reprobe(&reg, lapsed);
}
