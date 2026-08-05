use std::collections::BTreeMap;

use routectl_router::{CatalogOverlay, OverlayCell, OverlaySource};

use super::{emit_staleness_hint, freshest_verified_at, staleness_hint_line};

// Epoch-day 0 (1970-01-01) as the verified_at stamp keeps the boundary
// arithmetic explicit: `today` reads directly as the age in days, so a
// threshold of 14 is stale exactly when today exceeds 14.
const EPOCH_STAMP: &str = "1970-01-01";
const THRESHOLD: i64 = 14;

fn cell(date: &str) -> OverlayCell {
    OverlayCell {
        source: OverlaySource::User,
        verified_at: date.to_string(),
        wm: None,
        rm: None,
        ttl_seconds: None,
        min_prefix_tokens: None,
        max_context_tokens: None,
        input_cost_per_token: None,
        output_cost_per_token: None,
        capabilities: None,
    }
}

fn overlay(entries: &[(&str, Option<&str>)]) -> CatalogOverlay {
    let mut cells = BTreeMap::new();
    for (key, date) in entries {
        cells.insert((*key).to_string(), date.map(cell));
    }
    CatalogOverlay {
        schema_version: 1,
        revision: 1,
        cells,
    }
}

#[test]
fn hint_line_none_at_threshold_boundary() {
    assert_eq!(
        staleness_hint_line(EPOCH_STAMP, THRESHOLD, THRESHOLD),
        None,
        "a stamp exactly at the threshold reads as fresh"
    );
}

#[test]
fn hint_line_some_one_day_past_threshold() {
    let line = staleness_hint_line(EPOCH_STAMP, THRESHOLD, THRESHOLD + 1)
        .expect("one day past the threshold is stale");
    assert!(line.contains(EPOCH_STAMP), "line names the stamp date");
    assert!(
        line.contains("routectl catalog import"),
        "line names the remediation"
    );
    assert!(line.contains("14 days"), "line names the age horizon");
    assert!(line.is_ascii(), "line is ASCII only");
}

#[test]
fn hint_line_malformed_stamp_reads_as_stale() {
    assert!(
        staleness_hint_line("not-a-date", THRESHOLD, 10_000).is_some(),
        "an unparseable stamp surfaces rather than hides"
    );
}

#[allow(clippy::fn_params_excessive_bools)]
fn capture(is_tty: bool, is_ci: bool, kill_switch: bool, is_json: bool) -> String {
    let mut sink: Vec<u8> = Vec::new();
    emit_staleness_hint(
        &mut sink,
        EPOCH_STAMP,
        THRESHOLD,
        100,
        is_tty,
        is_ci,
        kill_switch,
        is_json,
    );
    String::from_utf8(sink).expect("hint output is valid UTF-8")
}

#[test]
fn emit_writes_hint_to_the_stderr_stream_on_the_positive_path() {
    let out = capture(true, false, false, false);
    assert!(
        out.contains("routectl catalog import"),
        "the stale hint lands on the captured stderr stream"
    );
    assert!(out.ends_with('\n'), "the hint is a single terminated line");
}

#[test]
fn emit_suppressed_on_json_output() {
    assert_eq!(capture(true, false, false, true), "");
}

#[test]
fn emit_suppressed_when_stderr_is_not_a_terminal() {
    assert_eq!(capture(false, false, false, false), "");
}

#[test]
fn emit_suppressed_under_ci() {
    assert_eq!(capture(true, true, false, false), "");
}

#[test]
fn emit_suppressed_by_kill_switch() {
    assert_eq!(capture(true, false, true, false), "");
}

#[test]
fn hint_fires_only_when_all_four_gates_align() {
    // The seam is a conjunction of four gates over a stale stamp: it fires
    // only when stderr is a TTY, the run is not CI, the kill switch is unset,
    // and output is not JSON. The per-gate tests above pin each suppression in
    // isolation; this exercises all four TOGETHER across the full truth table,
    // so no combination other than the all-aligned one can ever emit. The
    // stamp is stale (EPOCH_STAMP against today 100, threshold 14) throughout,
    // isolating the gate conjunction from the age check.
    for is_tty in [false, true] {
        for is_ci in [false, true] {
            for kill_switch in [false, true] {
                for is_json in [false, true] {
                    let fired = !capture(is_tty, is_ci, kill_switch, is_json).is_empty();
                    let want = is_tty && !is_ci && !kill_switch && !is_json;
                    assert_eq!(
                        fired, want,
                        "gates tty={is_tty} ci={is_ci} kill={kill_switch} json={is_json}: \
                         fired={fired} but expected {want}"
                    );
                }
            }
        }
    }
}

#[test]
fn emit_stays_silent_when_the_stamp_is_fresh() {
    let mut sink: Vec<u8> = Vec::new();
    emit_staleness_hint(
        &mut sink,
        EPOCH_STAMP,
        THRESHOLD,
        THRESHOLD,
        true,
        false,
        false,
        false,
    );
    assert!(sink.is_empty(), "a fresh stamp emits nothing");
}

#[test]
fn freshest_is_none_for_an_empty_overlay() {
    assert_eq!(freshest_verified_at(&CatalogOverlay::default()), None);
}

#[test]
fn freshest_picks_the_most_recent_present_stamp() {
    let ov = overlay(&[
        ("a:x", Some("2025-06-01")),
        ("b:y", Some("2026-01-15")),
        ("c:z", Some("2025-12-31")),
    ]);
    assert_eq!(freshest_verified_at(&ov).as_deref(), Some("2026-01-15"));
}

#[test]
fn freshest_ignores_disabled_cells() {
    let ov = overlay(&[("a:x", None), ("b:y", Some("2025-06-01")), ("c:z", None)]);
    assert_eq!(freshest_verified_at(&ov).as_deref(), Some("2025-06-01"));
}

#[test]
fn freshest_is_none_when_every_cell_is_disabled() {
    let ov = overlay(&[("a:x", None), ("b:y", None)]);
    assert_eq!(freshest_verified_at(&ov), None);
}
