//! The pace rule inside the seat chooser.
//!
//! Each test feeds standings that would flip the pick if the chooser let them
//! override a layer they must not touch: dispatchability, the Closed over
//! HalfOpenReady preference, the anti-herd rotation, a healthy pin, and a
//! migration off an unhealthy one.

use super::*;

use crate::quota::placement::{Reason, Tier};
use crate::runtime_state::CredentialState;

fn closed_with(rpm: f64) -> CapacitySnapshot {
    CapacitySnapshot {
        rpm_available: Some(rpm),
        circuit: CircuitPhase::Closed,
        credential: CredentialState::Usable,
        outage_remaining: None,
    }
}

fn open() -> CapacitySnapshot {
    CapacitySnapshot {
        rpm_available: None,
        circuit: CircuitPhase::Open,
        credential: CredentialState::Usable,
        outage_remaining: None,
    }
}

fn half_open(rpm: f64) -> CapacitySnapshot {
    CapacitySnapshot {
        circuit: CircuitPhase::HalfOpenReady,
        ..closed_with(rpm)
    }
}

fn roomy(score: f64) -> Standing {
    Standing {
        tier: Tier::Roomy,
        score: Some(score),
        reason: Reason::Pace,
    }
}

fn low_room(score: f64) -> Standing {
    Standing {
        tier: Tier::LowRoom,
        score: Some(score),
        reason: Reason::LowRoom,
    }
}

const fn unread() -> Standing {
    Standing {
        tier: Tier::Unread,
        score: None,
        reason: Reason::NoReading,
    }
}

#[test]
fn the_best_standing_supersedes_the_rpm_ranking_on_a_birth() {
    let snaps = vec![closed_with(100.0), closed_with(1.0)];
    let standings = vec![roomy(0.4), roomy(1.6)];

    let (order, outcome) = sticky_least_loaded_order(2, None, false, &snaps, 0, &standings);

    assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
    assert_eq!(order, vec![1, 0]);
}

#[test]
fn headroom_breaks_a_tie_inside_the_best_bucket_and_never_crosses_it() {
    // Seats 0 and 1 share the 1.23 bucket; seat 2 has the most headroom but
    // sits in a lower bucket and must never be chosen.
    let snaps = vec![closed_with(5.0), closed_with(9.0), closed_with(500.0)];
    let standings = vec![roomy(1.234), roomy(1.2349), roomy(1.22)];

    let (_, outcome) = sticky_least_loaded_order(3, None, false, &snaps, 0, &standings);

    assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
}

#[test]
fn the_rotation_spreads_births_across_a_tied_bucket() {
    let snaps = vec![
        closed_with(5.0),
        closed_with(5.0),
        closed_with(5.0),
        closed_with(5.0),
    ];
    let standings = vec![roomy(1.204), low_room(9.0), roomy(1.196), roomy(1.19)];

    let homes: Vec<SelectionOutcome> = (0..4)
        .map(|tiebreak| sticky_least_loaded_order(4, None, false, &snaps, tiebreak, &standings).1)
        .collect();

    assert_eq!(
        homes,
        vec![
            SelectionOutcome::Birth { home: 0 },
            SelectionOutcome::Birth { home: 2 },
            SelectionOutcome::Birth { home: 0 },
            SelectionOutcome::Birth { home: 2 },
        ]
    );
}

#[test]
fn a_non_dispatchable_seat_is_never_resurrected_by_its_standing() {
    let snaps = vec![open(), closed_with(1.0)];
    let standings = vec![roomy(9.0), low_room(0.0)];

    let (order, outcome) = sticky_least_loaded_order(2, None, false, &snaps, 0, &standings);

    assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
    assert_eq!(order, vec![1, 0]);
}

#[test]
fn the_closed_over_half_open_preference_runs_before_the_standing() {
    let snaps = vec![half_open(100.0), closed_with(3.0)];
    let standings = vec![roomy(9.0), unread()];

    let (_, outcome) = sticky_least_loaded_order(2, None, false, &snaps, 0, &standings);

    assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
}

#[test]
fn no_standings_ranks_on_headroom_exactly_as_before() {
    let snaps = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];

    for tiebreak in 0..3 {
        let dormant = sticky_least_loaded_order(3, None, false, &snaps, tiebreak, &[]);
        let all_unread =
            sticky_least_loaded_order(3, None, false, &snaps, tiebreak, &[unread(); 3]);

        assert_eq!(
            dormant,
            (vec![1, 0, 2], SelectionOutcome::Birth { home: 1 })
        );
        assert_eq!(all_unread, dormant, "tiebreak {tiebreak}");
    }
}

#[test]
fn a_healthy_pin_stays_whatever_the_standings() {
    let snaps = vec![closed_with(1.0), closed_with(1.0)];
    let standings = vec![low_room(0.0), roomy(9.0)];

    let (order, outcome) = sticky_least_loaded_order(2, Some(0), false, &snaps, 0, &standings);

    assert_eq!(outcome, SelectionOutcome::Stay { home: 0 });
    assert_eq!(order, vec![0, 1]);
}

#[test]
fn a_migration_off_an_unhealthy_home_ignores_standings() {
    let snaps = vec![open(), closed_with(1.0), closed_with(50.0)];
    let standings = vec![unread(), roomy(9.0), low_room(0.0)];

    let (order, outcome) = sticky_least_loaded_order(3, Some(0), false, &snaps, 0, &standings);

    assert_eq!(outcome, SelectionOutcome::OverflowRepin { home: 2 });
    assert_eq!(order, vec![2, 0, 1]);
}

#[test]
fn no_dispatchable_seat_defers_whatever_the_standings() {
    let snaps = vec![open(), open()];

    let (order, outcome) =
        sticky_least_loaded_order(2, None, false, &snaps, 0, &[roomy(1.0), roomy(1.0)]);

    assert_eq!(outcome, SelectionOutcome::DeferNoHealthy);
    assert_eq!(order, vec![0, 1]);
}

/// A keyless case: name, snapshots, standings, tiebreak, expected walk.
type KeylessRow = (
    &'static str,
    Vec<CapacitySnapshot>,
    Vec<Standing>,
    usize,
    Option<Vec<usize>>,
);

#[test]
fn a_keyless_walk_leads_with_the_birth_pick_and_keeps_every_seat() {
    let rows: [KeylessRow; 7] = [
        (
            "the best standing leads",
            vec![closed_with(1.0), closed_with(1.0), closed_with(1.0)],
            vec![roomy(0.3), roomy(0.95), roomy(0.4)],
            0,
            Some(vec![1, 0, 2]),
        ),
        (
            "a parked seat never leads, and still follows",
            vec![open(), closed_with(1.0)],
            vec![roomy(9.0), low_room(0.1)],
            0,
            Some(vec![1, 0]),
        ),
        (
            "a half-open seat never leads over a closed one",
            vec![half_open(100.0), closed_with(100.0)],
            vec![roomy(9.0), roomy(0.1)],
            0,
            Some(vec![1, 0]),
        ),
        (
            "the rotation spreads a tied bucket",
            vec![closed_with(1.0), closed_with(1.0)],
            vec![roomy(1.0), roomy(1.0)],
            1,
            Some(vec![1, 0]),
        ),
        (
            "an all-unread pool declines to the fill-first walk",
            vec![closed_with(1.0), closed_with(9.0)],
            vec![unread(), unread()],
            1,
            None,
        ),
        (
            "an unread eligible pool declines though a parked seat is read",
            vec![open(), closed_with(9.0)],
            vec![roomy(1.0), unread()],
            0,
            None,
        ),
        (
            "a dormant pool declines",
            vec![closed_with(1.0), closed_with(9.0)],
            vec![],
            0,
            None,
        ),
    ];
    for (name, snaps, standings, tiebreak, expected) in rows {
        assert_eq!(
            keyless_quota_order(snaps.len(), &snaps, &standings, tiebreak),
            expected,
            "row: {name}"
        );
    }
}

#[test]
fn a_keyless_walk_with_nothing_dispatchable_declines() {
    let snaps = vec![open(), open()];

    assert_eq!(
        keyless_quota_order(2, &snaps, &[roomy(1.0), roomy(1.0)], 0),
        None
    );
}
