//! Tests for the lane store: the ratio conversion, the zero-pair refusal, the
//! per-lane cap, lane separation, and the carry-over round trip.

use super::*;

use std::time::Duration;

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
}

fn key(kind: &str, nickname: &str) -> LaneKey {
    LaneKey {
        provider_kind: kind.into(),
        nickname: nickname.into(),
    }
}

/// The ratios a store holds for a lane, in arrival order.
fn ratios(store: &CalibrationStore, key: &LaneKey) -> Vec<u32> {
    store
        .export_entries()
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, samples)| samples.iter().map(|s| s.permille).collect())
        .unwrap_or_default()
}

#[test]
fn a_recorded_pair_becomes_the_actual_over_estimate_ratio_in_permille() {
    // Arrange
    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");

    // Act: the upstream counted 25% more than the estimate predicted.
    store.record(k.clone(), 10_000, 12_500, 1, now());

    // Assert: the ratio is actual/estimate, so above identity.
    assert_eq!(ratios(&store, &k), vec![1_250]);
}

#[test]
fn an_over_counting_estimate_records_a_ratio_below_identity() {
    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");

    store.record(k.clone(), 10_000, 8_000, 1, now());

    assert_eq!(ratios(&store, &k), vec![800]);
}

#[test]
fn a_zero_on_either_side_of_the_pair_is_not_recorded() {
    // A zero estimate has no ratio at all; a zero actual is an upstream that
    // reported nothing, and admitting it would drag the lane's correction
    // toward zero -- the direction that makes the gate admit oversized
    // requests.
    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");

    store.record(k.clone(), 0, 10_000, 1, now());
    store.record(k.clone(), 10_000, 0, 1, now());

    assert!(
        store.is_empty(),
        "neither degenerate pair may create a lane"
    );
}

#[test]
fn the_per_lane_ring_caps_and_drops_the_oldest() {
    // The cap is the memory bound: the per-process floor is the declared lane
    // count times this, whatever the traffic shape.
    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");

    for i in 0..(SAMPLES_PER_LANE + 5) {
        // A distinct ratio per sample so the drop is observable.
        store.record(k.clone(), 1_000, 1_000 + i as u64, 1, now());
    }

    let held = ratios(&store, &k);
    assert_eq!(held.len(), SAMPLES_PER_LANE);
    assert_eq!(
        held.first(),
        Some(&1_005),
        "the five oldest samples were dropped",
    );
}

#[test]
fn each_key_component_separates_lanes() {
    // Both halves of the key are load-bearing: two models on one provider
    // kind, and one model served through two provider kinds, must not bleed
    // corrections onto each other.
    let store = CalibrationStore::default();
    store.record(key("anthropic-api", "opus"), 1_000, 1_100, 1, now());
    store.record(key("anthropic-api", "sonnet"), 1_000, 1_900, 1, now());
    store.record(key("bedrock", "opus"), 1_000, 700, 1, now());

    assert_eq!(store.len(), 3);
    assert_eq!(ratios(&store, &key("anthropic-api", "opus")), vec![1_100]);
    assert_eq!(ratios(&store, &key("anthropic-api", "sonnet")), vec![1_900]);
    assert_eq!(ratios(&store, &key("bedrock", "opus")), vec![700]);
}

#[test]
fn an_unseen_lane_has_no_factor() {
    let store = CalibrationStore::default();
    store.record(key("anthropic-api", "opus"), 1_000, 1_100, 1, now());

    assert_eq!(
        store.factor_for(&key("anthropic-api", "sonnet"), now()),
        None
    );
}

#[test]
fn a_lane_with_enough_balanced_evidence_yields_its_factor() {
    // Arrange: three cohorts, nine samples, one consistent ratio.
    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");
    for i in 0..9 {
        store.record(k.clone(), 10_000, 12_000, i % 3, now());
    }

    // Act
    let factor = store
        .factor_for(&k, now())
        .expect("evidence clears the floors");

    // Assert
    assert_eq!(factor.permille(), 1_200);
}

#[test]
fn retain_lanes_drops_a_lane_the_predicate_refuses() {
    // Arrange: two lanes with different evidence.
    let store = CalibrationStore::default();
    for i in 0..4 {
        store.record(key("anthropic-api", "kept"), 1_000, 1_100 + i, i, now());
    }
    store.record(key("bedrock", "retired"), 1_000, 900, 7, now());
    assert_eq!(store.len(), 2);

    // Act: the predicate admits only the "kept" nickname, mirroring the
    // carry-over's `knows_nickname` check.
    store.retain_lanes(|k| k.nickname == "kept");

    // Assert
    let surviving: Vec<String> = store
        .export_entries()
        .into_iter()
        .map(|(k, _)| k.nickname)
        .collect();
    assert_eq!(surviving, vec!["kept".to_string()]);
}

#[test]
fn an_absurd_pair_is_refused_rather_than_truncated_into_the_band() {
    // A ratio that cannot fit the permille type must produce no sample at
    // all: a truncated value could land inside the sane band and quietly
    // become a correction.
    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");

    store.record(k.clone(), 1, u64::MAX, 1, now());

    assert!(store.is_empty());
}

#[test]
fn record_reports_whether_the_pair_was_stored() {
    // The rebuild tallies its drops from this return value, which is what
    // keeps both paths sharing ONE admission rule instead of each deciding.
    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");

    assert!(store.record(k.clone(), 10_000, 12_500, 1, now()));
    assert!(!store.record(k.clone(), 0, 12_500, 1, now()));
    assert!(!store.record(k.clone(), 10_000, 0, 1, now()));
    assert!(!store.record(k, 1, u64::MAX, 1, now()));
}

#[test]
fn the_calibrated_lane_count_only_counts_lanes_the_gate_would_correct() {
    // The count runs the SAME reduction the gate's lookup runs, so there is
    // no second notion of "calibrated" to drift from it.
    let store = CalibrationStore::default();
    let calibrated = key("anthropic-api", "opus");
    let thin = key("anthropic-api", "sonnet");
    for i in 0..9 {
        store.record(calibrated.clone(), 10_000, 12_000, i % 3, now());
    }
    // Nine samples from ONE cohort: clears the sample floor, misses the
    // distinct-cohort one.
    for _ in 0..9 {
        store.record(thin.clone(), 10_000, 12_000, 7, now());
    }

    assert_eq!(store.len(), 2);
    assert_eq!(store.calibrated_lane_count(now()), 1);
}

#[test]
fn a_lane_whose_evidence_aged_out_stops_counting_as_calibrated() {
    // Freshness is judged against the clock passed in, so the same store
    // reports differently as time moves past the age bound.
    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");
    for i in 0..9 {
        store.record(k.clone(), 10_000, 12_000, i % 3, now());
    }

    assert_eq!(store.calibrated_lane_count(now()), 1);
    assert_eq!(
        store.calibrated_lane_count(now() + Duration::from_hours(48)),
        0
    );
}

#[test]
fn every_keyless_caller_shares_one_cohort_so_such_a_lane_never_calibrates() {
    // The shared cohort derivation: a keyless request gets tag zero, so a
    // lane fed only keyless traffic can never clear the distinct-cohort
    // floor. Live writes and the ledger rebuild call the SAME function, so a
    // caller cannot count as two cohorts across a restart.
    assert_eq!(cohort_of(None), 0);
    assert_eq!(cohort_of(Some("caller")), cohort_of(Some("caller")));
    assert_ne!(cohort_of(Some("caller-a")), cohort_of(Some("caller-b")));

    let store = CalibrationStore::default();
    let k = key("anthropic-api", "opus");
    for _ in 0..9 {
        store.record(k.clone(), 10_000, 12_000, cohort_of(None), now());
    }

    assert_eq!(store.calibrated_lane_count(now()), 0);
}
