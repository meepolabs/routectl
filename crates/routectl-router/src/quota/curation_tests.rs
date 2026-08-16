//! Self-tests over the curated table.
//!
//! These check properties of the TABLE rather than of any one lookup, which is
//! the reason the five facts share one row: a duplicate role, a threshold off
//! its scale, a zero duration and a tolerance wider than the window it guards
//! are each a mismatch BETWEEN columns, and none of them is visible from a
//! single field. The Codex-has-no-FAST-row assertion is here for the same
//! reason -- it is the absence of a row, so only a test over the whole table
//! can pin it.

use super::*;

/// Every row, for the whole-table invariants.
fn all_rows() -> Vec<&'static CuratedWindow> {
    rows_for(ANTHROPIC_PROVIDER_KIND)
        .chain(rows_for(CODEX_PROVIDER_KIND))
        .collect()
}

#[test]
fn no_provider_curates_two_rows_in_one_role() {
    for kind in [ANTHROPIC_PROVIDER_KIND, CODEX_PROVIDER_KIND] {
        for role in [WindowRole::Fast, WindowRole::Slow] {
            let matching = rows_for(kind).filter(|row| row.role == role).count();

            assert!(
                matching <= 1,
                "{kind} curates {matching} rows for {role:?}; a role must name at most one window \
                 or a lookup silently picks whichever comes first"
            );
        }
    }
}

#[test]
fn no_provider_curates_two_rows_for_one_source_window() {
    for kind in [ANTHROPIC_PROVIDER_KIND, CODEX_PROVIDER_KIND] {
        let ids: Vec<&str> = rows_for(kind).map(|row| row.source_id).collect();

        for id in &ids {
            assert_eq!(
                ids.iter().filter(|other| *other == id).count(),
                1,
                "{kind} curates the window {id} more than once"
            );
        }
    }
}

#[test]
fn every_threshold_sits_inside_the_utilization_scale() {
    for row in all_rows() {
        assert!(
            row.threshold.is_finite() && row.threshold > 0.0 && row.threshold <= 1.0,
            "{} {} has threshold {}, which is not a usable fraction of a window",
            row.provider_kind,
            row.source_id,
            row.threshold
        );
    }
}

#[test]
fn no_window_has_a_zero_duration() {
    for row in all_rows() {
        assert!(
            !row.duration.is_zero(),
            "{} {} declares a zero-length window, so every reported reset would be \
             implausible and the window could never be read",
            row.provider_kind,
            row.source_id
        );
    }
}

#[test]
fn the_reset_tolerance_is_narrower_than_every_window_it_guards() {
    for row in all_rows() {
        assert!(
            RESET_TOLERANCE < row.duration,
            "the tolerance ({:?}) is not narrower than {} {} ({:?}); a tolerance at or above a \
             window's own length readmits a reset belonging to a LONGER window, which is the \
             class the plausibility bound exists to refuse",
            RESET_TOLERANCE,
            row.provider_kind,
            row.source_id,
            row.duration
        );
    }
}

#[test]
fn anthropic_curates_the_five_hour_window_fast_and_the_seven_day_window_slow() {
    let fast = row_for(ANTHROPIC_PROVIDER_KIND, &WindowRole::Fast).expect("a curated FAST row");
    let slow = row_for(ANTHROPIC_PROVIDER_KIND, &WindowRole::Slow).expect("a curated SLOW row");

    assert_eq!(fast.source_id, "5h");
    assert_eq!(fast.duration, Duration::from_hours(5));
    assert_eq!(slow.source_id, "7d");
    assert_eq!(slow.duration, Duration::from_hours(24 * 7));
}

#[test]
fn codex_curates_only_a_slow_window_so_its_fast_cap_is_dormant_by_construction() {
    let slow = row_for(CODEX_PROVIDER_KIND, &WindowRole::Slow).expect("a curated SLOW row");

    assert_eq!(
        slow.source_id, "primary",
        "the captured Codex window is the one the upstream calls primary"
    );
    assert_eq!(
        slow.duration,
        Duration::from_hours(24 * 7),
        "the captured primary window declares 10080 minutes, which is SLOW however it is named"
    );
    assert!(
        row_for(CODEX_PROVIDER_KIND, &WindowRole::Fast).is_none(),
        "Codex must curate NO fast window: the only captured evidence reports a seven-day \
         primary and an unused secondary, so there is no short recovering window to cap. This \
         absence is intended behavior -- the fast cap stays dormant -- and inventing a role \
         from the name primary would manufacture a placement signal from a word"
    );
}

#[test]
fn an_uncurated_provider_kind_yields_no_rows_and_no_role() {
    assert_eq!(rows_for("gemini").count(), 0);
    assert!(row_for("gemini", &WindowRole::Fast).is_none());
    assert!(row_for("gemini", &WindowRole::Slow).is_none());
    assert_eq!(
        rows_for("").count(),
        0,
        "an empty provider kind must not match a row by accident"
    );
}
