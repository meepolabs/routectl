//! The opening-accuracy report over hand-built ledger rows: one category per
//! row, the exact anchored cohort and its non-pass reasons, field defects,
//! unverified reports, error buckets, cold start and lanes. Every expected
//! number is computed by hand from the fixture's per-row comments.

use serde_json::json;

use super::classify::{UNRECOGNIZED_LABEL, defect, non_pass, unclassifiable};
use super::test_fixture::*;
use super::*;
use crate::handlers::opening_diagnostics::{key, source, terminal};

// ------------------------------------------------------------ universe

#[test]
fn every_row_in_the_universe_lands_in_exactly_one_category() {
    // Act
    let report = report_of(&mixed_fixture());

    // Assert
    assert_eq!(report.universe, 18);
    assert_eq!(report.legacy, 2);
    assert_eq!(
        report.unclassifiable,
        counts(&[(unclassifiable::INVALID_JSON, 1)])
    );
    assert_eq!(report.no_opening, 1);
    assert_eq!(
        report.known_by_source,
        counts(&[
            (ANCHOR, 11),
            (CALIBRATED, 1),
            (RAW, 1),
            (source::UPSTREAM_WIRE_UNVERIFIED, 1),
        ])
    );
    assert_eq!(report.classified(), report.universe);
}

#[test]
fn the_anchored_cohort_is_exactly_the_anchor_rows_each_with_one_non_pass_reason() {
    // Act
    let cohort = report_of(&mixed_fixture()).anchored;

    // Assert
    assert_eq!(cohort.eligible, 11);
    assert_eq!(cohort.pass, 2);
    assert_eq!(cohort.pass_unverified, 1);
    assert_eq!(
        cohort.non_pass,
        counts(&[
            (non_pass::MARKER_INCONSISTENT, 1),
            (non_pass::OPENING_UNSTATED, 1),
            (non_pass::OUTCOME_NOT_OK, 1),
            (non_pass::OUTSIDE_5PCT, 2),
            (non_pass::TERMINAL_MALFORMED, 1),
            (non_pass::TERMINAL_MISSING, 1),
            (non_pass::TERMINAL_UNSUPPORTED, 1),
            (non_pass::TERMINAL_ZERO, 1),
        ])
    );
    assert_eq!(
        cohort.pass + cohort.non_pass.values().sum::<u64>(),
        cohort.eligible
    );
}

#[test]
fn unverified_terminal_reports_are_counted_for_every_outcome_not_only_passes() {
    // Act
    let report = report_of(&mixed_fixture());

    // Assert: rows 2, 3, 7, 8, 9, 18 in the cohort; plus 10 and 12 globally.
    assert_eq!(report.anchored.unverified_reports, 6);
    assert_eq!(report.anchored.pass_unverified, 1);
    assert_eq!(report.unverified_reports, 8);
    assert_eq!(report.lanes[LANE_A_KEY].unverified_reports, 6);
    assert_eq!(report.lanes[LANE_B_KEY].unverified_reports, 2);
}

#[test]
fn a_failed_turn_with_a_relayed_terminal_is_an_unverified_report() {
    // Arrange: a proxy's explicit report accepted before the turn failed.
    let rows = [Row::anthropic(
        Some(LANE_A),
        "upstream_error",
        Some(anchored(1_000, 1_000, false)),
    )];

    // Act
    let report = report_of(&rows);

    // Assert
    assert_eq!(report.anchored.pass, 0);
    assert_eq!(report.anchored.unverified_reports, 1);
    // Positive control: the same row with a vendor-verified terminal is not.
    let verified = report_of(&[Row::anthropic(
        Some(LANE_A),
        "upstream_error",
        Some(anchored(1_000, 1_000, true)),
    )]);
    assert_eq!(verified.anchored.unverified_reports, 0);
}

#[test]
fn field_defects_on_known_rows_are_counted_by_kind() {
    // Act
    let report = report_of(&mixed_fixture());

    // Assert: row 17's marker, row 18's string count.
    assert_eq!(
        report.defects,
        counts(&[
            (defect::MARKER_INCONSISTENT, 1),
            (defect::COUNT_MALFORMED, 1)
        ])
    );
    assert_eq!(report.defect_rows, 2);
}

// ------------------------------------------------------------ distributions

fn buckets(w5: u64, w10: u64, w20: u64, over: u64, nc: u64) -> Buckets {
    Buckets {
        within_5: w5,
        within_10: w10,
        within_20: w20,
        over_20: over,
        not_comparable: nc,
    }
}

#[test]
fn error_buckets_split_by_opening_source_with_inclusive_upper_edges() {
    // Act
    let by = report_of(&mixed_fixture()).buckets_by_source;

    // Assert
    assert_eq!(by[ANCHOR], buckets(2, 1, 0, 1, 7));
    assert_eq!(by[CALIBRATED], buckets(0, 0, 1, 0, 0));
    assert_eq!(by[RAW], buckets(0, 0, 0, 1, 0));
    assert_eq!(by[source::UPSTREAM_WIRE_UNVERIFIED], buckets(1, 0, 0, 0, 0));
    assert_eq!(by.len(), 4);
}

#[test]
fn cold_start_is_the_known_rows_whose_anchor_had_no_record() {
    // Act
    let cold = report_of(&mixed_fixture()).cold_start;

    // Assert: rows 10 and 11 only; the upstream-wire row is not cold.
    assert_eq!(
        cold.into_iter().collect::<Vec<_>>(),
        vec![
            (CALIBRATED, buckets(0, 0, 1, 0, 0)),
            (RAW, buckets(0, 0, 0, 1, 0)),
        ]
    );
}

#[test]
fn lanes_reconcile_to_the_universe_and_carry_their_own_anchored_split() {
    // Act
    let report = report_of(&mixed_fixture());

    // Assert
    let lane_a = &report.lanes[LANE_A_KEY];
    let lane_b = &report.lanes[LANE_B_KEY];
    let none = &report.lanes["-"];
    assert_eq!((lane_a.rows, lane_a.anchored, lane_a.pass), (10, 5, 2));
    assert_eq!((lane_b.rows, lane_b.anchored, lane_b.pass), (7, 6, 0));
    assert_eq!((none.rows, none.no_opening), (1, 1));
    assert_eq!((lane_a.legacy, lane_b.unclassifiable), (2, 1));
    assert_eq!((lane_a.defect_rows, lane_b.defect_rows), (1, 1));
    assert_eq!(
        lane_a.sources,
        counts(&[
            (ANCHOR, 5),
            (CALIBRATED, 1),
            (RAW, 1),
            (source::UPSTREAM_WIRE_UNVERIFIED, 1),
        ])
    );
    assert_eq!(
        lane_b.reasons,
        counts(&[("anchor_hit", 5), (UNRECOGNIZED_LABEL, 1)])
    );
    assert_eq!(lane_b.buckets[ANCHOR], buckets(0, 0, 0, 1, 5));
    assert_eq!(
        lane_b.terminals,
        counts(&[
            (terminal::EXPLICIT_FINAL, 1),
            (terminal::MISSING, 2),
            (terminal::PROXY_OPENING, 1),
            (terminal::VENDOR_OPENING, 1),
            (UNRECOGNIZED_LABEL, 1),
        ])
    );
    let total: u64 = report.lanes.values().map(|l| l.rows).sum();
    assert_eq!(total, report.universe);
}

#[test]
fn a_repointed_nickname_splits_into_two_lanes_by_upstream_and_kind() {
    // Arrange: the same provider/model nickname on two upstreams and kinds.
    let repointed = ("openai-compat", "anth", "opus", "other-model-1");
    let rows = [
        Row::ok(anchored(1_000, 1_000, true)),
        Row::anthropic(Some(repointed), "ok", Some(anchored(2_000, 1_000, true))),
    ];

    // Act
    let report = report_of(&rows);

    // Assert
    assert_eq!(report.lanes.len(), 2);
    assert_eq!(report.lanes[LANE_A_KEY].pass, 1);
    assert_eq!(
        report.lanes["openai-compat:anth/opus@other-model-1"].pass,
        0
    );
}

#[test]
fn only_the_upstream_changing_still_splits_the_lane() {
    // Arrange: same provider kind, provider and model nicknames; only the
    // recorded upstream model id differs.
    let other_upstream: Lane = (LANE_A.0, LANE_A.1, LANE_A.2, "claude-opus-4-8");
    let rows = [
        Row::ok(anchored(1_000, 1_000, true)),
        Row::ok(anchored(1_000, 1_000, true)),
        Row::anthropic(
            Some(other_upstream),
            "ok",
            Some(anchored(2_000, 1_000, true)),
        ),
    ];

    // Act
    let report = report_of(&rows);

    // Assert
    assert_eq!(report.lanes.len(), 2, "{:?}", report.lanes.keys());
    let original = &report.lanes[LANE_A_KEY];
    let repointed = &report.lanes["anthropic-api:anth/opus@claude-opus-4-8"];
    assert_eq!((original.rows, original.anchored, original.pass), (2, 2, 2));
    assert_eq!(
        (repointed.rows, repointed.anchored, repointed.pass),
        (1, 1, 0)
    );
    assert_eq!(original.buckets[ANCHOR], buckets(2, 0, 0, 0, 0));
    assert_eq!(repointed.buckets[ANCHOR], buckets(0, 0, 0, 1, 0));
}

// ------------------------------------------------------------ row edges

#[test]
fn an_anchored_row_passes_only_on_evidence_within_five_percent_either_side() {
    let cases = [
        (anchored(950, 1_000, false), true),
        (anchored(949, 1_000, false), false),
        (anchored(1_050, 1_000, false), true),
        (anchored(1_051, 1_000, false), false),
        (anchored(1, 1, false), true),
        (anchored(u64::MAX, 1, false), false),
    ];
    for (extra, passes) in cases {
        let report = report_of(&[Row::ok(extra.clone())]);
        assert_eq!(report.anchored.eligible, 1, "{extra}");
        assert_eq!(report.anchored.pass == 1, passes, "{extra}");
    }
}

#[test]
fn a_terminal_label_the_producer_does_not_call_evidence_never_passes() {
    for label in [
        terminal::INTERIM_CARRY,
        terminal::PARTIAL_FINAL,
        terminal::PROXY_OPENING,
        terminal::UNMARKED,
        terminal::UNRECOGNIZED,
    ] {
        // Arrange: a perfect match on a non-evidence label.
        let extra = opened(ANCHOR, Some(1_000), Some((label, json!(1_000), true)));

        // Act
        let report = report_of(&[Row::ok(extra)]);

        // Assert
        assert_eq!(
            report.anchored.non_pass,
            counts(&[(non_pass::TERMINAL_UNSUPPORTED, 1)]),
            "{label}"
        );
    }
    // Positive control: the same row with an evidence label passes.
    assert_eq!(
        report_of(&[Row::ok(anchored(1_000, 1_000, false))])
            .anchored
            .pass,
        1
    );
}

#[test]
fn a_failed_or_disconnected_anchored_turn_stays_in_the_cohort_as_a_non_pass() {
    for outcome in [
        "upstream_error",
        "client_disconnect",
        "timeout",
        "cancelled",
    ] {
        // Act
        let report = report_of(&[Row::anthropic(
            Some(LANE_A),
            outcome,
            Some(anchored(1_000, 1_000, true)),
        )]);

        // Assert
        assert_eq!(
            report.anchored.non_pass,
            counts(&[(non_pass::OUTCOME_NOT_OK, 1)]),
            "{outcome}"
        );
    }
}

#[test]
fn an_exact_anchor_source_is_a_known_anchor_whatever_its_marker_says() {
    let good = anchored(1_000, 1_000, true);
    let cases = [
        with(&good, key::OPENING_PRESENT, None),
        with(&good, key::OPENING_PRESENT, Some(json!(false))),
        with(&good, key::OPENING_PRESENT, Some(json!("true"))),
    ];
    for extra in cases {
        // Act
        let report = report_of(&[Row::ok(extra.clone())]);

        // Assert: in the cohort, a non-pass, and a data defect.
        assert_eq!(report.anchored.eligible, 1, "{extra}");
        assert_eq!(
            report.anchored.non_pass,
            counts(&[(non_pass::MARKER_INCONSISTENT, 1)]),
            "{extra}"
        );
        assert_eq!(report.defects[defect::MARKER_INCONSISTENT], 1, "{extra}");
        assert!(report.unclassifiable.is_empty(), "{extra}");
    }
    // Positive control: the consistent row passes.
    assert_eq!(report_of(&[Row::ok(good)]).anchored.pass, 1);
}

#[test]
fn unknown_or_contradictory_rows_are_unclassifiable_by_fixed_kind() {
    let good_raw = opened(RAW, Some(1_000), explicit(1_000, true));
    let big = format!(
        "{{\"opening_present\":true,\"opening_source\":\"anchor\",\"pad\":\"{}\"}}",
        "x".repeat(classify::MAX_EXTRA_BYTES)
    );
    let cases = [
        ("[1,2]".to_string(), unclassifiable::NOT_AN_OBJECT),
        ("{bad".to_string(), unclassifiable::INVALID_JSON),
        (big, unclassifiable::OVERSIZED),
        (
            with(&good_raw, key::OPENING_PRESENT, None),
            unclassifiable::OPENING_PRESENT_MISSING,
        ),
        (
            with(&good_raw, key::OPENING_PRESENT, Some(json!(1))),
            unclassifiable::OPENING_PRESENT_NOT_BOOL,
        ),
        (
            with(&good_raw, key::OPENING_PRESENT, Some(json!(false))),
            unclassifiable::NO_OPENING_CONTRADICTED,
        ),
        (
            with(&good_raw, key::OPENING_SOURCE, None),
            unclassifiable::SOURCE_MISSING,
        ),
        (
            with(&good_raw, key::OPENING_SOURCE, Some(json!("Anchor"))),
            unclassifiable::SOURCE_UNRECOGNIZED,
        ),
        (
            with(&good_raw, key::OPENING_SOURCE, Some(json!(7))),
            unclassifiable::SOURCE_UNRECOGNIZED,
        ),
    ];
    for (extra, kind) in cases {
        // Act
        let report = report_of(&[Row::ok(extra.clone())]);

        // Assert
        assert_eq!(report.unclassifiable, counts(&[(kind, 1)]), "{kind}");
        assert_eq!(report.anchored.eligible, 0, "{kind}");
        assert_eq!(report.classified(), 1, "{kind}");
    }
}

#[test]
fn legacy_is_only_a_row_with_no_producer_key_at_all() {
    // Arrange: foreign keys only, versus one stray producer key.
    let foreign = json!({"stream_stage": "body", "credential_source": "forwarded"});
    let stray = json!({"stream_stage": "body", key::TERMINAL_INPUT: 5});

    // Act
    let report = report_of(&[
        Row::anthropic(Some(LANE_A), "ok", None),
        Row::ok(foreign.to_string()),
        Row::ok(stray.to_string()),
    ]);

    // Assert
    assert_eq!(report.legacy, 2);
    assert_eq!(
        report.unclassifiable,
        counts(&[(unclassifiable::OPENING_PRESENT_MISSING, 1)])
    );
}

#[test]
fn malformed_counts_flags_and_labels_on_known_rows_are_defects() {
    let good = opened(RAW, Some(1_000), explicit(1_000, true));
    let cases = [
        (
            with(&good, key::OPENING_INPUT, Some(json!(1000.5))),
            defect::COUNT_MALFORMED,
        ),
        (
            with(&good, key::TERMINAL_INPUT, Some(json!(-1))),
            defect::COUNT_MALFORMED,
        ),
        (
            with(&good, key::OPENING_PROVISIONAL, Some(json!(1))),
            defect::FLAG_MALFORMED,
        ),
        (
            with(&good, key::OPENING_REASON, Some(json!("colder"))),
            defect::REASON_UNRECOGNIZED,
        ),
        (
            with(&good, key::TERMINAL_SOURCE, Some(json!("explicit"))),
            defect::TERMINAL_SOURCE_UNRECOGNIZED,
        ),
        (
            with(&good, key::TERMINAL_VENDOR_VERIFIED, None),
            defect::REQUIRED_KEY_MISSING,
        ),
    ];
    for (extra, kind) in cases {
        // Act
        let report = report_of(&[Row::ok(extra)]);

        // Assert
        assert_eq!(report.defects, counts(&[(kind, 1)]), "{kind}");
        assert_eq!(report.known_by_source[RAW], 1, "{kind}");
    }
    // Positive control: the untouched row has no defect.
    assert!(report_of(&[Row::ok(good)]).defects.is_empty());
}

#[test]
fn non_integer_counts_on_an_anchored_row_are_non_passes_not_exclusions() {
    let good = anchored(1_000, 1_000, false);
    let cases = [
        (
            with(&good, key::OPENING_INPUT, None),
            non_pass::OPENING_UNSTATED,
        ),
        (
            with(&good, key::OPENING_INPUT, Some(json!(1000.5))),
            non_pass::OPENING_MALFORMED,
        ),
        (
            with(&good, key::OPENING_INPUT, Some(json!(-3))),
            non_pass::OPENING_MALFORMED,
        ),
        (
            with(&good, key::TERMINAL_INPUT, Some(json!(-1))),
            non_pass::TERMINAL_MALFORMED,
        ),
        (
            with(&good, key::TERMINAL_SOURCE, Some(json!(5))),
            non_pass::TERMINAL_MALFORMED,
        ),
    ];
    for (extra, reason) in cases {
        let report = report_of(&[Row::ok(extra.clone())]);
        assert_eq!(report.anchored.eligible, 1, "{extra}");
        assert_eq!(report.anchored.non_pass, counts(&[(reason, 1)]), "{extra}");
    }
}

// ------------------------------------------------------------ entry

#[test]
fn a_missing_ledger_is_an_explicit_no_ledger_state_and_creates_nothing() {
    // Arrange
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("absent.db");

    // Act
    let (out, verdict) = opening_accuracy_for_path(&path, "== t ==", WINDOW).expect("rendered");

    // Assert
    assert_eq!(verdict, Verdict::NoLedger);
    assert!(out.contains("no usage data yet"), "{out}");
    assert!(!path.exists(), "the report must not create the ledger");
}

#[test]
fn an_unreadable_ledger_is_a_user_facing_error_naming_the_path() {
    // Arrange: a file that is not SQLite.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let junk = b"not a database at all, just some bytes....";
    std::fs::write(&path, junk).expect("write");

    // Act
    let err = opening_accuracy_for_path(&path, "== t ==", WINDOW).expect_err("unreadable");

    // Assert
    let msg = err.to_string();
    assert!(msg.contains("usage db unavailable"), "{msg}");
    assert!(msg.contains(&path.display().to_string()), "{msg}");
    assert_eq!(std::fs::read(&path).expect("read back"), junk);
}

#[test]
fn the_report_needs_an_explicit_window() {
    // Arrange
    let now = Local::now();

    // Act + Assert
    assert!(matches!(
        opening_accuracy_bounds(WindowFlag::None, None, None, now),
        Err(UsageError::OpeningAccuracyNeedsWindow)
    ));
    let since = opening_accuracy_bounds(WindowFlag::None, Some("2026-06-01"), None, now)
        .expect("since is a window");
    assert_eq!(
        since.0,
        since_bounds("2026-06-01", None, now).expect("bounds")
    );
    let all = opening_accuracy_bounds(WindowFlag::All, None, None, now).expect("all");
    assert_eq!(all.0, window_bounds(WindowFlag::All, now));
}
