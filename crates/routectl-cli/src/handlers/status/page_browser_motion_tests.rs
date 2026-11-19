// The changed-value mark in a real headless Chrome: a refresh marks exactly
// the figures whose value moved on a node the morph kept, the fade's end
// clears the mark so the next move fades again, a first paint or a view
// switch marks nothing, and reduced motion turns the fade off while the new
// value still shows. `include!`d into a child module of `page::tests`; the
// scenarios live in page_browser_motion_driver.js.

use serde_json::Value;

const MOTION_DRIVER: &str = include_str!("page_browser_motion_driver.js");

const MOTION_BUDGET_MS: u64 = 40_000;

const REDUCED_MOTION: &str = "--force-prefers-reduced-motion";

fn run_motion(
    name: &'static str,
    hash: &'static str,
    snippets: &'static [&'static str],
    switches: &[&str],
) -> Value {
    let scenario = browser::Scenario {
        name,
        hash,
        snippets,
        budget_ms: MOTION_BUDGET_MS,
    };
    browser::run_scenario_with(&scenario, switches).result
}

fn motion_refresh(hash: &'static str, switches: &[&str]) -> Value {
    run_motion("motion-refresh", hash, &[MOTION_DRIVER], switches)
}

fn motion_view_switch(home: &'static str, away: &str) -> Value {
    let set = format!("window.routectlMotionAway = {away:?};");
    let snippets: &'static [&'static str] =
        Box::leak(vec![&*Box::leak(set.into_boxed_str()), MOTION_DRIVER].into_boxed_slice());
    run_motion("motion-view-switch", home, snippets, &[])
}

fn is_meter(fig: &Value) -> bool {
    fig["node"]
        .as_str()
        .is_some_and(|n| n.contains("sharebar-fill"))
}

fn seen<'a>(round: &'a Value, side: &str) -> &'a Vec<Value> {
    round[side].as_array().expect("a list of kept figures")
}

/// On Accounts, a refresh moves the quota figures and meters while the rest
/// hold: every kept figure whose value moved is marked and starts the fade,
/// and no kept figure whose value held is marked. Positive controls: both
/// sides are non-empty, and a meter is among the moved, kept nodes, so its
/// `--pct` width transitions on the same node.
///
/// Mutation check: drop `markChangedValue` from morphGate -> red (nothing
/// moved is marked).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_refresh_marks_the_figures_that_changed_and_no_other() {
    let out = motion_refresh("accounts", &[]);
    let round = &out["round"];

    assert_eq!(
        out["first_paint_marked"], 0,
        "a first paint marked: {out:#}"
    );
    let moved = seen(round, "moved");
    let unmoved = seen(round, "unmoved");
    assert!(!moved.is_empty(), "no kept figure moved: {out:#}");
    assert!(!unmoved.is_empty(), "every kept figure moved: {out:#}");
    for fig in moved {
        assert_eq!(fig["marked"], true, "a moved figure unmarked: {fig:#}");
        assert_eq!(fig["animation"], "changefade", "{fig:#}");
    }
    for fig in unmoved {
        assert_eq!(fig["marked"], false, "a held figure marked: {fig:#}");
    }
    assert!(
        moved.iter().any(is_meter),
        "no meter moved on a kept node: {out:#}"
    );
}

/// A figure that moves again while its fade still runs fades again from the
/// start (a new animation, not the running one), and the fade's end clears
/// the mark. The probe is the first kept figure that moved in the first
/// round; the fade's end is dispatched by the driver, since headless Chrome
/// under virtual time renders too few frames to reach it.
///
/// Mutation checks: drop the `animationend` listener in dash_90_chrome.js ->
/// red (the mark outlives the fade's end); drop the class removal and style
/// flush in markChangedValue -> red (the running fade carries on).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_changed_mark_restarts_on_the_next_change_and_clears_at_its_end() {
    let out = motion_refresh("accounts", &[]);
    let second = &out["second_round"];

    assert_eq!(
        second["moved"], true,
        "the probe did not move again: {out:#}"
    );
    assert_eq!(second["marked"], true, "{out:#}");
    assert_eq!(second["fading"], true, "{out:#}");
    assert_eq!(
        second["restarted"], true,
        "the fade did not restart: {out:#}"
    );
    assert_eq!(out["after_end"]["marked"], false, "{out:#}");
}

/// Returning to a view whose sources moved while it was away is a first
/// paint: nothing is marked. Positive control: kept figures on the return
/// carry values that differ from those the pane last showed, so a mark had
/// something to fire on.
///
/// Mutation check: mark changes on every repaint (`var marksChanges = true;`)
/// -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_view_switch_marks_no_figure() {
    let out = motion_view_switch("accounts", "config");
    let back = &out["on_return"];

    assert!(
        !seen(back, "moved").is_empty(),
        "no kept figure moved while away: {out:#}"
    );
    assert_eq!(out["on_return_marked"], 0, "{out:#}");
}

/// With the reduced-motion preference forced, every moved figure computes no
/// animation, and the new value is on screen: a meter's `--pct` and a
/// figure's text are the value it moved to.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_reduced_motion_shows_the_new_value_without_the_fade() {
    let out = motion_refresh("accounts", &[REDUCED_MOTION]);
    let moved = seen(&out["round"], "moved");

    assert!(!moved.is_empty(), "no kept figure moved: {out:#}");
    for fig in moved {
        assert_eq!(fig["animation"], "none", "{fig:#}");
    }
    let meter = moved.iter().find(|f| is_meter(f)).expect("a moved meter");
    assert_eq!(meter["pct"], meter["now"], "{meter:#}");
    assert!(
        moved.iter().any(|f| !is_meter(f) && f["text"] == f["now"]),
        "no moved figure shows its new value as text: {out:#}"
    );
}
