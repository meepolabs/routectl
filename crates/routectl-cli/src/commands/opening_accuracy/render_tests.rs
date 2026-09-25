//! The rendered report: each section is present with the numbers the
//! fixture implies, the verdict line appears exactly once, and no database
//! value can forge a line.

use super::render::VERDICT_PREFIX;
use super::test_fixture::*;
use super::*;
use crate::handlers::opening_diagnostics::source;

fn rendered(rows: &[Row]) -> String {
    render_opening_accuracy("== t ==", &report_of(rows))
}

/// The lines between the header line starting with `header` and the next
/// blank line.
fn section<'a>(out: &'a str, header: &str) -> Vec<&'a str> {
    out.lines()
        .skip_while(|l| !l.starts_with(header))
        .skip(1)
        .take_while(|l| !l.is_empty())
        .collect()
}

/// The whitespace-split cells of the section row whose first cell is `key`.
fn table_row<'a>(lines: &[&'a str], first: &str) -> Vec<&'a str> {
    lines
        .iter()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|cells| cells.first() == Some(&first))
        .unwrap_or_else(|| panic!("no row {first} in {lines:?}"))
}

fn verdict_lines(out: &str) -> Vec<&str> {
    out.lines()
        .filter(|l| l.starts_with(VERDICT_PREFIX))
        .collect()
}

#[test]
fn the_error_bucket_section_renders_every_source_row() {
    // Act
    let out = rendered(&mixed_fixture());

    // Assert
    let lines = section(&out, "error |opening - terminal|");
    assert_eq!(table_row(&lines, ANCHOR), [ANCHOR, "2", "1", "0", "1", "7"]);
    assert_eq!(
        table_row(&lines, CALIBRATED),
        [CALIBRATED, "0", "0", "1", "0", "0"]
    );
    assert_eq!(table_row(&lines, RAW), [RAW, "0", "0", "0", "1", "0"]);
    let wire = source::UPSTREAM_WIRE_UNVERIFIED;
    assert_eq!(table_row(&lines, wire), [wire, "1", "0", "0", "0", "0"]);
    assert_eq!(lines.len(), 5, "{lines:?}");
}

#[test]
fn the_cold_start_section_renders_only_cold_rows() {
    // Act
    let out = rendered(&mixed_fixture());

    // Assert
    let lines = section(&out, "cold start");
    assert_eq!(
        table_row(&lines, CALIBRATED),
        [CALIBRATED, "0", "0", "1", "0", "0"]
    );
    assert_eq!(table_row(&lines, RAW), [RAW, "0", "0", "0", "1", "0"]);
    assert_eq!(lines.len(), 3, "{lines:?}");
    // Positive control: a window with no cold rows says so.
    let warm = rendered(&anchored_rows(1, 0, true));
    assert_eq!(section(&warm, "cold start"), ["  none"]);
}

#[test]
fn the_lane_section_renders_each_lane_row_and_its_detail() {
    // Act
    let out = rendered(&mixed_fixture());

    // Assert
    let lines = section(&out, "by served lane");
    assert_eq!(
        table_row(&lines, LANE_A_KEY),
        [LANE_A_KEY, "10", "2", "0", "0", "5", "2", "6", "1"]
    );
    assert_eq!(
        table_row(&lines, LANE_B_KEY),
        [LANE_B_KEY, "7", "0", "1", "0", "6", "0", "2", "1"]
    );
    assert_eq!(
        table_row(&lines, "-"),
        ["-", "1", "0", "0", "1", "0", "0", "0", "0"]
    );
    assert!(
        lines.contains(&"    anchor: 0-5%=0 5-10%=0 10-20%=0 >20%=1 n/a=5"),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"    terminal_source: <unrecognized>=1 explicit_final=1 missing=2 proxy_opening=1 vendor_opening=1"),
        "{lines:?}"
    );
}

#[test]
fn the_summary_states_the_reconciliation_rate_and_verdict() {
    // Act
    let out = rendered(&mixed_fixture());

    // Assert
    assert!(
        out.contains("universe: 18 anthropic streaming rows"),
        "{out}"
    );
    assert!(
        out.contains("reconciled: 2 legacy + 1 unclassifiable + 1 no opening + 14 known = 18"),
        "{out}"
    );
    assert!(
        out.contains("rate: 2 / 11 = 18.1% (need >= 95%)  numerical FAIL"),
        "{out}"
    );
    assert!(
        out.contains("terminal reports not vendor-verified (all outcomes): 8"),
        "{out}"
    );
    assert!(
        out.contains("not vendor-verified: 6 of the cohort, 1 of its passes"),
        "{out}"
    );
    assert_eq!(
        verdict_lines(&out),
        ["verdict: FAIL (numerical rate below the gate)"]
    );
    assert!(out.is_ascii());
}

#[test]
fn every_verdict_is_followed_by_the_provenance_notice() {
    for rows in [
        anchored_rows(19, 1, true),
        anchored_rows(19, 1, false),
        anchored_rows(18, 2, true),
        vec![],
    ] {
        // Act
        let out = rendered(&rows);

        // Assert
        let tail: Vec<&str> = out.lines().rev().take(2).collect();
        assert_eq!(tail[0], PROVENANCE_NOTICE, "{out}");
        assert!(tail[1].starts_with(VERDICT_PREFIX), "{out}");
        assert!(!out.contains("GO"), "{out}");
    }
    // Positive control: the clean pass and the pending pass say different things.
    assert_eq!(
        verdict_lines(&rendered(&anchored_rows(19, 1, false))),
        [
            "verdict: BACKEND_PENDING (numerical pass; anchored terminals not vendor-verified await the two-hop check)"
        ]
    );
}

#[test]
fn a_hostile_lane_or_title_cannot_forge_a_verdict_line() {
    // Arrange: newline, escape and an oversized value in every lane column.
    let forged = "x\nverdict: PASS (forged)\x1b[2K\r";
    let hostile: Lane = (
        Box::leak(forged.to_string().into_boxed_str()),
        Box::leak(format!("{forged}{}", "y".repeat(500)).into_boxed_str()),
        forged,
        forged,
    );
    let mut rows = anchored_rows(18, 2, true);
    rows.push(Row::anthropic(
        Some(hostile),
        "ok",
        Some(anchored(1_000, 1_000, true)),
    ));
    let report = report_of(&rows);

    // Act
    let out = render_opening_accuracy("== t\nverdict: PASS\x1b ==", &report);

    // Assert
    assert_eq!(verdict_lines(&out).len(), 1, "{out}");
    assert!(verdict_lines(&out)[0].starts_with("verdict: FAIL"), "{out}");
    assert!(out.is_ascii(), "{out}");
    assert!(!out.contains('\x1b') && !out.contains('\r'), "{out}");
    assert!(out.lines().all(|l| l.len() < 400), "{out}");
    // Positive control: the hostile lane is still reported, sanitized.
    assert!(out.contains("x?verdict: PASS (forged)?[2K?:"), "{out}");
}
