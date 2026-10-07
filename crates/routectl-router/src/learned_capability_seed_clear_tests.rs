//! Tests for the seed-clear markers: which clears record one, which learned
//! verdicts lift one, and how long one lives.

use super::*;
use crate::beta_capability::beta_capability_key;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

const DECAY: Duration = Duration::from_hours(48);
const WINDOW: Duration = Duration::from_hours(1);
const LANE: &str = "bed#upstream";
const KIND: &str = "bedrock";

/// Every flag a test here marks. `zz-flag-*` fill flags stay unseeded.
const SEED: &[&str] = &[
    "fx-a",
    "fx-kept",
    "fx-purged",
    "fx-acting",
    "fx-pending",
    "fx-positive",
    "fx-lifted",
];

fn registry() -> LearnedCapabilityRegistry {
    let reg = LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES);
    reg.set_seed_scope(BetaSeedScope::new(KIND, SEED));
    reg
}

fn beta(flag: &str) -> String {
    beta_capability_key(flag).expect("well-formed flag")
}

fn learn(reg: &LearnedCapabilityRegistry, key: &str, tier: SignalTier, at: Instant) {
    reg.observe(
        LANE,
        key,
        KIND,
        tier,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        at,
    );
}

fn marked(reg: &LearnedCapabilityRegistry, key: &str) -> bool {
    reg.seed_cleared(LANE, key, KIND)
}

#[test]
fn a_probe_settled_success_marks_a_beta_key_and_no_other() {
    // Arrange -- lapsed negatives so the probe slot is claimable.
    let reg = registry();
    let base = Instant::now();
    learn(&reg, &beta("fx-a"), SignalTier::SelfIdentifying, base);
    learn(&reg, "web_search", SignalTier::SelfIdentifying, base);

    // Act
    for key in [beta("fx-a"), "web_search".to_string()] {
        let settled = reg.record_probe_outcome_in_generation(
            reg.generation(),
            LANE,
            &key,
            KIND,
            ProbeOutcome::Success,
            base + DECAY,
        );
        assert!(settled.value().is_some(), "premise: {key} settles");
    }

    // Assert
    assert!(marked(&reg, &beta("fx-a")));
    assert!(!marked(&reg, "web_search"));
    assert!(reg.snapshot().is_empty(), "both negatives cleared");
}

#[test]
fn a_finalized_purge_marks_its_beta_key_and_a_restored_one_does_not() {
    // Arrange
    let reg = registry();
    let now = Instant::now();
    learn(&reg, &beta("fx-kept"), SignalTier::SelfIdentifying, now);
    learn(&reg, &beta("fx-purged"), SignalTier::SelfIdentifying, now);
    let reserve = |key: &str| match reg.prepare_purge(reg.generation(), LANE, key, KIND) {
        PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected a reservation for {key}, got {other:?}"),
    };

    // Act
    reg.restore_purge(reserve(&beta("fx-kept")));
    assert!(reg.finalize_purge(reserve(&beta("fx-purged"))));

    // Assert
    assert!(!marked(&reg, &beta("fx-kept")));
    assert!(marked(&reg, &beta("fx-purged")));
}

#[test]
fn an_acting_negative_lifts_the_marker_and_a_pending_or_positive_does_not() {
    // Arrange -- three marked cells.
    let reg = registry();
    let now = Instant::now();
    for flag in ["fx-acting", "fx-pending", "fx-positive"] {
        assert!(!reg.replay_cleared(LANE, &beta(flag), KIND));
        assert!(marked(&reg, &beta(flag)), "premise: {flag} marked");
    }

    // Act
    learn(&reg, &beta("fx-acting"), SignalTier::SelfIdentifying, now);
    learn(&reg, &beta("fx-pending"), SignalTier::Inferred, now);
    let _ = reg.observe_accepted_beta_in_generation(
        reg.generation(),
        LANE,
        &beta("fx-positive"),
        KIND,
        now,
    );

    // Assert
    assert!(!marked(&reg, &beta("fx-acting")));
    assert!(marked(&reg, &beta("fx-pending")));
    assert!(marked(&reg, &beta("fx-positive")));
}

#[test]
fn a_corroborated_inferred_negative_lifts_the_marker() {
    let reg = registry();
    let now = Instant::now();
    let _ = reg.replay_cleared(LANE, &beta("fx-a"), KIND);

    learn(&reg, &beta("fx-a"), SignalTier::Inferred, now);
    learn(&reg, &beta("fx-a"), SignalTier::Inferred, now + secs(1));

    assert!(!marked(&reg, &beta("fx-a")));
}

#[test]
fn markers_sit_outside_the_lane_cap_and_are_never_its_victim() {
    // Arrange -- a marker on a cell with no entry, then a full lane of betas.
    let reg = registry();
    let base = Instant::now();
    let _ = reg.replay_cleared(LANE, &beta("fx-lifted"), KIND);
    let fill = MAX_BETA_ENTRIES_PER_LANE + 4;

    // Act
    for n in 0..fill {
        learn(
            &reg,
            &beta(&format!("zz-flag-{n}")),
            SignalTier::SelfIdentifying,
            base + secs(n),
        );
    }

    // Assert -- the lane holds exactly its bound of entries, and the marker
    // took no slot and survived every eviction.
    assert_eq!(reg.snapshot().len(), MAX_BETA_ENTRIES_PER_LANE);
    assert!(marked(&reg, &beta("fx-lifted")));
}

#[test]
fn a_revision_boundary_prunes_markers() {
    // Arrange
    let reg = registry();
    let _ = reg.replay_cleared(LANE, &beta("fx-a"), KIND);
    let BoundaryCut::Taken { receipt, .. } = reg.with_boundary_cut(|_, _| (), |()| true) else {
        panic!("the boundary must be admitted");
    };
    assert!(
        marked(&reg, &beta("fx-a")),
        "admission alone prunes nothing"
    );

    // Act
    let settled = reg.commit_boundary_transition(&receipt);

    // Assert
    assert!(matches!(settled, BoundarySettlement::Applied { .. }));
    assert!(!marked(&reg, &beta("fx-a")));
}

#[test]
fn a_rolled_back_boundary_keeps_markers() {
    let reg = registry();
    let _ = reg.replay_cleared(LANE, &beta("fx-a"), KIND);
    let BoundaryCut::Taken { receipt, .. } = reg.with_boundary_cut(|_, _| (), |()| true) else {
        panic!("the boundary must be admitted");
    };

    let _ = reg.rollback_pending_generation(&receipt);

    assert!(marked(&reg, &beta("fx-a")));
}

#[test]
fn a_marker_is_scoped_to_its_lane_and_kind_and_normalized_like_an_entry() {
    // Arrange
    let reg = registry();
    let _ = reg.replay_cleared(LANE, &beta("fx-a"), KIND);

    // Assert
    assert!(marked(&reg, &beta("fx-a")));
    assert!(!reg.seed_cleared("other#upstream", &beta("fx-a"), KIND));
    assert!(!reg.seed_cleared(LANE, &beta("fx-a"), "anthropic-api"));
    assert_eq!(
        reg.seed_clear_snapshot(),
        vec![SeedClearMarker {
            state_key: LANE.to_string(),
            provider_kind: KIND.to_string(),
            feature_key: normalize_capability_key(&beta("fx-a"), KIND),
        }],
    );
}

fn secs(n: usize) -> Duration {
    Duration::from_secs(u64::try_from(n).expect("small test index"))
}

#[test]
fn only_a_seeded_cell_is_marked() {
    // Arrange
    let reg = registry();

    // Act -- a thousand distinct unseeded flags, one seeded flag, and the
    // seeded flag under another kind.
    for n in 0..1000 {
        let _ = reg.replay_cleared(LANE, &beta(&format!("zz-unseeded-{n}")), KIND);
    }
    let _ = reg.replay_cleared(LANE, &beta("fx-a"), KIND);
    let _ = reg.replay_cleared(LANE, &beta("fx-a"), "anthropic-api");

    // Assert
    assert_eq!(reg.seed_clear_snapshot().len(), 1);
    assert!(marked(&reg, &beta("fx-a")));
}

#[test]
fn a_registry_with_no_seed_scope_marks_nothing() {
    let reg = LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES);

    let _ = reg.replay_cleared(LANE, &beta("fx-a"), KIND);

    assert!(reg.seed_clear_snapshot().is_empty());
}

fn prepare(reg: &LearnedCapabilityRegistry, key: &str) -> PurgePreparation {
    reg.prepare_purge(reg.generation(), LANE, key, KIND)
}

#[test]
fn an_unmarked_seeded_cell_with_nothing_resident_reserves_a_seed_lift() {
    // Arrange
    let reg = registry();

    // Act
    let prepared = prepare(&reg, &beta("fx-a"));

    // Assert
    let PurgePreparation::SeedLift(lease) = prepared else {
        panic!("expected a seed lift, got {prepared:?}");
    };
    assert_eq!(lease.incarnation(), 0, "nothing resident to supersede");
    assert_eq!(lease.generation(), reg.generation());
    assert!(lease.captured_entry().is_none());
    assert!(
        !marked(&reg, &beta("fx-a")),
        "reserving records no marker before the clear commits",
    );
    reg.restore_purge(lease);
}

#[test]
fn a_seed_lift_is_refused_when_nothing_is_left_to_lift() {
    // Arrange
    let reg = registry();
    let _ = reg.replay_cleared(LANE, &beta("fx-kept"), KIND);
    let rows = [
        ("an already-cleared seeded cell", beta("fx-kept"), KIND),
        ("an unseeded beta flag", beta("zz-unseeded"), KIND),
        (
            "a seeded flag under another kind",
            beta("fx-a"),
            "anthropic-api",
        ),
        ("a non-beta key", "web_search".to_string(), KIND),
    ];

    for (name, key, kind) in rows {
        // Act
        let prepared = reg.prepare_purge(reg.generation(), LANE, &key, kind);

        // Assert
        assert!(
            matches!(prepared, PurgePreparation::Absent),
            "{name}: expected absent, got {prepared:?}",
        );
    }
}

#[test]
fn a_finalized_seed_lift_marks_the_cell_and_an_abandoned_one_does_not() {
    // Arrange
    let reg = registry();
    let lift = |flag: &str| match prepare(&reg, &beta(flag)) {
        PurgePreparation::SeedLift(lease) => lease,
        other => panic!("expected a seed lift for {flag}, got {other:?}"),
    };

    // Act
    reg.restore_purge(lift("fx-kept"));
    reg.finalize_seed_lift(lift("fx-purged"));

    // Assert
    assert!(!marked(&reg, &beta("fx-kept")));
    assert!(marked(&reg, &beta("fx-purged")));
    assert!(reg.snapshot().is_empty(), "a lift creates no entry");
    assert!(
        matches!(prepare(&reg, &beta("fx-purged")), PurgePreparation::Absent),
        "a lifted cell has nothing left to lift",
    );
    assert!(
        matches!(
            prepare(&reg, &beta("fx-kept")),
            PurgePreparation::SeedLift(_)
        ),
        "an abandoned lift released its lease",
    );
}

#[test]
fn a_leased_seed_lift_refuses_a_second_lift_and_a_boundary_cut() {
    // Arrange
    let reg = registry();
    let PurgePreparation::SeedLift(held) = prepare(&reg, &beta("fx-a")) else {
        panic!("premise: the first lift reserves");
    };

    // Act
    let second = prepare(&reg, &beta("fx-a"));
    let cut = reg.with_boundary_cut(|_, _| (), |()| true);

    // Assert
    assert!(matches!(second, PurgePreparation::Busy), "{second:?}");
    assert!(
        !matches!(cut, BoundaryCut::Taken { .. }),
        "an open lift lease refuses the boundary cut, as a learned purge's does",
    );
    reg.restore_purge(held);
}

#[test]
fn an_admitted_unsettled_boundary_refuses_a_seed_lift_as_busy() {
    // Arrange
    let reg = registry();
    let BoundaryCut::Taken { receipt, .. } = reg.with_boundary_cut(|_, _| (), |()| true) else {
        panic!("the boundary must be admitted");
    };

    // Act
    let prepared = prepare(&reg, &beta("fx-a"));

    // Assert
    assert!(matches!(prepared, PurgePreparation::Busy), "{prepared:?}");
    let _ = reg.rollback_pending_generation(&receipt);
}
