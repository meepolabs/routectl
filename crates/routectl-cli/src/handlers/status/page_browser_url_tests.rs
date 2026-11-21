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
        ("now?group=seat", "#now?group=seat"),
        ("now?sort=models.cost.desc", "#now"),
        ("now?range=%77eek", "#now?range=week"),
        ("now?range=week%", "#now"),
        ("now?range=today", "#now"),
        ("now?range=last30d", "#now"),
        ("now?range=Last7d", "#now"),
        ("now?range=last7d", "#now?range=last7d"),
        ("now?range=last24h", "#now?range=last24h"),
        ("usage/last24h", "#traffic?range=last24h"),
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
        scope_case(
            "padding inside the raw bound trimmed",
            &format!("{}x", "%20".repeat(511)),
            Some("x"),
            "#now?scope=x",
        ),
        scope_case(
            "padding past the raw bound dropped",
            &format!("{}x", "%20".repeat(512)),
            None,
            "#now",
        ),
        scope_case(
            "zero-width space dropped",
            "anthro%E2%80%8Bpic",
            None,
            "#now",
        ),
        scope_case("bidi override dropped", "%E2%80%AEcipohtna", None, "#now"),
        scope_case("C0 control dropped", "anthro%01pic", None, "#now"),
        scope_case("inner tab dropped", "anthro%09pic", None, "#now"),
        scope_case("inner next-line dropped", "anthro%C2%85pic", None, "#now"),
        scope_case(
            "combining grapheme joiner dropped",
            "anthro%CD%8Fpic",
            None,
            "#now",
        ),
        scope_case(
            "variation selector dropped",
            "anthropic%EF%B8%8F",
            None,
            "#now",
        ),
        scope_case("hangul filler dropped", "%E3%85%A4", None, "#now"),
        scope_case("line separator dropped", "anthro%E2%80%A8pic", None, "#now"),
        scope_case(
            "inner no-break space dropped",
            "anthro%C2%A0pic",
            None,
            "#now",
        ),
        scope_case(
            "blank braille pattern dropped",
            "anthro%E2%A0%80pic",
            None,
            "#now",
        ),
    ]
}

/// The scope value classes the URLSearchParams parser can produce: each is
/// trimmed, kept at up to 128 chars and dropped when blank or longer, longer
/// than 512 before the trim, or carrying a character that draws nothing or
/// draws as a plain space (control, format, default-ignorable, line or
/// paragraph separator, inner whitespace other than U+0020, the blank
/// Braille pattern U+2800), and the QUERY
/// sent for it carries exactly the kept value or no provider at all.
/// Non-ASCII text that is none of these (the euro sign, U+FFFD) is kept, as
/// is an inner U+0020 (the plus row).
///
/// Mutation checks: drop the length bound in scopeParam -> red (129/200
/// rows); drop the hidden-character test -> red (every dropped-character
/// row); drop `\p{Default_Ignorable_Code_Point}` from it -> red (the joiner,
/// variation selector and filler rows); drop `\u2800` from it -> red (the
/// blank Braille row); drop the raw bound -> red (past-the-raw-bound row).
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

/// A scope far past the raw bound is dropped without the edge trim ever
/// running. Each row is one page load on that address, timed across the
/// page's own load handler, which reads it; the address is then canonical.
///
/// Rows: 300,000 spaces then one letter (the shape a long link takes), and a
/// letter, 300,000 spaces and a letter, the input the edge-trim regex
/// backtracks on (the trim alone takes over a second at 80,000 spaces in
/// Chrome). Positive control: the plain-scope row is kept, so the handler
/// read the address the row wrote.
///
/// The bound sits far above a dropped load under machine load (single-digit
/// ms) and far below the trim running first (seconds at 300,000 spaces).
///
/// Mutation check: run the edge trim before the raw bound -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_an_overlong_scope_is_dropped_before_any_regex_runs() {
    const OVERLONG_SCOPE_BOUND_MS: f64 = 500.0;
    let spaces = "+".repeat(300_000);
    let rows = [
        (
            "plain scope",
            "#config?scope=anthropic".to_owned(),
            "#config?scope=anthropic",
        ),
        (
            "spaces then a letter",
            format!("#config?scope={spaces}b"),
            "#config",
        ),
        (
            "spaces between letters",
            format!("#config?scope=a{spaces}b"),
            "#config",
        ),
    ];
    for (name, address, canonical) in rows {
        let set = format!(
            "window.routectlUrlLoadAddress = {};",
            serde_json::to_string(&address).expect("an address serializes")
        );
        let snippets: &'static [&'static str] =
            Box::leak(vec![&*Box::leak(set.into_boxed_str()), URL_DRIVER].into_boxed_slice());
        let scenario = browser::Scenario {
            name: "url-scope-load-timing",
            hash: "config",
            snippets,
            budget_ms: URL_BUDGET_MS,
        };

        let report = browser::run_scenario(&scenario).result;

        assert_eq!(report["view"], "config", "`{name}`");
        assert_eq!(report["hash"], canonical, "`{name}`");
        let ms = report["handled_ms"].as_f64().expect("a duration");
        assert!(
            ms < OVERLONG_SCOPE_BOUND_MS,
            "`{name}`: the load handler took {ms} ms"
        );
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

/// The QUERY_SHAPES row the page must send for a window, grouping and series
/// mode, read from the script's own declaration.
fn declared_shape(window: &str, group_by: &str, bucketed: bool) -> Value {
    let shapes: Vec<Value> = json_object_literals(&literal_body("QUERY_SHAPES", '[', ']'))
        .iter()
        .map(|shape| serde_json::from_str(shape).expect("a shape is JSON"))
        .filter(|shape: &Value| {
            shape["window"] == window
                && shape["group_by"] == group_by
                && shape.get("bucket").is_some() == bucketed
        })
        .collect();
    assert_eq!(
        shapes.len(),
        1,
        "one `{window}`/`{group_by}` shape (series: {bucketed})"
    );
    shapes[0].clone()
}

/// The trailing ranges, each with what the verdict strip says it spans.
const TRAILING_RANGES: &[(&str, &str)] = &[
    ("last24h", "in the last 24 hours"),
    ("last7d", "in the last 7 days"),
];

/// Picking a trailing range on `view` sends exactly the declared shape for
/// it, the address carries it, and the strip's count reads its span.
fn assert_trailing_ranges_query(view: &'static str, group_by: &str, bucketed: bool) {
    let rows: Vec<String> = TRAILING_RANGES
        .iter()
        .map(|(range, _)| (*range).to_owned())
        .collect();

    let run = browser::run_scenario(&url_rows_scenario("url-range-pick", view, &rows));

    let picks = run.result.as_array().expect("one report per range");
    assert_eq!(picks.len(), TRAILING_RANGES.len());
    for (pick, (range, span)) in picks.iter().zip(TRAILING_RANGES) {
        let at = format!("{view} {range}");
        assert_eq!(pick["view"], view, "{at}: {pick:#}");
        assert_eq!(pick["hash"], format!("#{view}?range={range}"), "{at}");
        let bodies = queries(pick);
        assert!(!bodies.is_empty(), "{at}: no QUERY after the click");
        let want = declared_shape(range, group_by, bucketed);
        for body in bodies {
            assert_eq!(body, &want, "{at}: the body is not the declared shape");
        }
        let stats = pick["stats"].as_str().expect("the strip's req/span text");
        assert!(
            stats.ends_with(&format!(" req {span}")),
            "{at}: the strip reads `{stats}`"
        );
    }
}

/// On Now, the trailing ranges ask for the provider series at the declared
/// bucket, and the strip names the span.
///
/// Mutation check: give the last7d body a `day` bucket in bodyFromShape ->
/// red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_sends_the_declared_series_shape_for_each_trailing_range() {
    assert_trailing_ranges_query("now", "provider", true);
}

/// On Traffic, the trailing ranges ask for the grouping in view without a
/// series, and the strip names the span.
///
/// Mutation check: map `last24h` to `today` in WINDOW_SPAN -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_sends_the_declared_grouped_shape_for_each_trailing_range() {
    assert_trailing_ranges_query("traffic", "model", false);
}

/// A trailing range in the address survives a reload: the picker shows it
/// pressed and the QUERY asks for it.
///
/// Mutation check: drop `last7d` from WINDOWS -> red (here and in the
/// canonical-address rows).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_reload_keeps_a_trailing_range_selected() {
    let run = browser::run_scenario(&url_scenario("url-reload", "now?range=last7d"));

    for side in ["before", "after"] {
        let report = &run.result[side];
        assert_eq!(report["hash"], "#now?range=last7d", "{side}: {report:#}");
        assert_eq!(report["range"], "7d", "{side}");
        let last = queries(report).last().expect("a QUERY was issued");
        assert_eq!(last["window"], "last7d", "{side}: {last}");
    }
}

/// What each range covers, as the line beside the picker shows it.
const COVERS: &[(&str, &str)] = &[
    ("last24h", "Last 24 hours: the 24 hours up to now"),
    ("week", "Week: since Monday 00:00 local time"),
    ("last7d", "Last 7 days: the 168 hours up to now"),
    ("month", "Month: since the 1st at 00:00 local time"),
    ("all", "All: every request on record"),
    ("today", "Today: since 00:00 local time"),
];

/// Each of the six ranges, once picked, shows its own covers line beside the
/// picker; on a view the picker does not apply to the line is gone, which is
/// the control that the check sees it hidden.
///
/// Mutation check: return early from syncWindowCovers -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_each_range_says_what_it_covers_beside_the_picker() {
    let rows: Vec<String> = COVERS
        .iter()
        .map(|(range, _)| (*range).to_owned())
        .collect();

    let out = browser::run_scenario(&url_rows_scenario("url-range-covers", "now", &rows)).result;

    let seen = out["ranges"].as_array().expect("one report per range");
    assert_eq!(seen.len(), COVERS.len());
    for (report, (range, text)) in seen.iter().zip(COVERS) {
        assert_eq!(report["range"], *range, "{report:#}");
        assert_eq!(report["text"], *text, "`{range}`");
        assert_eq!(report["shown"], true, "`{range}`: {report:#}");
        assert_eq!(report["beside_picker"], true, "`{range}`");
    }
    let windowless = &out["windowless"];
    assert_eq!(windowless["shown"], false, "{windowless:#}");
    assert_eq!(windowless["text"], "", "{windowless:#}");
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

/// Focus, scroll and an open row leave the address and the history alone;
/// the tab click between them is the control that shows the observation
/// sees a change.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_open_rows_focus_and_scroll_stay_out_of_the_address() {
    let obs = browser::run_scenario(&url_scenario("url-ui-state-stays-out", "now")).result;

    let obs = obs.as_array().expect("observations");
    let start_len = obs[0]["history_length"].as_u64().expect("a length");
    let expected: &[(&str, &str, u64)] = &[
        ("start", "#now", 0),
        ("focused, scrolled", "#now", 0),
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

/// The spellings the guard's sites pattern matches, plus the other ways a
/// script reaches the address: the document's own URL, navigation by method,
/// and a bracket access on location, history or a global holder.
///
/// This is a text scan, so it covers spellings, not the class: a member name
/// or event name computed at run time (`'hash' + 'change'`, a key held in a
/// variable) is out of its reach. The bracket needles flag every computed
/// access on an object that holds the address, so such a name can only reach
/// the address through code the scan reports.
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
    "document.URL",
    "documentURI",
    "baseURI",
    "location.assign",
    "location.replace",
    "location[",
    "history[",
    "window[",
    "self[",
    "globalThis[",
    "document[",
];

/// Globals that hold the address. Any code reference to one, not only the
/// spellings above, is a site: `var h = history; h.back()` or `location = x`.
const ADDRESS_GLOBALS: &[&str] = &["location", "history", "navigation"];

/// Objects through which a global is reached as a member: `window.location`
/// is the same object as `location`, and `top.location` is another frame's.
const GLOBAL_HOLDERS: &[&str] = &[
    "window",
    "self",
    "globalThis",
    "document",
    "top",
    "parent",
    "opener",
    "frames",
];

/// Offsets in `code` (comments already stripped) of each identifier `name`
/// that refers to the global: outside string literals, not part of a longer
/// identifier, and either bare or a member of a global holder (`x.history` on
/// any other object is not the global).
fn global_references(code: &str, name: &str) -> Vec<usize> {
    let chars: Vec<(usize, char)> = code.char_indices().collect();
    let word: Vec<char> = name.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        if matches!(c, '\'' | '"' | '`') {
            let plain: Vec<char> = chars.iter().map(|(_, c)| *c).collect();
            i = skip_string_literal(&plain, i);
            continue;
        }
        let at_word = chars.len() - i >= word.len()
            && chars[i..i + word.len()]
                .iter()
                .map(|(_, c)| *c)
                .eq(word.iter().copied());
        let starts = i == 0 || !is_ident_char(chars[i - 1].1);
        let ends = chars
            .get(i + word.len())
            .is_none_or(|(_, c)| !is_ident_char(*c));
        if at_word && starts && ends {
            let mut j = i;
            while j > 0 && chars[j - 1].1.is_whitespace() {
                j -= 1;
            }
            if j == 0 || chars[j - 1].1 != '.' || member_of_global_holder(&chars[..j - 1]) {
                found.push(chars[i].0);
            }
            i += word.len();
            continue;
        }
        i += 1;
    }
    found
}

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
    let mut sites: Vec<(&'static str, usize)> = Vec::new();
    for needle in SITE_NEEDLES {
        sites.extend(code.match_indices(needle).map(|(at, _)| (*needle, at)));
    }
    for name in ADDRESS_GLOBALS {
        sites.extend(
            global_references(&code, name)
                .into_iter()
                .map(|at| (*name, at)),
        );
    }
    let total = sites.len();
    let stray = sites
        .into_iter()
        .filter(|(_, at)| !allowed.iter().any(|span| span.contains(at)))
        .collect();
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

/// Whether `before` (the text ahead of a `.`) ends in a global holder name.
fn member_of_global_holder(before: &[(usize, char)]) -> bool {
    let mut end = before.len();
    while end > 0 && before[end - 1].1.is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_ident_char(before[start - 1].1) {
        start -= 1;
    }
    let object: String = before[start..end].iter().map(|(_, c)| *c).collect();
    let preceded_by_dot = start > 0 && before[start - 1].1 == '.';
    !preceded_by_dot && GLOBAL_HOLDERS.contains(&object.as_str())
}

/// A synthetic page whose codec functions all read `location.hash`, plus the
/// one listener, followed by `extra`.
fn synthetic_codec_page(extra: &str) -> String {
    let codec: String = CODEC_FUNCTIONS
        .iter()
        .map(|name| format!("function {name}() {{ return location.hash; }}\n"))
        .collect();
    format!("{codec}{URL_LISTENER}\n{extra}")
}

/// Each spelling outside the codec is reported under the needle or global
/// that names it. Every row also yields the bare-global hit where it spells
/// `location` or `history`, so the row's expected list is exact.
#[test]
fn the_url_site_scan_flags_a_read_outside_the_codec() {
    let rows: &[(&str, &str, &[&str])] = &[
        (
            "pushState",
            "history.pushState(null, '', '#x');",
            &["history.pushState", "history"],
        ),
        ("document URL", "var u = document.URL;", &["document.URL"]),
        (
            "assign",
            "location.assign('#x');",
            &["location.assign", "location"],
        ),
        (
            "replace",
            "location.replace('#x');",
            &["location.replace", "location"],
        ),
        (
            "location bracket",
            "var h = location['hash'];",
            &["location[", "location"],
        ),
        (
            "history bracket",
            "history['back']();",
            &["history[", "history"],
        ),
        ("history alias", "var h = history; h.back();", &["history"]),
        ("location write", "location = '#x';", &["location"]),
        ("window member", "var l = window.location;", &["location"]),
        (
            "document member",
            "document.location.hash = 'x';",
            &["location.hash", "location"],
        ),
        ("top member", "top.location = '#x';", &["location"]),
        ("parent member", "var l = parent.location;", &["location"]),
        ("opener member", "opener.history.back();", &["history"]),
        ("frames member", "var l = frames.location;", &["location"]),
        ("navigation", "navigation.navigate('#x');", &["navigation"]),
        (
            "document URI",
            "var u = document.documentURI;",
            &["documentURI"],
        ),
        ("base URI", "var u = document.baseURI;", &["baseURI"]),
        (
            "window computed",
            "var h = window['location'].hash;",
            &["window["],
        ),
        ("self computed", "var h = self[key];", &["self["]),
        (
            "globalThis computed",
            "var h = globalThis[key];",
            &["globalThis["],
        ),
        (
            "document computed",
            "var u = document[key];",
            &["document["],
        ),
    ];
    for (name, extra, expected) in rows {
        let src = synthetic_codec_page(&format!("function other() {{ {extra} }}\n"));

        let mut flagged: Vec<&str> = stray_url_sites(&src).0.iter().map(|(n, _)| *n).collect();
        flagged.sort_unstable();
        let mut want = expected.to_vec();
        want.sort_unstable();

        assert_eq!(flagged, want, "`{name}`: {extra}");
    }
}

/// The negative controls: the codec's own uses, a member of another object
/// named like a global, a longer identifier, a global's bare name inside a
/// string, and anything in a comment are not sites. (A full needle such as
/// `location.hash` is flagged even inside a string: a string can become a
/// bracket key or an evaluated name.)
#[test]
fn the_url_site_scan_ignores_members_strings_comments_and_the_codec() {
    let rows: &[(&str, &str)] = &[
        ("codec only", ""),
        (
            "member of another object",
            "var n = rec.history.length; x.location = 1; rec.top.location = 1;",
        ),
        (
            "holder names as plain words",
            "var top = wrap.scrollTop; var parent = node.parentNode; frames.push(1);",
        ),
        (
            "longer identifier",
            "var historyRows = 1; var relocation = 2;",
        ),
        (
            "bare words in a string",
            "var label = 'all-history usage at this location';",
        ),
        ("comment", "// history.pushState(location.hash)\n"),
    ];
    for (name, extra) in rows {
        let src = synthetic_codec_page(extra);

        let (stray, total) = stray_url_sites(&src);

        assert_eq!(stray, Vec::new(), "`{name}`");
        assert!(
            total >= CODEC_FUNCTIONS.len(),
            "`{name}`: the codec reads went unseen"
        );
    }
}
