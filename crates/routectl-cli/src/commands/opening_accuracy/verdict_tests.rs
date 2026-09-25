//! The folded verdict and its exit status: the exact rate over the anchored
//! cohort, then completeness (legacy), data quality and backend provenance.

use serde_json::json;

use super::test_fixture::*;
use super::*;
use crate::handlers::opening_diagnostics::key;

fn verdict_of(rows: &[Row]) -> (Numerical, Verdict) {
    let report = report_of(rows);
    (report.numerical(), report.verdict())
}

#[test]
fn nineteen_of_twenty_on_direct_evidence_is_a_clean_pass_and_exits_zero() {
    // Act
    let (numerical, verdict) = verdict_of(&anchored_rows(19, 1, true));

    // Assert
    assert_eq!(
        numerical,
        Numerical::Pass {
            pass: 19,
            eligible: 20
        }
    );
    assert_eq!(verdict, Verdict::Pass);
    assert_eq!(verdict.exit_code(), 0);
}

#[test]
fn eighteen_of_twenty_fails() {
    // Act
    let (numerical, verdict) = verdict_of(&anchored_rows(18, 2, true));

    // Assert
    assert_eq!(
        numerical,
        Numerical::Fail {
            pass: 18,
            eligible: 20
        }
    );
    assert_eq!(verdict, Verdict::Fail);
    assert_ne!(verdict.exit_code(), 0);
}

#[test]
fn eighteen_clean_plus_one_malformed_known_anchor_does_not_pass() {
    // Arrange: the malformed marker keeps the row in the cohort as a miss.
    let mut rows = anchored_rows(18, 0, true);
    rows.push(Row::ok(with(
        &anchored(1_000, 1_000, true),
        key::OPENING_PRESENT,
        Some(json!("yes")),
    )));

    // Act
    let (numerical, verdict) = verdict_of(&rows);

    // Assert
    assert_eq!(
        numerical,
        Numerical::Fail {
            pass: 18,
            eligible: 19
        }
    );
    assert_eq!(verdict, Verdict::Fail);
}

#[test]
fn a_malformed_known_raw_row_keeps_the_rate_but_makes_the_result_indeterminate() {
    // Arrange
    let mut rows = anchored_rows(19, 1, true);
    let raw = opened(RAW, Some(1_000), explicit(1_000, true));
    rows.push(Row::ok(with(&raw, key::OPENING_INPUT, Some(json!("1000")))));

    // Act
    let (numerical, verdict) = verdict_of(&rows);

    // Assert
    assert_eq!(
        numerical,
        Numerical::Pass {
            pass: 19,
            eligible: 20
        }
    );
    assert_eq!(verdict, Verdict::Indeterminate);
    assert_ne!(verdict.exit_code(), 0);
}

#[test]
fn an_unclassifiable_row_vetoes_without_entering_the_denominator() {
    // Arrange
    let mut rows = anchored_rows(19, 1, true);
    rows.push(Row::ok("{".to_string()));

    // Act
    let (numerical, verdict) = verdict_of(&rows);

    // Assert
    assert_eq!(
        numerical,
        Numerical::Pass {
            pass: 19,
            eligible: 20
        }
    );
    assert_eq!(verdict, Verdict::Indeterminate);
}

#[test]
fn a_legacy_row_in_the_window_makes_a_numerical_pass_insufficient() {
    // Arrange
    let mut rows = anchored_rows(19, 1, true);
    rows.push(Row::anthropic(Some(LANE_A), "ok", None));

    // Act
    let (numerical, verdict) = verdict_of(&rows);

    // Assert
    assert_eq!(
        numerical,
        Numerical::Pass {
            pass: 19,
            eligible: 20
        }
    );
    assert_eq!(verdict, Verdict::Insufficient);
    assert_ne!(verdict.exit_code(), 0);
}

#[test]
fn a_numerical_pass_on_relayed_terminals_is_backend_pending() {
    // Act
    let (numerical, verdict) = verdict_of(&anchored_rows(19, 1, false));

    // Assert
    assert_eq!(
        numerical,
        Numerical::Pass {
            pass: 19,
            eligible: 20
        }
    );
    assert_eq!(verdict, Verdict::BackendPending);
    assert_ne!(verdict.exit_code(), 0);
}

#[test]
fn one_relayed_report_on_a_failing_anchored_row_still_holds_the_pass_pending() {
    // Arrange: every pass is direct; only the miss was relayed.
    let mut rows = anchored_rows(19, 0, true);
    rows.push(Row::ok(anchored(2_000, 1_000, false)));

    // Act
    let (_, verdict) = verdict_of(&rows);

    // Assert
    assert_eq!(verdict, Verdict::BackendPending);
}

#[test]
fn zero_anchored_rows_are_insufficient_never_pass() {
    let non_anchored = vec![
        Row::ok(opened(RAW, Some(1_000), explicit(1_000, true))),
        Row::ok(no_opening()),
    ];
    for rows in [vec![], non_anchored, vec![Row::ok("{".to_string())]] {
        // Act
        let (numerical, verdict) = verdict_of(&rows);

        // Assert
        assert_eq!(numerical, Numerical::Insufficient);
        assert_eq!(verdict, Verdict::Insufficient);
        assert_ne!(verdict.exit_code(), 0);
    }
}

#[test]
fn only_a_clean_pass_exits_zero_and_every_other_verdict_is_distinct() {
    let all = [
        Verdict::Pass,
        Verdict::BackendPending,
        Verdict::Fail,
        Verdict::Insufficient,
        Verdict::Indeterminate,
        Verdict::NoLedger,
    ];
    for v in all {
        assert_eq!(v.exit_code() == 0, v == Verdict::Pass, "{v:?}");
    }
    let tokens: std::collections::BTreeSet<&str> = all.iter().map(|v| v.as_str()).collect();
    assert_eq!(tokens.len(), all.len());
}
