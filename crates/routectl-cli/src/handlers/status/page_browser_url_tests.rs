// The view state in the address: the fragment codec in headless Chrome, plus a
// source scan pinning where the script touches location and history.
// `include!`d into a child module of `page::tests` beside `browser`; every
// browser test here is `#[ignore]` and runs with
// `cargo test -p routectl-cli page_browser -- --ignored`.

use serde_json::Value;

const URL_DRIVER: &str = include_str!("page_browser_url_driver.js");

const URL_BUDGET_MS: u64 = 30_000;

/// A provider label the fixtures carry, so a scoped view has data to show.
const FIXTURE_PROVIDER: &str = "anthropic";

fn url_scenario(name: &'static str, hash: &'static str) -> browser::Scenario {
    browser::Scenario {
        name,
        hash,
        snippets: &[URL_DRIVER],
        budget_ms: URL_BUDGET_MS,
    }
}

/// A scenario that first sets `window.routectlUrlRows` to `rows`.
fn url_rows_scenario(name: &'static str, hash: &'static str, rows: &[String]) -> browser::Scenario {
    let rows_js = format!(
        "window.routectlUrlRows = {};",
        serde_json::to_string(rows).expect("rows serialize")
    );
    let snippets: &'static [&'static str] =
        Box::leak(vec![&*Box::leak(rows_js.into_boxed_str()), URL_DRIVER].into_boxed_slice());
    browser::Scenario {
        name,
        hash,
        snippets,
        budget_ms: URL_BUDGET_MS,
    }
}

fn queries(report: &Value) -> &Vec<Value> {
    report["queries"].as_array().expect("the QUERY bodies")
}

fn titles(report: &Value) -> Vec<&str> {
    report["titles"]
        .as_array()
        .expect("section titles")
        .iter()
        .map(|t| t.as_str().expect("a title is text"))
        .collect()
}

fn assert_traffic_week_alias_scoped(label: &str, report: &Value) {
    let scoped = format!("Scoped to {FIXTURE_PROVIDER}");
    assert_eq!(report["view"], "traffic", "{label}: view {report:#}");
    assert_eq!(report["range"], "Week", "{label}: range");
    assert_eq!(report["group"], "Alias", "{label}: group");
    assert!(
        titles(report).contains(&scoped.as_str()),
        "{label}: no scope strip {report:#}"
    );
    let last = queries(report).last().expect("a QUERY was issued");
    assert_eq!(last["window"], "week", "{label}: {last}");
    assert_eq!(last["group_by"], "alias", "{label}: {last}");
    assert_eq!(last["provider"], FIXTURE_PROVIDER, "{label}: {last}");
    assert_eq!(
        report["hash"],
        format!("#traffic?range=week&scope={FIXTURE_PROVIDER}&group=alias"),
        "{label}: the address is not canonical"
    );
}

/// A full address survives a reload: view, range, group and scope come back
/// and the QUERY asks for all three.
///
/// Mutation check: read `scope` as null in parseViewHash -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_reload_restores_view_range_group_and_scope() {
    let run = browser::run_scenario(&url_scenario(
        "url-reload",
        "traffic?range=week&group=alias&scope=anthropic",
    ));

    assert_traffic_week_alias_scoped("first load", &run.result["before"]);
    assert_traffic_week_alias_scoped("after reload", &run.result["after"]);
}

/// Every address is rewritten to its canonical form: the first of a
/// duplicate key wins, an unknown key or a value outside its vocabulary is
/// dropped, keys come back in their fixed order, and an old `#<tab>/<window>`
/// link keeps its window as the range. The rewrite replaces the entry: the
/// history is as long as after loading an address that was already canonical.
///
/// Mutation checks: take the last duplicate (`getAll(..).pop()`) -> red;
/// push instead of replace in syncFromLocation -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_an_address_is_rewritten_canonical() {
    let rows: &[(&'static str, &str)] = &[
        ("now?range=bogus&range=week&x=1", "#now"),
        ("now?range=week&range=bogus", "#now?range=week"),
        (
            "now?group=alias&range=month",
            "#now?range=month&group=alias",
        ),
        ("now?group=Alias", "#now"),
        ("now?sort=models.cost.desc", "#now"),
        ("now?range=%77eek", "#now?range=week"),
        ("now?range=week%", "#now"),
        ("now?range=today", "#now"),
        ("now??range=week", "#now?range=week"),
        ("nowhere?range=week", "#now?range=week"),
        ("usage/week", "#traffic?range=week"),
        ("usage/week?range=month", "#traffic?range=month"),
        ("routing/bogus", "#traffic"),
    ];
    let baseline = browser::run_scenario(&url_scenario("url-state", "now")).result;
    assert_eq!(baseline["hash"], "#now");
    for (hash, canonical) in rows {
        let report = browser::run_scenario(&url_scenario("url-state", hash)).result;

        assert_eq!(report["hash"], *canonical, "`#{hash}`: {report:#}");
        assert_eq!(
            report["history_length"], baseline["history_length"],
            "`#{hash}` added a history entry"
        );
    }
}

/// A scope that looks like markup is drawn as text, no element is made from
/// it, and the QUERY carries it decoded.
///
/// Mutation check: draw sectionHead's title with innerHTML -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_markup_scope_renders_as_text_and_reaches_the_query_decoded() {
    let run = browser::run_scenario(&url_scenario(
        "url-scope-text",
        "now?scope=%3Cb%3E%26%2B%25",
    ));
    let report = &run.result;

    assert_eq!(report["hash"], "#now?scope=%3Cb%3E%26%2B%25");
    assert!(
        titles(report).contains(&"Scoped to <b>&+%"),
        "the scope label is not the decoded text: {report:#}"
    );
    assert_eq!(report["b_elements"], 0, "the scope made an element");
    let bodies = queries(report);
    assert!(!bodies.is_empty(), "no QUERY was issued");
    for body in bodies {
        assert_eq!(body["provider"], "<b>&+%", "{body}");
    }
}

/// One row of the scope table: the encoded value in the address, the scope
/// the page settles on (None = dropped), and the canonical address.
struct ScopeCase {
    name: &'static str,
    raw: String,
    scope: Option<String>,
    hash: String,
}

fn scope_case(name: &'static str, raw: &str, scope: Option<&str>, hash: &str) -> ScopeCase {
    ScopeCase {
        name,
        raw: format!("#now?scope={raw}"),
        scope: scope.map(str::to_owned),
        hash: hash.to_owned(),
    }
}

fn scope_cases() -> Vec<ScopeCase> {
    let at_limit = "x".repeat(128);
    vec![
        scope_case(
            "plain",
            "anthropic",
            Some("anthropic"),
            "#now?scope=anthropic",
        ),
        scope_case("plus is a space", "a+b", Some("a b"), "#now?scope=a+b"),
        scope_case(
            "edges trimmed",
            "%20anthropic%09",
            Some("anthropic"),
            "#now?scope=anthropic",
        ),
        scope_case("blank dropped", "%20%20", None, "#now"),
        scope_case("next-line only dropped", "%C2%85", None, "#now"),
        scope_case("empty dropped", "", None, "#now"),
        scope_case(
            "128 chars kept",
            &at_limit,
            Some(&at_limit),
            &format!("#now?scope={at_limit}"),
        ),
        scope_case("129 chars dropped", &"x".repeat(129), None, "#now"),
        scope_case("200 chars dropped", &"y".repeat(200), None, "#now"),
        scope_case(
            "non-ASCII",
            "%E2%82%AC",
            Some("\u{20ac}"),
            "#now?scope=%E2%82%AC",
        ),
        scope_case("bad UTF-8", "%FF", Some("\u{fffd}"), "#now?scope=%EF%BF%BD"),
        scope_case("stray percent", "50%", Some("50%"), "#now?scope=50%25"),
        scope_case(
            "first duplicate",
            "first&scope=second",
            Some("first"),
            "#now?scope=first",
        ),
    ]
}

/// The scope value classes the URLSearchParams parser can produce: each is
/// trimmed, kept at up to 128 chars and dropped when blank or longer, and the
/// QUERY sent for it carries exactly the kept value or no provider at all.
///
/// Mutation check: drop the length bound in scopeParam -> red (129/200 rows).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_scope_from_the_address_is_trimmed_bounded_and_sent_as_given() {
    let cases = scope_cases();
    let rows: Vec<String> = cases.iter().map(|c| c.raw.clone()).collect();

    let run = browser::run_scenario(&url_rows_scenario("url-scope-rows", "config", &rows));

    let seen = run.result.as_array().expect("one report per row");
    assert_eq!(seen.len(), cases.len());
    for (case, report) in cases.iter().zip(seen) {
        let name = case.name;
        assert_eq!(report["view"], "now", "{name}");
        assert_eq!(report["hash"], case.hash.as_str(), "{name}: {report:#}");
        let bodies = queries(report);
        assert_eq!(bodies.len(), 1, "{name}: one QUERY per arrival {report:#}");
        let provider = bodies[0].get("provider").and_then(Value::as_str);
        assert_eq!(provider, case.scope.as_deref(), "{name}: {report:#}");
    }
}

/// One range click pushes exactly one history entry; Back restores the prior
/// range with one QUERY, and the figure shown is the one its answer carried.
///
/// Mutation check: re-apply a hashchange without a refetch -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_range_click_is_one_history_entry_and_back_restores_it() {
    let out = browser::run_scenario(&url_scenario("url-range-history", "now")).result;

    let start_len = out["start"]["history_length"].as_u64().expect("a length");
    assert_eq!(out["start"]["hash"], "#now");
    assert_eq!(out["clicked"]["history_length"], start_len + 1, "{out:#}");
    assert_eq!(out["clicked"]["hash"], "#now?range=week");
    assert_eq!(out["week"]["range"], "Week");
    assert_eq!(
        out["week"]["figure"], out["week"]["answered_requests"],
        "{out:#}"
    );

    let back = &out["back"];
    assert_eq!(back["hash"], "#now", "{out:#}");
    assert_eq!(back["range"], "Today");
    assert_eq!(back["history_length"], start_len + 1, "Back added an entry");
    let issued = out["back_issued"].as_array().expect("the QUERY bodies");
    assert_eq!(issued.len(), 1, "Back issued {issued:#?}");
    assert_eq!(issued[0]["window"], "today");
    assert_eq!(back["answered_window"], "today");
    assert_eq!(back["figure"], back["answered_requests"], "{out:#}");
    assert_ne!(
        back["figure"], out["week"]["figure"],
        "the figures never moved"
    );
}

/// Range, scope, view and group each push one entry; a control that leaves
/// the selection as it is pushes none.
///
/// Mutation check: replace instead of push in commitViewState -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_each_view_state_control_pushes_one_history_entry() {
    let steps = browser::run_scenario(&url_scenario("url-controls-history", "now")).result;

    let expected: &[(&str, u64, &str)] = &[
        ("range", 1, "#now?range=week"),
        ("scope", 1, "#now?range=week&scope=anthropic"),
        ("view", 1, "#traffic?range=week&scope=anthropic"),
        (
            "group",
            1,
            "#traffic?range=week&scope=anthropic&group=alias",
        ),
        (
            "same range again",
            0,
            "#traffic?range=week&scope=anthropic&group=alias",
        ),
    ];
    let steps = steps.as_array().expect("one report per step");
    assert_eq!(steps.len(), expected.len());
    for (step, (name, added, hash)) in steps.iter().zip(expected) {
        assert_eq!(step["name"], *name);
        assert_eq!(step["added"], *added, "`{name}`: {step:#}");
        assert_eq!(step["hash"], *hash, "`{name}`: {step:#}");
    }
}

/// An open dialog, focus, scroll and an open row leave the address and the
/// history alone; the tab click between them is the control that shows the
/// observation sees a change.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_open_rows_dialogs_focus_and_scroll_stay_out_of_the_address() {
    let obs = browser::run_scenario(&url_scenario("url-ui-state-stays-out", "now")).result;

    let obs = obs.as_array().expect("observations");
    let start_len = obs[0]["history_length"].as_u64().expect("a length");
    let expected: &[(&str, &str, u64)] = &[
        ("start", "#now", 0),
        ("dialog open, focus inside, scrolled", "#now", 0),
        ("config tab", "#config", 1),
        ("row open, focused", "#config", 1),
    ];
    assert_eq!(obs.len(), expected.len());
    for (o, (name, hash, added)) in obs.iter().zip(expected) {
        assert_eq!(o["name"], *name);
        assert_eq!(o["hash"], *hash, "`{name}`: {o:#}");
        assert_eq!(o["history_length"], start_len + added, "`{name}`: {o:#}");
    }
}

// ---- source scan: where location and history are touched -------------------

/// The spellings the guard's sites pattern matches.
const SITE_NEEDLES: &[&str] = &[
    "location.hash",
    "location.search",
    "location.href",
    "URLSearchParams",
    "history.pushState",
    "history.replaceState",
    "hashchange",
    "popstate",
    "decodeURI",
];

/// The codec functions allowed to touch location and history.
const CODEC_FUNCTIONS: &[&str] = &[
    "parseViewHash",
    "formatViewHash",
    "writeViewHash",
    "syncFromLocation",
];

/// The one listener allowed to name an address event.
const URL_LISTENER: &str =
    "window.addEventListener('hashchange', function () { syncFromLocation(true); });";

/// Byte span of `function <name>(...) { ... }` in `src`, braces balanced and
/// string literals skipped.
fn function_span(src: &str, name: &str) -> std::ops::Range<usize> {
    let start = src
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("the script declares `function {name}(`"));
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (offset, c) in src[start..].char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return start..start + offset + 1;
                }
            }
            _ => {}
        }
    }
    panic!("`{name}` body is not closed");
}

/// Every site-pattern match in `src` (comments stripped) that lies outside the
/// codec functions and the one listener statement, as `(needle, offset)`.
/// Also returns how many matches there were in total.
fn stray_url_sites(src: &str) -> (Vec<(&'static str, usize)>, usize) {
    let code = strip_js_comments(src);
    let mut allowed: Vec<std::ops::Range<usize>> = CODEC_FUNCTIONS
        .iter()
        .map(|name| function_span(&code, name))
        .collect();
    let listeners: Vec<usize> = code.match_indices(URL_LISTENER).map(|(at, _)| at).collect();
    assert_eq!(listeners.len(), 1, "exactly one address listener");
    allowed.push(listeners[0]..listeners[0] + URL_LISTENER.len());
    let mut stray = Vec::new();
    let mut total = 0;
    for needle in SITE_NEEDLES {
        for (at, _) in code.match_indices(needle) {
            total += 1;
            if !allowed.iter().any(|span| span.contains(&at)) {
                stray.push((*needle, at));
            }
        }
    }
    (stray, total)
}

/// The page touches location and history only inside the codec and its one
/// listener. Positive control: the scan finds the codec's own reads and
/// writes, so a clean result is not an empty search.
///
/// Mutation check: read `location.hash` in selectTab -> red.
#[test]
fn the_page_touches_location_and_history_only_in_the_view_state_codec() {
    let (stray, total) = stray_url_sites(&script());

    assert!(stray.is_empty(), "outside the codec: {stray:?}");
    let code = strip_js_comments(&script());
    for needle in [
        "location.hash",
        "URLSearchParams",
        "history.pushState",
        "history.replaceState",
    ] {
        assert!(code.contains(needle), "the codec no longer uses `{needle}`");
    }
    assert!(total >= 6, "the scan found only {total} sites");
}

/// The scan reports a site outside the codec, and one inside it is allowed.
#[test]
fn the_url_site_scan_flags_a_read_outside_the_codec() {
    let codec: String = CODEC_FUNCTIONS
        .iter()
        .map(|name| format!("function {name}() {{ return location.hash; }}\n"))
        .collect();
    let clean = format!("{codec}{URL_LISTENER}\n");
    let dirty = format!("{clean}function other() {{ history.pushState(null, '', '#x'); }}\n");

    assert_eq!(stray_url_sites(&clean).0, Vec::new());
    let (stray, _) = stray_url_sites(&dirty);
    assert_eq!(stray.len(), 1, "{stray:?}");
    assert_eq!(stray[0].0, "history.pushState");
}
