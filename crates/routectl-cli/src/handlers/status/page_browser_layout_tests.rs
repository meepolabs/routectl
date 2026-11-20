// Every figure readable in full in a real headless Chrome, at the widths the
// page supports and at 1440 px with the text doubled (a 32 px default font in
// the browser profile; the page sizes its type in rem). `include!`d into a
// child module of `page::tests`; the scenario lives in
// page_browser_layout_driver.js.

use serde_json::{Value, json};

const LAYOUT_DRIVER: &str = include_str!("page_browser_layout_driver.js");

const LAYOUT_BUDGET_MS: u64 = 40_000;

/// The views whose figures the checks read, each at the address that shows
/// its densest content.
const VIEWS: &[(&str, &str)] = &[
    ("now", "now?range=week"),
    ("accounts", "accounts"),
    ("traffic", "traffic?range=week&group=alias"),
    ("models", "models"),
    ("config", "config"),
    ("doctor", "doctor"),
];

/// One browser setup: the window and the profile's default font size.
struct Setup {
    name: &'static str,
    width: u32,
    font_px: u32,
}

const TEXT_200: Setup = Setup {
    name: "1440 px, 200% text",
    width: 1440,
    font_px: 32,
};

const DEFAULT_TEXT: Setup = Setup {
    name: "1440 px, default text",
    width: 1440,
    font_px: 16,
};

const FLOOR: Setup = Setup {
    name: "720 px, default text",
    width: 720,
    font_px: 16,
};

fn run_layout(setup: &Setup, hash: &'static str) -> Value {
    let scenario = browser::Scenario {
        name: "layout-check",
        hash,
        snippets: &[LAYOUT_DRIVER],
        budget_ms: LAYOUT_BUDGET_MS,
    };
    let window = format!("--window-size={},900", setup.width);
    let preferences = json!({ "webkit": { "webprefs": { "default_font_size": setup.font_px } } });
    browser::run_scenario_in_profile(&scenario, &[&window], &preferences).result
}

fn list<'a>(audit: &'a Value, key: &str) -> &'a Vec<Value> {
    audit[key].as_array().expect("a list in the audit")
}

/// The view as the setup drew it, and nothing hides text: no box cuts off
/// text it cannot fit (a truncated name carrying its full text in a title is
/// the one allowance), no scroller other than a wide table inside its card is
/// out of keyboard reach, no two text runs
/// in a stat row draw over each other, and every fix command fits its box.
/// Positive controls: the page really ran at the setup's text size and width,
/// the audit read elements carrying digits, the stat rows had text runs, and
/// the view carried a fix command where one is expected, and Now drew its
/// account meters.
fn assert_readable(setup: &Setup, view: &str, out: &Value) {
    let at = format!("{view} at {}", setup.name);
    assert_eq!(out["view"], view, "{at}");
    assert_eq!(
        out["root_font_px"].as_f64(),
        Some(f64::from(setup.font_px)),
        "{at}: the profile's font size did not apply"
    );
    assert_eq!(
        out["inner_width"].as_u64(),
        Some(u64::from(setup.width)),
        "{at}"
    );
    assert_eq!(
        out["page_scrolls_sideways"], false,
        "{at}: the page scrolls sideways"
    );
    for part in ["strip", "body"] {
        let audit = &out[part];
        assert!(
            audit["digit_elements"].as_u64().unwrap_or(0) > 0,
            "{at}: the {part} audit read no figure: {audit:#}"
        );
        assert_eq!(
            list(audit, "hidden"),
            &Vec::<Value>::new(),
            "{at}: the {part} hides text"
        );
        assert_eq!(
            list(audit, "collisions"),
            &Vec::<Value>::new(),
            "{at}: text in a stat row overlaps"
        );
    }
    if view == "now" {
        assert!(
            out["glance_meters"].as_u64().unwrap_or(0) > 0,
            "{at}: Now drew no account meter: {out:#}"
        );
    }
    let body = &out["body"];
    if view == "traffic" {
        assert!(
            body["stat_rows"].as_u64().unwrap_or(0) > 0
                && body["stat_runs"].as_u64().unwrap_or(0) > 0,
            "{at}: no stat row was measured: {body:#}"
        );
    }
    if view == "doctor" {
        let fixes = list(body, "fixes");
        assert!(!fixes.is_empty(), "{at}: no fix command was measured");
        for fix in fixes {
            assert_eq!(fix["hidden_overflow"], false, "{at}: {fix:#}");
            assert_eq!(fix["box_inside_card"], true, "{at}: {fix:#}");
        }
    }
}

fn assert_views_readable(setup: &Setup) {
    for (view, hash) in VIEWS {
        assert_readable(setup, view, &run_layout(setup, hash));
    }
}

/// At 1440 px with the text doubled, every view (Traffic over a week, grouped
/// by alias) shows every figure, label and fix command in full, the figures
/// widened to the magnitudes a real ledger produces.
///
/// Mutation checks: put `.ucard-stats` back on its px column minimum AND
/// `.ustat-label` and `.ustat-value` back on no-wrap with an ellipsis -> red
/// (figures are cut off); the column minimum alone stays green, because the
/// wrapping labels still fit.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_figures_read_in_full_at_double_text_size() {
    assert_views_readable(&TEXT_200);
}

/// The same views hold at the default text size, so the reflow for large
/// text does not cost the common case.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_figures_read_in_full_at_default_text_size() {
    assert_views_readable(&DEFAULT_TEXT);
}

/// At the 720 px floor the same views still show everything and the page
/// does not scroll sideways.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_figures_read_in_full_at_the_narrow_floor() {
    assert_views_readable(&FLOOR);
}

/// The audit is not blind: over planted boxes beside the view it flags a
/// numeral cut by an ellipsis, one clipped without, one pushed past the
/// window's edge, a scroller the
/// keyboard cannot reach, and two labels drawn over each other; it lets
/// through the same scroller made focusable, and files a titled name apart
/// from the text it hides.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_layout_audit_flags_planted_cut_offs_and_collisions() {
    let out = run_layout(&TEXT_200, "traffic?range=week&group=alias");
    let ctl = &out["controls"];
    let hidden: Vec<(&str, &str)> = list(ctl, "hidden")
        .iter()
        .map(|h| {
            (
                h["kind"].as_str().expect("a kind"),
                h["el"].as_str().expect("an element"),
            )
        })
        .collect();

    let flagged = |kind: &str, id_text: &str| {
        hidden
            .iter()
            .any(|(k, el)| *k == kind && el.contains(id_text))
    };
    assert!(flagged("ellipsis", "123456789012345"), "{ctl:#}");
    assert!(flagged("clipped", "987654321098765"), "{ctl:#}");
    assert!(flagged("offscreen", "555555555555"), "{ctl:#}");
    assert_eq!(
        hidden
            .iter()
            .filter(|(k, _)| *k == "unreachable-scroller")
            .count(),
        1,
        "only the unfocusable scroller is flagged: {ctl:#}"
    );
    assert_eq!(hidden.len(), 4, "{ctl:#}");
    assert_eq!(list(ctl, "titled_names").len(), 1, "{ctl:#}");
    assert_eq!(list(ctl, "collisions").len(), 1, "{ctl:#}");
}
