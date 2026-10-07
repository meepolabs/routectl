//! Tests for the per-lane beta-flag bound: a lane spraying beta flags replaces
//! its own oldest beta entry instead of evicting anything else.

use super::*;
use crate::beta_capability::beta_capability_key;
use routectl_core::capability::{BETA_ACCEPTED, EvidenceSource, FailurePhase, SignalTier};

const DECAY: Duration = Duration::from_hours(48);
const WINDOW: Duration = Duration::from_hours(1);
const LANE: &str = "nick#upstream";
const OTHER_LANE: &str = "other#upstream";
const KIND: &str = "anthropic-api";

fn registry() -> LearnedCapabilityRegistry {
    LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES)
}

fn beta(n: usize) -> String {
    beta_capability_key(&format!("zz-flag-{n}")).expect("well-formed flag")
}

fn learn(reg: &LearnedCapabilityRegistry, lane: &str, kind: &str, key: &str, at: Instant) {
    reg.observe(
        lane,
        key,
        kind,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        at,
    );
}

/// Learn `count` beta negatives on `lane`, the n-th one `n` seconds after `base`.
fn fill_betas(
    reg: &LearnedCapabilityRegistry,
    lane: &str,
    kind: &str,
    count: usize,
    base: Instant,
) {
    for n in 0..count {
        learn(reg, lane, kind, &beta(n), base + secs(n));
    }
}

fn secs(n: usize) -> Duration {
    Duration::from_secs(u64::try_from(n).expect("small test index"))
}

fn resident(reg: &LearnedCapabilityRegistry, lane: &str, kind: &str) -> Vec<String> {
    reg.recorded_snapshot()
        .into_iter()
        .filter(|e| e.entry.state_key == lane && e.provider_kind == kind)
        .map(|e| e.entry.feature_key)
        .collect()
}

fn beta_count(reg: &LearnedCapabilityRegistry, lane: &str, kind: &str) -> usize {
    resident(reg, lane, kind)
        .iter()
        .filter(|k| capability_key_is_beta(k))
        .count()
}

#[test]
fn the_beta_past_the_lane_bound_evicts_the_lanes_oldest_beta_and_warns() {
    // Arrange
    let reg = registry();
    let base = Instant::now();
    fill_betas(&reg, LANE, KIND, MAX_BETA_ENTRIES_PER_LANE, base);

    // Act
    let events = routectl_testkit::capture_events(|| {
        learn(
            &reg,
            LANE,
            KIND,
            &beta(MAX_BETA_ENTRIES_PER_LANE),
            base + secs(100),
        );
    });

    // Assert
    let keys = resident(&reg, LANE, KIND);
    assert_eq!(keys.len(), MAX_BETA_ENTRIES_PER_LANE);
    assert!(!keys.contains(&beta(0)), "the oldest beta must be evicted");
    assert!(keys.contains(&beta(1)));
    assert!(keys.contains(&beta(MAX_BETA_ENTRIES_PER_LANE)));
    let warn = events
        .iter()
        .find(|e| e.level == tracing::Level::WARN && e.field("event") == Some("evict"))
        .expect("the lane eviction must emit an evict WARN");
    assert_eq!(warn.field("reason"), Some("beta_lane_cap"));
    assert_eq!(warn.field("capability_key"), Some(beta(0).as_str()));
    assert_eq!(warn.field("state_key"), Some(LANE));
    assert_eq!(
        warn.field("max_beta_entries_per_lane"),
        Some(MAX_BETA_ENTRIES_PER_LANE.to_string().as_str())
    );
}

#[test]
fn the_lane_bound_leaves_non_beta_facts_and_other_lanes_and_kinds_untouched() {
    // Arrange -- the oldest entries on the lane are non-beta, and other
    // lanes / kinds carry older betas than any on the filled lane.
    let reg = registry();
    let base = Instant::now();
    learn(&reg, LANE, KIND, "web_search", base);
    learn(&reg, OTHER_LANE, KIND, &beta(0), base);
    learn(&reg, LANE, "bedrock", &beta(0), base);
    fill_betas(&reg, LANE, KIND, MAX_BETA_ENTRIES_PER_LANE, base + secs(10));

    // Act
    learn(
        &reg,
        LANE,
        KIND,
        &beta(MAX_BETA_ENTRIES_PER_LANE),
        base + secs(100),
    );

    // Assert
    let keys = resident(&reg, LANE, KIND);
    assert!(
        keys.contains(&"web_search".to_string()),
        "non-beta fact survives"
    );
    assert_eq!(beta_count(&reg, LANE, KIND), MAX_BETA_ENTRIES_PER_LANE);
    assert!(
        !keys.contains(&beta(0)),
        "the lane's own oldest beta is the victim"
    );
    assert_eq!(resident(&reg, OTHER_LANE, KIND), vec![beta(0)]);
    assert_eq!(resident(&reg, LANE, "bedrock"), vec![beta(0)]);
}

#[test]
fn a_leased_beta_is_never_the_lane_victim() {
    // Arrange -- the oldest beta is held by a purge lease.
    let reg = registry();
    let base = Instant::now();
    fill_betas(&reg, LANE, KIND, MAX_BETA_ENTRIES_PER_LANE, base);
    let lease = match reg.prepare_purge(reg.generation(), LANE, &beta(0), KIND) {
        PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected a reservation, got {other:?}"),
    };

    // Act
    learn(
        &reg,
        LANE,
        KIND,
        &beta(MAX_BETA_ENTRIES_PER_LANE),
        base + secs(100),
    );

    // Assert
    let keys = resident(&reg, LANE, KIND);
    assert!(
        keys.contains(&beta(0)),
        "the leased entry must stay resident"
    );
    assert!(
        !keys.contains(&beta(1)),
        "the oldest unleased beta is the victim"
    );
    assert_eq!(keys.len(), MAX_BETA_ENTRIES_PER_LANE);
    reg.restore_purge(lease);
}

#[test]
fn refreshing_a_resident_beta_at_the_bound_evicts_nothing() {
    // Arrange
    let reg = registry();
    let base = Instant::now();
    fill_betas(&reg, LANE, KIND, MAX_BETA_ENTRIES_PER_LANE, base);

    // Act
    let events = routectl_testkit::capture_events(|| {
        learn(&reg, LANE, KIND, &beta(5), base + secs(100));
    });

    // Assert
    assert_eq!(beta_count(&reg, LANE, KIND), MAX_BETA_ENTRIES_PER_LANE);
    assert!(resident(&reg, LANE, KIND).contains(&beta(0)));
    assert!(
        !events.iter().any(|e| e.field("event") == Some("evict")),
        "a refresh must not evict"
    );
}

#[test]
fn a_non_beta_insert_on_a_full_beta_lane_evicts_nothing() {
    // Arrange
    let reg = registry();
    let base = Instant::now();
    fill_betas(&reg, LANE, KIND, MAX_BETA_ENTRIES_PER_LANE, base);

    // Act
    learn(&reg, LANE, KIND, "web_search", base + secs(100));

    // Assert
    assert_eq!(beta_count(&reg, LANE, KIND), MAX_BETA_ENTRIES_PER_LANE);
    assert_eq!(
        resident(&reg, LANE, KIND).len(),
        MAX_BETA_ENTRIES_PER_LANE + 1
    );
}

#[test]
fn non_beta_inserts_at_the_global_cap_still_evict_the_oldest_entry() {
    // Arrange -- global cap of 2.
    let reg = LearnedCapabilityRegistry::new(DECAY, WINDOW, 2);
    let base = Instant::now();
    learn(&reg, LANE, KIND, "cap_a", base);
    learn(&reg, OTHER_LANE, KIND, "cap_b", base + secs(1));

    // Act
    let events = routectl_testkit::capture_events(|| {
        learn(&reg, LANE, KIND, "cap_c", base + secs(2));
    });

    // Assert
    let keys: Vec<String> = reg.snapshot().into_iter().map(|e| e.feature_key).collect();
    assert_eq!(keys.len(), 2);
    assert!(!keys.contains(&"cap_a".to_string()));
    let warn = events
        .iter()
        .find(|e| e.field("event") == Some("evict"))
        .expect("the global eviction must emit an evict WARN");
    assert_eq!(warn.field("reason"), Some("registry_cap"));
    assert_eq!(warn.field("capability_key"), Some("cap_a"));
}

#[test]
fn an_accepted_beta_positive_is_bounded_per_lane() {
    // Arrange
    let reg = registry();
    let base = Instant::now();
    fill_betas(&reg, LANE, KIND, MAX_BETA_ENTRIES_PER_LANE, base);

    // Act
    let outcome = reg.observe_accepted_beta_in_generation(
        reg.generation(),
        LANE,
        &beta(MAX_BETA_ENTRIES_PER_LANE),
        KIND,
        base + secs(100),
    );

    // Assert
    assert!(matches!(outcome.value(), Some(PositiveOutcome::Recorded)));
    let keys = resident(&reg, LANE, KIND);
    assert_eq!(keys.len(), MAX_BETA_ENTRIES_PER_LANE);
    assert!(!keys.contains(&beta(0)));
    assert!(keys.contains(&beta(MAX_BETA_ENTRIES_PER_LANE)));
}

#[test]
fn an_accepted_beta_replacing_its_own_negative_evicts_nothing() {
    // Arrange
    let reg = registry();
    let base = Instant::now();
    fill_betas(&reg, LANE, KIND, MAX_BETA_ENTRIES_PER_LANE, base);

    // Act
    let _ = reg.replay_accepted_beta(
        LANE,
        &beta(7),
        KIND,
        EvidenceSource::Probe,
        base + secs(100),
    );

    // Assert
    let keys = resident(&reg, LANE, KIND);
    assert_eq!(keys.len(), MAX_BETA_ENTRIES_PER_LANE);
    assert!(keys.contains(&beta(0)));
    assert!(reg.is_verified_working(LANE, &beta(7), KIND, base + secs(101)));
}

#[test]
fn a_replayed_accepted_beta_is_bounded_per_lane() {
    // Arrange
    let reg = registry();
    let base = Instant::now();
    fill_betas(&reg, LANE, KIND, MAX_BETA_ENTRIES_PER_LANE, base);

    // Act
    let _ = reg.replay_accepted_beta(
        LANE,
        &beta(MAX_BETA_ENTRIES_PER_LANE),
        KIND,
        EvidenceSource::Live,
        base + secs(100),
    );

    // Assert
    let keys = resident(&reg, LANE, KIND);
    assert_eq!(keys.len(), MAX_BETA_ENTRIES_PER_LANE);
    assert!(!keys.contains(&beta(0)));
    let accepted = reg
        .export_entries()
        .into_iter()
        .find(|e| e.feature_key == beta(MAX_BETA_ENTRIES_PER_LANE))
        .expect("the accepted beta is resident");
    assert_eq!(accepted.evidence_class.as_deref(), Some(BETA_ACCEPTED));
}

#[test]
fn an_imported_carry_over_is_bounded_per_lane() {
    // Arrange -- a carry-over holding one more beta than the bound.
    let source = LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES);
    let base = Instant::now();
    for n in 0..=MAX_BETA_ENTRIES_PER_LANE {
        // Distinct lanes in the source so its own bound never fires.
        learn(
            &source,
            &format!("src{n}#upstream"),
            KIND,
            &beta(n),
            base + secs(n),
        );
    }
    let carried: Vec<ExportedEntry> = source
        .export_entries()
        .into_iter()
        .map(|e| ExportedEntry {
            state_key: LANE.to_string(),
            ..e
        })
        .collect();
    let reg = registry();

    // Act
    reg.import_entries(carried);

    // Assert
    let keys = resident(&reg, LANE, KIND);
    assert_eq!(keys.len(), MAX_BETA_ENTRIES_PER_LANE);
    assert!(!keys.contains(&beta(0)));
}
