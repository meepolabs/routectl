//! Unit contract for the per-request repair ceiling: the allowance is
//! spent exactly `REPAIRS_PER_REQUEST` times and then refuses forever,
//! independent of how many targets a chain walk offers it.

use super::{REPAIRS_PER_REQUEST, RepairBudget};

#[test]
fn a_fresh_budget_grants_exactly_the_per_request_allowance() {
    // Arrange
    let mut budget = RepairBudget::per_request();

    // Act
    let granted = (0..u32::from(REPAIRS_PER_REQUEST))
        .filter(|_| budget.draw())
        .count();

    // Assert
    assert_eq!(
        granted,
        usize::from(REPAIRS_PER_REQUEST),
        "every draw within the allowance is granted",
    );
}

#[test]
fn a_draw_past_the_allowance_is_refused_and_stays_refused() {
    // Arrange -- allowance already spent.
    let mut budget = RepairBudget::per_request();
    for _ in 0..REPAIRS_PER_REQUEST {
        assert!(budget.draw());
    }

    // Act
    let after = [budget.draw(), budget.draw(), budget.draw()];

    // Assert -- exhaustion is terminal, never wrapping back around.
    assert_eq!(
        after,
        [false, false, false],
        "an exhausted request budget refuses every later repair",
    );
}

#[test]
fn the_allowance_is_five_repairs_per_logical_request() {
    // The ceiling is a forever-visible operator-facing bound: a change
    // to it changes how many upstream calls one client request can cost,
    // so it is pinned here rather than left to drift silently.
    assert_eq!(REPAIRS_PER_REQUEST, 5);
}

/// A refused draw or availability check marks the budget once; reading the
/// mark clears it, and a granted draw never sets it.
///
/// Mutation checks: drop the mark in `draw` -> red on `a refused draw`; drop it
/// in `check_or_mark_refusal` -> red on `a refused check`; set it on a granted draw -> red
/// on `granted`.
#[test]
fn only_a_refusal_marks_the_budget_and_reading_the_mark_clears_it() {
    let mut budget = RepairBudget::per_request();
    assert!(budget.check_or_mark_refusal() && budget.draw());
    assert!(!budget.take_refusal(), "granted: no mark");
    for _ in 1..REPAIRS_PER_REQUEST {
        assert!(budget.draw());
    }

    assert!(!budget.draw());
    assert!(budget.take_refusal(), "a refused draw marks the budget");
    assert!(!budget.take_refusal(), "reading the mark clears it");
    assert!(!budget.check_or_mark_refusal());
    assert!(budget.take_refusal(), "a refused check marks the budget");
}

/// `spent` counts the draws granted so far and stops at the allowance.
#[test]
fn spent_counts_the_granted_draws() {
    let mut budget = RepairBudget::per_request();
    assert_eq!(budget.spent(), 0);
    budget.draw();
    budget.draw();
    assert_eq!(budget.spent(), 2);
    for _ in 0..REPAIRS_PER_REQUEST {
        budget.draw();
    }
    assert_eq!(budget.spent(), REPAIRS_PER_REQUEST);
}
