// The six views in headless Chrome: each renders its sections from the
// populated fixtures, an old tab token opens its successor view, and the
// window picker is live only on the windowed views. `include!`d into a child
// module of `page::tests` beside `browser`; every test here is `#[ignore]` and
// runs with `cargo test -p routectl-cli page_browser -- --ignored`.

use serde_json::Value;

const VIEWS_DRIVER: &str = include_str!("page_browser_views_driver.js");

const VIEW_BUDGET_MS: u64 = 15_000;

fn views_scenario(name: &'static str, hash: &'static str) -> browser::Scenario {
    browser::Scenario {
        name,
        hash,
        snippets: &[VIEWS_DRIVER],
        budget_ms: VIEW_BUDGET_MS,
    }
}

fn titles(report: &Value) -> Vec<&str> {
    report["titles"]
        .as_array()
        .expect("section titles")
        .iter()
        .map(|t| t.as_str().expect("a title is text"))
        .collect()
}

/// One view as the tests expect to find it: the section titles it must show,
/// the titles that belong to another view and must not, and whether its
/// figures follow the window picker.
struct ViewCase {
    id: &'static str,
    shows: &'static [&'static str],
    omits: &'static [&'static str],
    windowed: bool,
}

const VIEW_CASES: &[ViewCase] = &[
    ViewCase {
        id: "now",
        shows: &["Providers"],
        omits: &["Accounts by pool", "Fallback chains"],
        windowed: true,
    },
    ViewCase {
        id: "accounts",
        shows: &["Accounts by pool", "Background seat drivers"],
        omits: &["Model lanes", "Fallback chains"],
        windowed: true,
    },
    ViewCase {
        id: "traffic",
        shows: &["By model"],
        omits: &[
            "Accounts by pool",
            "Model lanes",
            "Fallback chains",
            "Live target state",
            "Estimated step attribution",
        ],
        windowed: true,
    },
    ViewCase {
        id: "models",
        shows: &["Model lanes"],
        omits: &["Accounts by pool", "Fallback chains", "Dispatch targets"],
        windowed: false,
    },
    ViewCase {
        id: "config",
        shows: &["Aliases", "Routing providers", "Retry classes"],
        omits: &["Accounts by pool", "Model lanes"],
        windowed: false,
    },
    ViewCase {
        id: "doctor",
        shows: &["Findings", "Capability matrix"],
        omits: &["Accounts by pool", "Model lanes"],
        windowed: false,
    },
];

fn assert_renders_content(case: &ViewCase, report: &Value) {
    let id = case.id;
    assert_eq!(report["view"], id, "`#{id}` selects its own tab");
    assert_eq!(
        report["hash"],
        format!("#{id}"),
        "`#{id}` is already canonical"
    );
    assert_eq!(
        report["visible_panes"],
        serde_json::json!([format!("pane-{id}")]),
        "only the `{id}` pane is shown"
    );
    assert_eq!(
        report["error_cards"], 0,
        "`{id}` shows an error card: {report:#}"
    );
    assert!(
        report["text_length"].as_u64().unwrap_or(0) > 0,
        "`{id}` rendered no text"
    );
    let shown = titles(report);
    assert!(!shown.is_empty(), "`{id}` rendered no section");
    for title in case.shows {
        assert!(shown.contains(title), "`{id}` lacks `{title}`: {shown:?}");
    }
    for title in case.omits {
        assert!(
            !shown.contains(title),
            "`{id}` carries `{title}`: {shown:?}"
        );
    }
}

fn assert_picker(case: &ViewCase, report: &Value) {
    let id = case.id;
    let picker = &report["picker"];
    let disabled = picker["disabled"].as_array().expect("picker buttons");
    assert_eq!(disabled.len(), 6, "the picker has its six windows");
    if case.windowed {
        assert_eq!(picker["windowless"], "false", "`{id}` dims the picker");
        assert!(
            disabled.iter().all(|d| d == false),
            "`{id}` disables a window: {picker:#}"
        );
        assert_eq!(picker["label"], Value::Null, "`{id}` shows a span label");
    } else {
        assert_eq!(
            picker["windowless"], "true",
            "`{id}` leaves the picker live"
        );
        assert!(
            disabled.iter().all(|d| d == true),
            "`{id}` leaves a window enabled: {picker:#}"
        );
        assert_eq!(
            picker["label"], "Current state",
            "`{id}` names its span beside the picker"
        );
    }
}

/// Each view opened by its own id shows its own sections and no error card,
/// and the picker is live exactly on the windowed views.
///
/// Mutation check: give `models` a null span in VIEWS -> red (picker live).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_each_view_renders_its_sections_from_the_fixtures() {
    for case in VIEW_CASES {
        let hash: &'static str = case.id;
        let report = browser::run_scenario(&views_scenario("view-content", hash)).result;

        assert_renders_content(case, &report);
        assert_picker(case, &report);
    }
}

/// The positive control for the error-card count above: a broken QUERY
/// answer shows its card inside Now while the GET-backed sections render.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_broken_source_shows_its_error_card_inside_the_view() {
    let report = browser::run_scenario(&views_scenario("view-content-query-broken", "now")).result;

    assert_eq!(report["view"], "now");
    assert_eq!(report["error_cards"], 1, "{report:#}");
    assert!(titles(&report).contains(&"Needs attention"), "{report:#}");
}

/// An old tab token opens the view that took over its content and the hash
/// is rewritten to that view's id, a window segment kept as the range.
///
/// Mutation check: drop `health: 'accounts'` from LEGACY_VIEW_IDS -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_old_tab_tokens_open_their_successor_view() {
    let rows: &[(&'static str, &str, &str)] = &[
        ("health", "accounts", "#accounts"),
        ("overview", "now", "#now"),
        ("usage", "traffic", "#traffic"),
        ("routing", "traffic", "#traffic"),
        ("usage/week", "traffic", "#traffic?range=week"),
        ("nowhere", "now", "#now"),
    ];
    for (hash, view, canonical) in rows {
        let report = browser::run_scenario(&views_scenario("view-content", hash)).result;

        assert_eq!(report["view"], *view, "`#{hash}` opened the wrong view");
        assert_eq!(report["hash"], *canonical, "`#{hash}` was not rewritten");
        assert_eq!(report["error_cards"], 0, "`#{hash}`: {report:#}");
    }
}

/// `#usage/week` keeps its window: Week is pressed and the QUERY asks for it.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_old_usage_link_with_a_window_opens_traffic_on_that_window() {
    let run = browser::run_scenario(&views_scenario("view-content", "usage/week"));

    assert_eq!(run.result["view"], "traffic");
    assert_eq!(run.result["picker"]["pressed"], "week");
    let asked_week = run.requests.iter().any(|r| {
        r["method"] == "QUERY"
            && r["body"]
                .as_str()
                .is_some_and(|b| b.contains("\"window\":\"week\""))
    });
    assert!(
        asked_week,
        "no QUERY asked for the week: {:#?}",
        run.requests
    );
}
