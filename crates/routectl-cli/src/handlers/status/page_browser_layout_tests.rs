// Every figure readable in full in a real headless Chrome, at the widths the
// page supports and at 1440 px with the text doubled (a 32 px default font in
// the browser profile; the page sizes its type in rem). `include!`d into a
// child module of `page::tests`; the scenario lives in
// page_browser_layout_driver.js.

use serde_json::{Value, json};

const LAYOUT_DRIVER: &str = include_str!("page_browser_layout_driver.js");

const LAYOUT_BUDGET_MS: u64 = 40_000;

const VERDICT_OUTAGE_BUDGET_MS: u64 = 90_000;

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
    run_in_setup(setup, "layout-check", hash, LAYOUT_BUDGET_MS)
}

/// One driver scenario in the setup's window and text size.
fn run_in_setup(setup: &Setup, name: &'static str, hash: &'static str, budget_ms: u64) -> Value {
    let scenario = browser::Scenario {
        name,
        hash,
        snippets: &[LAYOUT_DRIVER],
        budget_ms,
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
/// the audit read elements carrying digits, Traffic drew its group rows, and
/// the view carried a fix command where one is expected, and Now drew its
/// account meters, and Config held its priced model's detail open beside
/// its pool members.
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
    if view == "config" {
        assert_eq!(
            out["config_open_details"], 1,
            "{at}: the priced model's detail was not open: {out:#}"
        );
        assert!(
            out["config_pool_members"].as_u64().unwrap_or(0) > 0,
            "{at}: Config drew no pool member: {out:#}"
        );
    }
    let body = &out["body"];
    if view == "traffic" {
        assert!(
            out["traffic_rows"].as_u64().unwrap_or(0) > 0,
            "{at}: Traffic drew no group row: {out:#}"
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

/// Every table scroller in the view is a region the keyboard can stop on,
/// announced by its table's caption, and so is every one too wide for its
/// card. Returns how many of each the view held.
fn assert_tables_named(setup: &Setup, view: &str, out: &Value) -> (usize, usize) {
    let at = format!("{view} at {}", setup.name);
    let body = &out["body"];
    let named = |t: &Value| {
        let name = t["name"].as_str().unwrap_or("");
        assert!(
            !name.is_empty(),
            "{at}: a table scroller has no name: {t:#}"
        );
        assert_eq!(t["caption"].as_str(), Some(name), "{at}: {t:#}");
        assert_eq!(t["role"], "region", "{at}: {t:#}");
        assert_eq!(t["self_focusable"], true, "{at}: {t:#}");
    };
    list(body, "tables").iter().for_each(named);
    list(body, "table_scrollers").iter().for_each(named);
    (
        list(body, "tables").len(),
        list(body, "table_scrollers").len(),
    )
}

/// Checks every view and returns how many table scrollers they held in all,
/// and how many of those were too wide for their card.
fn assert_views_readable(setup: &Setup) -> (usize, usize) {
    let mut totals = (0, 0);
    for (view, hash) in VIEWS {
        let out = run_layout(setup, hash);
        assert_readable(setup, view, &out);
        let (tables, scrolling) = assert_tables_named(setup, view, &out);
        totals = (totals.0 + tables, totals.1 + scrolling);
    }
    totals
}

/// At 1440 px with the text doubled, every view (Traffic over a week, grouped
/// by alias) shows every figure, label and fix command in full, the figures
/// widened to the magnitudes a real ledger produces.
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
/// does not scroll sideways. A table too wide for its card scrolls inside
/// it, and that scroller takes keyboard focus itself (axe
/// scrollable-region-focusable) as a region named by its table's caption,
/// as does every other table scroller on every view. Positive control: the
/// views held table scrollers, and at least one scrolled at this width.
///
/// Mutation check: drop the `tabIndex` line from `tableScroll` -> red (the
/// too-wide tables become unreachable scrollers).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_figures_read_in_full_at_the_narrow_floor() {
    let (tables, scrolling) = assert_views_readable(&FLOOR);
    assert!(tables > 0, "no view held a table scroller");
    assert!(scrolling > 0, "no table scrolled inside its card at 720 px");
}

fn run_verdict_outage(setup: &Setup) -> Value {
    run_in_setup(
        setup,
        "layout-verdict-outage",
        "now",
        VERDICT_OUTAGE_BUDGET_MS,
    )
}

/// Through an outage the strip alone says the daemon cannot be reached (no
/// banner), and at 1440 px with the text doubled the sentence keeps its own
/// line rather than being squeezed into a column beside the note: it reads
/// on one line, as at the default size. Positive control: at 200% text the
/// note, link and figures do not all fit beside the sentence, so part of
/// the strip wraps to a row below it.
///
/// Mutation check: drop `flex-wrap: wrap` from `.verdict-lead` -> red (the
/// sentence wraps beside the note).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_outage_strip_keeps_the_sentence_whole_at_double_text_size() {
    let default = run_verdict_outage(&DEFAULT_TEXT);
    let doubled = run_verdict_outage(&TEXT_200);

    for (setup, out) in [(&DEFAULT_TEXT, &default), (&TEXT_200, &doubled)] {
        let at = setup.name;
        assert_eq!(
            out["root_font_px"].as_f64(),
            Some(f64::from(setup.font_px)),
            "{at}: {out:#}"
        );
        assert_eq!(out["banner_hidden"], true, "{at}: {out:#}");
        assert_eq!(out["link_text"], "see Accounts", "{at}: {out:#}");
    }
    assert_eq!(default["text_lines"], 1, "{default:#}");
    assert_eq!(
        doubled["text_lines"], default["text_lines"],
        "the doubled sentence wraps: {doubled:#}"
    );
    assert_eq!(
        doubled["lead_rows_wrapped"], true,
        "positive control: the doubled strip had to wrap: {doubled:#}"
    );
}

fn run_config_path(name: &'static str, width: u32) -> Value {
    let scenario = browser::Scenario {
        name,
        hash: "config",
        snippets: &[LAYOUT_DRIVER],
        budget_ms: LAYOUT_BUDGET_MS,
    };
    let window = format!("--window-size={width},900");
    browser::run_scenario_with(&scenario, &[&window]).result
}

/// A config path longer than its row, at 720, 1024 and 1440 px, once with
/// slashes and once as a Windows path with backslashes: it sits on a strip
/// row of its own, moves to a new line only right after a separator, is cut
/// nowhere (no overflow, nothing past the strip, the page does not scroll
/// sideways), carries no title, and both its text and a selection of it are
/// the served path exactly. Positive control: the path does wrap at every
/// width, so the separator rule was exercised.
///
/// Mutation checks: drop `display: inline-block` from `.cfgpath-piece` ->
/// red (a line breaks inside a segment); drop `grid-column` from
/// `.cfgfact--path` -> red (the path shares its row); stop `pathPieces`
/// ending a piece at a backslash -> red (the Windows path breaks inside a
/// segment).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_config_path_wraps_only_after_a_separator_and_is_never_cut() {
    let shapes = [
        ("slashes", "layout-config-path", "/|"),
        ("backslashes", "layout-config-path-windows", "\\|"),
    ];
    for (shape, scenario, after_separator) in shapes {
        for width in [720, 1024, 1440] {
            let out = run_config_path(scenario, width);
            let at = format!("{shape} at {width} px");
            assert_eq!(out["inner_width"].as_u64(), Some(u64::from(width)), "{at}");
            assert_eq!(out["text"], out["wire"], "{at}: {out:#}");
            assert_eq!(out["selected"], out["wire"], "{at}: {out:#}");
            assert_eq!(out["title"], Value::Null, "{at}: {out:#}");
            assert!(
                out["lines"].as_u64().unwrap_or(0) > 1,
                "{at}: premise, the path wraps: {out:#}"
            );
            let inside_segment: Vec<&Value> = list(&out, "breaks")
                .iter()
                .filter(|b| !b.as_str().is_some_and(|b| b.starts_with(after_separator)))
                .collect();
            assert_eq!(inside_segment, Vec::<&Value>::new(), "{at}: {out:#}");
            assert_eq!(out["cut"], false, "{at}: {out:#}");
            assert_eq!(out["right_inside_strip"], true, "{at}: {out:#}");
            assert_eq!(out["row_to_itself"], true, "{at}: {out:#}");
            assert_eq!(out["page_scrolls_sideways"], false, "{at}: {out:#}");
        }
    }
}

/// The line that declares the page's path splitter.
const PATH_PIECES_DECLARED: &str = "  function pathPieces(path) {\n";

/// Each config path shape, with the pieces the strip draws it in: a piece
/// ends at the first slash or backslash after a named segment, and the rest
/// of a run of separators (the root's included) starts the next piece.
const PATH_PIECE_ROWS: &[(&str, &str, &[&str])] = &[
    ("posix path", "/a/b/c.toml", &["/a/", "b/", "c.toml"]),
    ("separator runs", "//x//y/z", &["//x/", "/y/", "z"]),
    (
        "relative path",
        "relative/dir/f",
        &["relative/", "dir/", "f"],
    ),
    ("root alone", "/", &["/"]),
    (
        "windows path",
        "C:\\Users\\op\\config.toml",
        &["C:\\", "Users\\", "op\\", "config.toml"],
    ),
    (
        "windows share",
        "\\\\host\\share\\config.toml",
        &["\\\\host\\", "share\\", "config.toml"],
    ),
    (
        "mixed separators",
        "C:\\Users/op\\rc/config.toml",
        &["C:\\", "Users/", "op\\", "rc/", "config.toml"],
    ),
    ("trailing slash", "/etc/routectl/", &["/etc/", "routectl/"]),
    ("trailing backslash", "C:\\cfg\\", &["C:\\", "cfg\\"]),
    ("empty", "", &[]),
];

/// The page with its path splitter and the rows' paths published on
/// `window`, for the driver to call.
fn with_published_path_pieces(page: String) -> String {
    assert_eq!(
        page.matches(PATH_PIECES_DECLARED).count(),
        1,
        "the page declares its path splitter as this test expects"
    );
    let inputs: Vec<&str> = PATH_PIECE_ROWS.iter().map(|(_, path, _)| *path).collect();
    let inputs = serde_json::to_string(&inputs).expect("the paths as JSON");
    page.replace(
        PATH_PIECES_DECLARED,
        &format!(
            "  window.routectlPathPieces = pathPieces;\n  \
             window.routectlPathPieceInputs = {inputs};\n{PATH_PIECES_DECLARED}"
        ),
    )
}

/// The page cuts every row's path into the pieces listed, and the pieces
/// put back together are the path exactly, so a copy of the drawn path is
/// the served one.
///
/// Mutation check: stop `pathPieces` ending a piece at a backslash -> red
/// (the Windows rows come back as one piece).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_config_path_pieces_end_after_each_separator_and_rejoin_exactly() {
    let scenario = browser::Scenario {
        name: "layout-config-path-pieces",
        hash: "config",
        snippets: &[LAYOUT_DRIVER],
        budget_ms: LAYOUT_BUDGET_MS,
    };
    let out = browser::run_scenario_on_patched_page(&scenario, with_published_path_pieces).result;
    let drawn = out.as_array().expect("one list of pieces per path");
    assert_eq!(drawn.len(), PATH_PIECE_ROWS.len(), "{out:#}");
    for ((row, path, pieces), got) in PATH_PIECE_ROWS.iter().zip(drawn) {
        assert_eq!(*got, json!(pieces), "row `{row}`");
        let rejoined: String = got
            .as_array()
            .expect("a list of pieces")
            .iter()
            .map(|p| p.as_str().expect("a piece"))
            .collect();
        assert_eq!(rejoined, *path, "row `{row}`");
    }
}

/// The audit is not blind: over planted boxes beside the view it flags a
/// numeral cut by an ellipsis, one clipped without, one pushed past the
/// window's edge, a scroller the keyboard cannot reach (once as a bare box,
/// once as a table scroller with no focus of its own), and two labels drawn
/// over each other; it lets through the same scroller made focusable, files a
/// titled name apart from the text it hides, and reads the planted table
/// scroller as unnamed and unfocusable.
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
    assert!(
        flagged("unreachable-scroller", "routectl-planted-table-cell"),
        "{ctl:#}"
    );
    assert_eq!(
        hidden
            .iter()
            .filter(|(k, _)| *k == "unreachable-scroller")
            .count(),
        2,
        "only the unfocusable scrollers are flagged: {ctl:#}"
    );
    assert_eq!(hidden.len(), 5, "{ctl:#}");
    let planted = list(ctl, "table_scrollers");
    assert_eq!(planted.len(), 1, "{ctl:#}");
    assert_eq!(planted[0]["name"], "", "{ctl:#}");
    assert_eq!(planted[0]["self_focusable"], false, "{ctl:#}");
    assert_eq!(planted[0]["caption"], "Planted table", "{ctl:#}");
    assert_eq!(list(ctl, "titled_names").len(), 1, "{ctl:#}");
    assert_eq!(list(ctl, "collisions").len(), 1, "{ctl:#}");
}
