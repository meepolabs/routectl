//! Executed record of the one baseline-shape verification that was
//! previously done by hand: the `plain-turn-01` driver case claims
//! `wire_pattern: baseline`, and its captured ingress structural line
//! must actually exhibit that shape (no tools, no active thinking
//! block, no cache breakpoints).
//!
//! The predicate lives in [`ingress_line_is_baseline`] so the three
//! failing controls below can prove each of its clauses load-bearing;
//! a fixture-reading assertion alone cannot distinguish a real check
//! from one that would pass on any line.

mod common;

use std::fs;
use std::path::PathBuf;

use common::replay::{driver_root, load_fixture};

/// Lane + case of the single fixture this file pins.
const LANE: &str = "anthropic-api";
const CASE: &str = "plain-turn-01";

/// Structural-summary file inside a driver fixture directory. Holds at
/// most two lines: the ingress summary first, then the outgoing one.
const STRUCTURAL_FILE: &str = "structural.txt";

fn case_dir() -> PathBuf {
    driver_root().join(LANE).join(CASE)
}

/// Value of the `key=value` token named `key`, or `None` when the line
/// carries no such token.
///
/// Token-exact by construction: a substring search for `thinking_shape=`
/// also matches `output_thinking_shape=...`, which would let an
/// unrelated field satisfy a clause about a missing one.
fn token_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .filter_map(|token| token.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

/// The ingress structural line, selected by its `direction="ingress"`
/// token rather than by position -- the file's line order is a capture
/// convention, not a guarantee.
fn ingress_line(structural: &str) -> Option<&str> {
    structural
        .lines()
        .find(|line| token_value(line, "direction").map(|v| v.trim_matches('"')) == Some("ingress"))
}

/// The `baseline` wire pattern as a predicate over one structural
/// summary line: no tools, no ACTIVE thinking block, no cache
/// breakpoints.
///
/// The thinking clause is deliberately two-sided. The real client sends
/// `thinking: {"type": "disabled"}`, which the summary renders as the
/// explicit token `thinking_shape=disabled` rather than as an absent
/// field, so both spellings of "off" are baseline and every other value
/// (`enabled:31999`, ...) is not.
fn ingress_line_is_baseline(line: &str) -> Result<(), String> {
    match token_value(line, "tools_len") {
        Some("0") => {}
        Some(other) => return Err(format!("tools_len={other}, want 0")),
        None => return Err("tools_len token absent".to_string()),
    }

    match token_value(line, "thinking_shape") {
        None | Some("" | "disabled") => {}
        Some(other) => return Err(format!("thinking_shape={other} is active")),
    }

    match token_value(line, "cache_control_count") {
        Some("0") => {}
        Some(other) => return Err(format!("cache_control_count={other}, want 0")),
        None => return Err("cache_control_count token absent".to_string()),
    }

    Ok(())
}

/// The `plain-turn-01` driver case exhibits the baseline wire shape it
/// records in `meta.json`.
///
/// SPECIAL CASE WITH A KNOWN END: this asserts one pattern against one
/// named fixture because no general predicate exists yet. The general
/// wire-pattern check -- every driver fixture against the pattern its
/// `meta.json` claims -- subsumes it entirely, and DELETES this test
/// when it lands. A special case must not survive behind the general
/// rule that covers it.
#[test]
fn plain_turn_01_carries_its_claimed_baseline_shape() {
    let dir = case_dir();
    let structural_path = dir.join(STRUCTURAL_FILE);
    if !structural_path.is_file() {
        println!(
            "{CASE} baseline verification: NOT RUN (no driver corpus in this checkout; \
             {STRUCTURAL_FILE} absent under {})",
            dir.display(),
        );
        return;
    }

    let structural = fs::read_to_string(&structural_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", structural_path.display()));
    let line = ingress_line(&structural).unwrap_or_else(|| {
        panic!(
            "{} carries no direction=\"ingress\" structural line",
            structural_path.display(),
        )
    });

    if let Err(why) = ingress_line_is_baseline(line) {
        panic!("{CASE} ingress line is not baseline: {why}");
    }

    let fixture = load_fixture(&dir).unwrap_or_else(|e| panic!("load {}: {e}", dir.display()));
    assert_eq!(
        fixture.meta.wire_pattern, "baseline",
        "{CASE} asserts the baseline predicate but records a different wire_pattern",
    );
    assert_eq!(
        fixture.meta.ingress_kind, "anthropic",
        "{CASE} must keep a non-empty ingress_kind: the conservation harness SKIPS a fixture \
         whose ingress_kind is empty, and with lane `{LANE}` gated a skip turns the gate red \
         for zero coverage -- a symptom that reads as unrelated to the lost token",
    );
}

// ---------------------------------------------------------------------------
// Controls: each failing line flips exactly ONE clause of the predicate.
// ---------------------------------------------------------------------------

/// Hand-built ingress summary in the real field order, with the three
/// predicate fields supplied by the caller.
fn structural_line(tools_len: &str, thinking: Option<&str>, cache_control_count: &str) -> String {
    let thinking_token = match thinking {
        Some(shape) => format!("thinking_shape={shape} "),
        None => String::new(),
    };
    format!(
        "structural summary direction=\"ingress\" kind=\"ingress\" id=\"anthropic\" \
         model=claude-sonnet-4-5 max_tokens=32000 {thinking_token}output_config_effort= \
         tool_choice_shape= cache_control_count={cache_control_count} messages_len=1 \
         tools_len={tools_len} anthropic_beta= provider_extras_keys= stream=true"
    )
}

#[test]
fn baseline_predicate_accepts_an_explicitly_disabled_thinking_shape() {
    let line = structural_line("0", Some("disabled"), "0");
    assert_eq!(ingress_line_is_baseline(&line), Ok(()));
}

#[test]
fn baseline_predicate_accepts_an_absent_thinking_shape() {
    let line = structural_line("0", None, "0");
    assert!(
        !line.contains("thinking_shape"),
        "control must omit the token"
    );
    assert_eq!(ingress_line_is_baseline(&line), Ok(()));
}

#[test]
fn baseline_predicate_rejects_a_line_carrying_tools() {
    let line = structural_line("16", Some("disabled"), "0");
    let err = ingress_line_is_baseline(&line).expect_err("tools_len=16 is not baseline");
    assert!(err.contains("tools_len"), "unexpected reason: {err}");
}

#[test]
fn baseline_predicate_rejects_an_enabled_thinking_shape() {
    let line = structural_line("0", Some("enabled:31999"), "0");
    let err =
        ingress_line_is_baseline(&line).expect_err("thinking_shape=enabled:31999 is not baseline");
    assert!(err.contains("thinking_shape"), "unexpected reason: {err}");
}

#[test]
fn baseline_predicate_rejects_cache_breakpoints() {
    let line = structural_line("0", Some("disabled"), "3");
    let err = ingress_line_is_baseline(&line).expect_err("cache_control_count=3 is not baseline");
    assert!(
        err.contains("cache_control_count"),
        "unexpected reason: {err}"
    );
}

#[test]
fn ingress_line_selection_ignores_the_outgoing_summary() {
    let outgoing = structural_line("0", Some("disabled"), "2")
        .replace("direction=\"ingress\"", "direction=\"outgoing\"");
    let ingress = structural_line("0", Some("disabled"), "0");
    let file = format!("{outgoing}\n{ingress}\n");

    assert_eq!(ingress_line(&file), Some(ingress.as_str()));
}

#[test]
fn token_lookup_is_exact_rather_than_substring() {
    let line = "structural summary output_thinking_shape=enabled:31999 tools_len=0";
    assert_eq!(token_value(line, "thinking_shape"), None);
}
