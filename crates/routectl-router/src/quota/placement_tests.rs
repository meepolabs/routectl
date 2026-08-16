//! Tests for the placement partition.
//!
//! Each of the four arms fails differently and three of them fail SILENTLY:
//! preferring an unknown seat over a known-empty one throws away the strongest
//! signal the feature has, failing an all-capped pool denies a request a soft
//! cap was never meant to deny, and treating a mixed pool as decidable lets an
//! unknown reading move a placement -- the exact inversion the fail-closed rule
//! exists to prevent. None of those surfaces as an error, so an arm per test is
//! the only guard.

use super::*;

use std::time::{Duration, Instant, SystemTime};

use crate::quota::curation::{ANTHROPIC_PROVIDER_KIND, CODEX_PROVIDER_KIND, RESET_TOLERANCE};
use crate::quota::freshness::accept_reset;
use crate::quota::key::seat_key_for_secret_ref;
use crate::quota::reduce::QuotaSnapshot;
use crate::quota::window::{Billing, Utilization};

const SEAT_PROVIDER: &str = "anthropic";
const FAST_WINDOW: Duration = Duration::from_hours(5);

/// The curated Anthropic FAST cap, read from the table rather than restated:
/// a test asserting "below cap" against a hardcoded 0.5 would go vacuous the
/// day the curated threshold moves.
fn fast_cap() -> f64 {
    crate::quota::curation::row_for(ANTHROPIC_PROVIDER_KIND, &WindowRole::Fast)
        .expect("anthropic curates a fast window")
        .threshold
}

fn base_wall() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_000_000)
}

fn stamp_at(offset: Duration) -> ObservationStamp {
    ObservationStamp::from_parts(base_wall() + offset, Instant::now() + offset)
}

fn seat(label: Option<&str>) -> SeatKey {
    let secret_ref = routectl_auth::SecretRef::OAuth {
        provider: SEAT_PROVIDER.to_string(),
        label: label.map(str::to_string),
    };
    seat_key_for_secret_ref(Some(&secret_ref)).expect("an oauth ref yields a key")
}

/// A `Known` FAST window at `fraction`, resetting inside its own window.
fn known_fast(fraction: f64, observed: &ObservationStamp) -> QuotaWindow {
    QuotaWindow::Known {
        utilization: Utilization::new(fraction).expect("a valid fraction"),
        reset_at: accept_reset(
            observed.wall() + FAST_WINDOW / 2,
            observed,
            FAST_WINDOW,
            RESET_TOLERANCE,
        )
        .expect("a reset inside the window is accepted"),
    }
}

/// A store holding one FAST reading per named seat.
fn store_with(readings: &[(Option<&str>, f64)]) -> QuotaStore {
    let store = QuotaStore::default();
    store.admit_seats([seat(None), seat(Some("b")), seat(Some("c"))]);
    let observed = stamp_at(Duration::ZERO);
    for (label, fraction) in readings {
        store.observe(
            &seat(*label),
            QuotaSnapshot {
                observed,
                fast: known_fast(*fraction, &observed),
                slow: QuotaWindow::Unknown,
                billing: Billing::Unknown,
            },
        );
    }
    store
}

fn keys(labels: &[Option<&str>]) -> Vec<Option<SeatKey>> {
    labels.iter().map(|label| Some(seat(*label))).collect()
}

/// Run the partition over `quota` with every seat eligible.
fn restrict(quota: &[SeatQuota]) -> (Option<Vec<usize>>, QuotaDecision) {
    let eligible: Vec<usize> = (0..quota.len()).collect();
    let mut decision = QuotaDecision::Dormant;
    let restricted = restrict_by_quota(&eligible, quota, &mut decision);
    (restricted, decision)
}

// ---- tier classification ----

#[test]
fn a_reading_below_the_curated_cap_reads_below_cap() {
    // Arrange
    let store = store_with(&[(None, 0.10)]);

    // Act
    let tiers = seat_tiers(
        &store,
        &keys(&[None]),
        Some(ANTHROPIC_PROVIDER_KIND),
        &stamp_at(Duration::ZERO),
    );

    // Assert: remaining is the unspent fraction of the WINDOW.
    assert_eq!(tiers, vec![SeatQuota::BelowCap { remaining: 0.90 }]);
}

#[test]
fn a_reading_at_the_curated_cap_reads_at_cap() {
    // Arrange: exactly at the threshold, which the partition treats as capped
    // -- the cap engages AT the threshold, not past it.
    let store = store_with(&[(None, fast_cap())]);

    // Act
    let tiers = seat_tiers(
        &store,
        &keys(&[None]),
        Some(ANTHROPIC_PROVIDER_KIND),
        &stamp_at(Duration::ZERO),
    );

    // Assert
    assert_eq!(
        tiers,
        vec![SeatQuota::AtCap {
            remaining: 1.0 - fast_cap()
        }]
    );
}

#[test]
fn a_seat_with_no_reading_reads_unknown() {
    let store = store_with(&[]);

    let tiers = seat_tiers(
        &store,
        &keys(&[None, Some("b")]),
        Some(ANTHROPIC_PROVIDER_KIND),
        &stamp_at(Duration::ZERO),
    );

    assert_eq!(tiers, vec![SeatQuota::Unknown, SeatQuota::Unknown]);
}

#[test]
fn a_seat_with_no_account_identity_reads_unknown() {
    // A seat on a non-OAuth credential mints no store key, so there is no
    // account budget to read. It must not borrow a sibling's tier.
    let store = store_with(&[(None, 0.10)]);

    let tiers = seat_tiers(
        &store,
        &[None, Some(seat(None))],
        Some(ANTHROPIC_PROVIDER_KIND),
        &stamp_at(Duration::ZERO),
    );

    assert_eq!(
        tiers,
        vec![SeatQuota::Unknown, SeatQuota::BelowCap { remaining: 0.90 }]
    );
}

#[test]
fn a_provider_curating_no_fast_window_is_dormant() {
    // Codex reports one seven-day window and no short recovering one, so it
    // yields NO tiers at all rather than a pool of unknowns -- the dormancy is
    // in the curated table, not in a branch here.
    let store = store_with(&[(None, 0.10)]);

    let tiers = seat_tiers(
        &store,
        &keys(&[None]),
        Some(CODEX_PROVIDER_KIND),
        &stamp_at(Duration::ZERO),
    );

    assert!(tiers.is_empty());
}

#[test]
fn an_uncurated_provider_is_dormant() {
    let store = store_with(&[(None, 0.10)]);

    assert!(
        seat_tiers(
            &store,
            &keys(&[None]),
            Some("gemini"),
            &stamp_at(Duration::ZERO)
        )
        .is_empty()
    );
    assert!(seat_tiers(&store, &keys(&[None]), None, &stamp_at(Duration::ZERO)).is_empty());
}

#[test]
fn a_window_expired_at_read_time_reads_unknown_not_its_last_value() {
    // The millis-as-seconds defense's PLACEMENT half in its general form: a
    // lapsed reading must read as no evidence, never as its last (low) value,
    // or a stale seat keeps attracting every new session.
    let store = store_with(&[(None, 0.10)]);

    // Act: read past the window's own reset.
    let tiers = seat_tiers(
        &store,
        &keys(&[None]),
        Some(ANTHROPIC_PROVIDER_KIND),
        &stamp_at(FAST_WINDOW),
    );

    // Assert
    assert_eq!(tiers, vec![SeatQuota::Unknown]);
}

#[test]
fn an_implausible_reset_leaves_placement_treating_the_seat_as_unknown() {
    // The millis-as-seconds fixture's placement half. A real captured reset
    // multiplied by 1000 is refused at normalization, so NO known window
    // enters the store -- and placement must therefore see Unknown, NOT a seat
    // permanently fresh at low utilization, which would attract every new
    // session forever.
    let store = QuotaStore::default();
    store.admit_seats([seat(None)]);
    let observed = stamp_at(Duration::ZERO);
    // A real captured 5h reset, in epoch SECONDS, misread as milliseconds.
    let captured_reset_secs: u64 = 1_781_001_000;
    let millis_scale_reset =
        SystemTime::UNIX_EPOCH + Duration::from_secs(captured_reset_secs * 1000);
    assert!(
        accept_reset(millis_scale_reset, &observed, FAST_WINDOW, RESET_TOLERANCE).is_err(),
        "a millis-scale reset must be refused before any window is built"
    );
    store.observe(
        &seat(None),
        QuotaSnapshot {
            observed,
            // What the reducer produces from that input: cap-dormant, with the
            // low utilization discarded alongside the unusable reset.
            fast: QuotaWindow::Unknown,
            slow: QuotaWindow::Unknown,
            billing: Billing::Unknown,
        },
    );

    // Act
    let tiers = seat_tiers(
        &store,
        &keys(&[None]),
        Some(ANTHROPIC_PROVIDER_KIND),
        &observed,
    );

    // Assert: unknown, and therefore never preferred by the partition below.
    assert_eq!(tiers, vec![SeatQuota::Unknown]);
    let (restricted, decision) = restrict(&tiers);
    assert!(restricted.is_none());
    assert_eq!(decision, QuotaDecision::AllUnknownFallback);
}

// ---- the four partition arms ----

#[test]
fn below_cap_tier_wins_and_takes_the_most_remaining() {
    // Arrange: two below-cap seats and one capped one.
    let quota = vec![
        SeatQuota::BelowCap { remaining: 0.7 },
        SeatQuota::BelowCap { remaining: 0.95 },
        SeatQuota::AtCap { remaining: 0.2 },
    ];

    // Act
    let (restricted, decision) = restrict(&quota);

    // Assert: restricted to the emptiest below-cap seat.
    assert_eq!(restricted, Some(vec![1]));
    assert_eq!(decision, QuotaDecision::BelowCapTier);
}

#[test]
fn a_known_empty_seat_beats_an_unknown_one_and_unknown_never_competes() {
    // THE ARM THAT MAKES THE PARTITION WORTH ITS COMPLEXITY. A seat known to
    // be at 0.0 -- the best target there is -- must beat a seat with no
    // reading. Under any scoring scheme the unknown seat needs a number, and
    // every number either beats known-zero (discarding the strongest signal)
    // or loses to a genuinely exhausted seat.
    let quota = vec![
        SeatQuota::Unknown,
        SeatQuota::BelowCap { remaining: 1.0 },
        SeatQuota::Unknown,
    ];

    let (restricted, decision) = restrict(&quota);

    assert_eq!(restricted, Some(vec![1]));
    assert_eq!(decision, QuotaDecision::BelowCapTier);
}

#[test]
fn all_known_and_all_capped_takes_the_most_remaining_and_never_fails() {
    // Arrange: every eligible seat over its cap. The cap is SOFT, so this
    // still places -- on the seat with the most left.
    let quota = vec![
        SeatQuota::AtCap { remaining: 0.05 },
        SeatQuota::AtCap { remaining: 0.30 },
        SeatQuota::AtCap { remaining: 0.10 },
    ];

    // Act
    let (restricted, decision) = restrict(&quota);

    // Assert: a pick, never a refusal.
    assert_eq!(restricted, Some(vec![1]));
    assert_eq!(decision, QuotaDecision::AllCappedMostRemaining);
}

#[test]
fn a_fully_exhausted_pool_still_places() {
    // The extreme of the arm above: every seat reports its window entirely
    // spent. There is no better seat, and there must still be a seat.
    let quota = vec![
        SeatQuota::AtCap { remaining: 0.0 },
        SeatQuota::AtCap { remaining: 0.0 },
    ];

    let (restricted, decision) = restrict(&quota);

    assert_eq!(restricted, Some(vec![0, 1]));
    assert_eq!(decision, QuotaDecision::AllCappedMostRemaining);
}

#[test]
fn mixed_capped_known_and_unknown_falls_through_cap_dormant() {
    // Arrange: no below-cap evidence anywhere, and at least one seat unknown.
    // Preferring the capped seat would act on a cap while a seat that might be
    // empty goes unconsidered; preferring the unknown one would let an absent
    // reading beat a present one. Neither is defensible, so quota decides
    // nothing and the unchanged capacity ranking stands.
    let quota = vec![SeatQuota::AtCap { remaining: 0.2 }, SeatQuota::Unknown];

    // Act
    let (restricted, decision) = restrict(&quota);

    // Assert
    assert!(restricted.is_none());
    assert_eq!(decision, QuotaDecision::MixedUnknownFallback);
}

#[test]
fn all_unknown_falls_through_cap_dormant() {
    let quota = vec![SeatQuota::Unknown, SeatQuota::Unknown];

    let (restricted, decision) = restrict(&quota);

    assert!(restricted.is_none());
    assert_eq!(decision, QuotaDecision::AllUnknownFallback);
}

#[test]
fn no_tiers_at_all_is_dormant() {
    // What the kill switch OFF produces, and what an uncurated provider
    // produces. Distinguished from the fall-through arms because a dormant
    // pick is not an event: it must emit nothing.
    let (restricted, decision) = restrict(&[]);

    assert!(restricted.is_none());
    assert_eq!(decision, QuotaDecision::Dormant);
    assert!(!decision.placed());
}

#[test]
fn only_the_eligible_set_is_considered() {
    // The dispatchability filter and the health preference run BEFORE this, so
    // a seat they excluded must not be reachable however good its reading --
    // otherwise a parked-but-empty seat would win a placement it cannot serve.
    let quota = vec![
        SeatQuota::BelowCap { remaining: 1.0 },
        SeatQuota::BelowCap { remaining: 0.6 },
    ];
    let mut decision = QuotaDecision::Dormant;

    let restricted = restrict_by_quota(&[1], &quota, &mut decision);

    assert_eq!(restricted, Some(vec![1]));
    assert_eq!(decision, QuotaDecision::BelowCapTier);
}

#[test]
fn ties_within_a_tier_are_left_for_the_callers_tiebreak() {
    // The partition narrows; it does not break ties. Returning one seat here
    // would silently replace the anti-herd rotation, so a burst of new
    // conversations would herd onto the emptiest seat -- the herding the
    // rotation exists to prevent.
    let quota = vec![
        SeatQuota::BelowCap { remaining: 0.8 },
        SeatQuota::BelowCap { remaining: 0.8 },
        SeatQuota::BelowCap { remaining: 0.3 },
    ];

    let (restricted, decision) = restrict(&quota);

    assert_eq!(restricted, Some(vec![0, 1]));
    assert_eq!(decision, QuotaDecision::BelowCapTier);
}

#[test]
fn a_quota_slice_shorter_than_the_pool_reads_the_missing_seats_as_unknown() {
    // A length mismatch is a wiring bug. It must fall through rather than
    // panic on a request-serving path or index a neighbour's reading.
    let quota = vec![SeatQuota::AtCap { remaining: 0.2 }];
    let mut decision = QuotaDecision::Dormant;

    let restricted = restrict_by_quota(&[0, 1, 2], &quota, &mut decision);

    assert!(restricted.is_none());
    assert_eq!(decision, QuotaDecision::MixedUnknownFallback);
}

#[test]
fn an_empty_eligible_set_is_dormant() {
    // No dispatchable seat at all: the caller's existing no-healthy behavior
    // owns that case, and quota must not claim it.
    let quota = vec![SeatQuota::BelowCap { remaining: 1.0 }];
    let mut decision = QuotaDecision::AllUnknownFallback;

    let restricted = restrict_by_quota(&[], &quota, &mut decision);

    assert!(restricted.is_none());
    assert_eq!(decision, QuotaDecision::Dormant);
}

#[test]
fn only_the_deciding_arms_report_as_having_placed() {
    assert!(QuotaDecision::BelowCapTier.placed());
    assert!(QuotaDecision::AllCappedMostRemaining.placed());
    assert!(!QuotaDecision::MixedUnknownFallback.placed());
    assert!(!QuotaDecision::AllUnknownFallback.placed());
    assert!(!QuotaDecision::Dormant.placed());
}
