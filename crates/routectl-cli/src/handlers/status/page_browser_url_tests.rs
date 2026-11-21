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

/// A scenario that first sets each `window.<name>` of `vars` to its value.
fn url_vars_scenario(
    name: &'static str,
    hash: &'static str,
    vars: &[(&str, Value)],
) -> browser::Scenario {
    let set: String = vars
        .iter()
        .map(|(var, value)| format!("window.{var} = {value};\n"))
        .collect();
    let snippets: &'static [&'static str] =
        Box::leak(vec![&*Box::leak(set.into_boxed_str()), URL_DRIVER].into_boxed_slice());
    browser::Scenario {
        name,
        hash,
        snippets,
        budget_ms: URL_BUDGET_MS,
    }
}

/// A scope no config entry names is kept once the ledger's own unscoped
/// provider read carries it as a label; one that looks like markup is then
/// drawn as text, no element is made from it, and the QUERY carries it
/// decoded. No QUERY carries it before the ledger read named it.
///
/// Mutation check: draw sectionHead's title with innerHTML -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_markup_scope_from_the_ledger_renders_as_text_and_reaches_the_query_decoded() {
    let label = "<b>&+%";
    let run = browser::run_scenario(&url_vars_scenario(
        "url-scope-text",
        "now?scope=%3Cb%3E%26%2B%25",
        &[("routectlUrlLedgerLabel", Value::from(label))],
    ));
    let report = &run.result;

    assert_eq!(report["hash"], "#now?scope=%3Cb%3E%26%2B%25");
    assert!(
        titles(report).contains(&"Scoped to <b>&+%"),
        "the scope label is not the decoded text: {report:#}"
    );
    assert_eq!(report["b_elements"], 0, "the scope made an element");
    let bodies = queries(report);
    let first_scoped = bodies
        .iter()
        .position(|b| b.get("provider").is_some())
        .expect("a scoped QUERY");
    assert!(
        first_scoped > 0,
        "the scope was sent before the ledger named it"
    );
    for body in &bodies[..first_scoped] {
        assert_eq!(body["group_by"], "provider", "the label read: {body}");
    }
    for body in &bodies[first_scoped..] {
        assert_eq!(body["provider"], label, "{body}");
    }
}

/// One row of the address table: the address followed, what each QUERY sent
/// for it carries as `provider` and `model` (None = absent), and the
/// canonical address it is rewritten to.
struct ParamCase {
    name: &'static str,
    view: &'static str,
    raw: String,
    scope: Option<&'static str>,
    model: Option<&'static str>,
    hash: String,
}

fn param_case(
    name: &'static str,
    query: &str,
    scope: Option<&'static str>,
    model: Option<&'static str>,
    hash: &str,
) -> ParamCase {
    param_case_on("traffic", name, query, scope, model, hash)
}

/// A row followed on `view`. Now reads the providers unscoped, so a scope no
/// config entry names is decided there against the ledger's labels.
fn param_case_on(
    view: &'static str,
    name: &'static str,
    query: &str,
    scope: Option<&'static str>,
    model: Option<&'static str>,
    hash: &str,
) -> ParamCase {
    ParamCase {
        name,
        view,
        raw: format!("#{view}?{query}"),
        scope,
        model,
        hash: hash.to_owned(),
    }
}

/// The value classes URLSearchParams can produce for `scope` and `model`.
/// Positive rows are the configured names spelled as the page writes them;
/// every other value is dropped, including one that decodes to a configured
/// name through another spelling, and the label the ledger gives requests
/// that reached no provider.
fn param_cases() -> Vec<ParamCase> {
    vec![
        param_case(
            "model and scope round trip",
            "scope=anthropic&model=sonnet",
            Some("anthropic"),
            Some("sonnet"),
            "#traffic?scope=anthropic&model=sonnet",
        ),
        param_case(
            "keys in another order",
            "model=seat-sonnet&scope=claude-seat",
            Some("claude-seat"),
            Some("seat-sonnet"),
            "#traffic?scope=claude-seat&model=seat-sonnet",
        ),
        param_case(
            "model alone",
            "model=glm",
            None,
            Some("glm"),
            "#traffic?model=glm",
        ),
        param_case("unknown nickname", "model=nosuch", None, None, "#traffic"),
        param_case(
            "provider entry as a model",
            "model=anthropic",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "Cyrillic look-alike model",
            "model=se%D0%B0t-sonnet",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "plus-encoded model",
            "model=+sonnet",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "plus inside a model",
            "model=seat+sonnet",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "percent-encoded model",
            "model=%73onnet",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "duplicate model",
            "model=sonnet&model=sonnet",
            None,
            None,
            "#traffic",
        ),
        param_case("empty model", "model=", None, None, "#traffic"),
        param_case("bad UTF-8 model", "model=%FF", None, None, "#traffic"),
        param_case("unknown scope", "scope=nosuch", None, None, "#traffic"),
        param_case(
            "nickname as a scope",
            "scope=sonnet",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "Cyrillic look-alike scope",
            "scope=%D0%B0nthropic",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "plus-encoded scope",
            "scope=anthropic+",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "percent-encoded scope",
            "scope=%61nthropic",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "duplicate scope",
            "scope=anthropic&scope=compat",
            None,
            None,
            "#traffic",
        ),
        param_case(
            "zero-width space in a scope",
            "scope=anthro%E2%80%8Bpic",
            None,
            None,
            "#traffic",
        ),
        param_case_on(
            "now",
            "provider entry as a scope on Now",
            "scope=anthropic",
            Some("anthropic"),
            None,
            "#now?scope=anthropic",
        ),
        param_case_on(
            "now",
            "the unattributed ledger label as a scope",
            "scope=%28unattributed%29",
            None,
            None,
            "#now",
        ),
        param_case(
            "a dropped model keeps a kept scope",
            "scope=compat&model=n%D0%BEsuch",
            Some("compat"),
            None,
            "#traffic?scope=compat",
        ),
    ]
}

/// Each address is followed from Config once the config panel is live: a
/// `scope` that is a configured provider entry and a `model` that is a
/// configured nickname, spelled once and as the page writes it, reach every
/// QUERY sent for the address and stay in it; any other value never reaches
/// a QUERY and is cut from the address.
///
/// Mutation checks: read the value with `params.get` in canonicalParam ->
/// red (the percent-encoded and duplicate rows); return keep for every model
/// in addressVerdict -> red (the unknown, look-alike and plus rows); leave
/// `model` out of bodyFromShape -> red (the positive rows); drop the
/// isScopeLabel filter in noteLedgerProviders -> red (the unattributed row).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_scope_and_model_from_the_address_are_kept_only_in_their_live_sets() {
    let cases = param_cases();
    let rows: Vec<String> = cases.iter().map(|c| c.raw.clone()).collect();

    let run = browser::run_scenario(&url_rows_scenario("url-param-rows", "config", &rows));

    let seen = run.result.as_array().expect("one report per row");
    assert_eq!(seen.len(), cases.len());
    for (case, report) in cases.iter().zip(seen) {
        let name = case.name;
        assert_eq!(report["view"], case.view, "{name}");
        assert_eq!(report["hash"], case.hash.as_str(), "{name}: {report:#}");
        let bodies = queries(report);
        assert!(!bodies.is_empty(), "{name}: no QUERY for the address");
        for body in bodies {
            let provider = body.get("provider").and_then(Value::as_str);
            let model = body.get("model").and_then(Value::as_str);
            assert_eq!(provider, case.scope, "{name}: {body}");
            assert_eq!(model, case.model, "{name}: {body}");
        }
    }
}

/// The provider card of the group the ledger leaves unattributed carries no
/// scope action: clicking it leaves the address and every QUERY unscoped.
/// Positive control: the anthropic card, clicked next, scopes both.
///
/// Mutation check: set the scope action on every card in provCardShell ->
/// red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_unattributed_provider_card_sets_no_scope() {
    let run = browser::run_scenario(&url_vars_scenario(
        "url-card-scope",
        "now",
        &[("routectlUrlCardLabel", Value::from("(unattributed)"))],
    ));
    let out = &run.result;

    let placeholder = &out["placeholder"];
    assert_eq!(placeholder["action"], Value::Null, "{out:#}");
    assert_eq!(placeholder["disabled"], true, "{out:#}");
    assert_eq!(placeholder["hash"], "#now", "{out:#}");
    for body in queries(placeholder) {
        assert!(body.get("provider").is_none(), "{body}");
    }
    let provider = &out["provider"];
    assert_eq!(provider["hash"], "#now?scope=anthropic", "{out:#}");
    assert!(
        queries(provider)
            .iter()
            .any(|b| b["provider"] == FIXTURE_PROVIDER),
        "the control scoped the QUERY: {out:#}"
    );
}

/// While the config panel is not live, the set a `scope` or `model` value
/// must be in is unknown: the value stays in the address, but no QUERY
/// carries it and the strip does not name it. Once config answers, both are
/// checked, kept, and the next QUERY carries them.
///
/// Mutation check: keep a model in addressVerdict while the nicknames are
/// unknown -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_no_query_carries_an_address_scope_or_model_before_config_is_live() {
    let run = browser::run_scenario(&url_vars_scenario(
        "url-held-before-config",
        "traffic?scope=anthropic&model=sonnet",
        &[("routectlUrlConfigDown", Value::from(2))],
    ));
    let out = &run.result;

    let down = &out["down"];
    assert_eq!(
        down["hash"], "#traffic?scope=anthropic&model=sonnet",
        "{out:#}"
    );
    let held = down["queries"].as_array().expect("the QUERY bodies");
    assert!(
        !held.is_empty(),
        "positive control: Traffic asked while config was down"
    );
    for body in held {
        assert!(
            body.get("provider").is_none(),
            "an unchecked scope was sent: {body}"
        );
        assert!(
            body.get("model").is_none(),
            "an unchecked model was sent: {body}"
        );
    }
    assert!(
        !titles(down).iter().any(|t| t.starts_with("Scoped to")),
        "the strip named an unchecked scope: {out:#}"
    );

    let live = &out["live"];
    assert_eq!(live["hash"], "#traffic?scope=anthropic&model=sonnet");
    let last = queries(live).last().expect("a QUERY");
    assert_eq!(last["provider"], "anthropic", "{last}");
    assert_eq!(last["model"], "sonnet", "{last}");
    assert!(
        titles(live).contains(&"Scoped to model sonnet on provider anthropic"),
        "{live:#}"
    );
}

/// The strip reads "model X on provider Y"; its one reset, activated from
/// the keyboard, lifts both scopes in one history entry, the next QUERY
/// carries neither, and focus lands on the pressed Group by button rather
/// than the page body, where the next refresh leaves it.
///
/// Mutation checks: lift only the provider in onProviderScope's reset ->
/// red; drop the `landScopeResetFocus(node);` call in onProviderScope -> red
/// (focus falls to the body).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_scope_strip_names_model_and_provider_and_its_reset_clears_both() {
    let out = browser::run_scenario(&url_scenario(
        "url-scope-strip-reset",
        "traffic?scope=claude-seat&model=seat-sonnet",
    ))
    .result;

    assert!(
        titles(&out["before"]).contains(&"Scoped to model seat-sonnet on provider claude-seat"),
        "{out:#}"
    );
    assert_eq!(out["reset_text"], "all models and providers");
    assert_eq!(out["added"], 1, "{out:#}");
    let after = &out["after"];
    assert_eq!(after["hash"], "#traffic", "{out:#}");
    assert_eq!(after["reset_present"], false, "{out:#}");
    assert_eq!(out["focused_before"], "usage-scope-reset", "{out:#}");
    assert_eq!(out["focused_at_once"], "usage-groupby-kmodel", "{out:#}");
    assert_eq!(after["focused"], "usage-groupby-kmodel", "{out:#}");
    assert_eq!(
        out["focused_after_refresh"], "usage-groupby-kmodel",
        "{out:#}"
    );
    let bodies = queries(after);
    assert!(!bodies.is_empty(), "no QUERY after the reset");
    for body in bodies {
        assert!(body.get("provider").is_none(), "{body}");
        assert!(body.get("model").is_none(), "{body}");
    }
}

/// Model A's figures are on screen and A's next answer is held; the address
/// moves to model B. Until B's own answer lands, nothing under B's label
/// shows a figure from any A answer, and A's held answer, handed over after
/// B's, never paints.
///
/// Mutation check: drop `model` from queryBodyKey -> red (A's figures stay
/// under B's label while B's answer is out).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_held_answer_for_model_a_never_paints_under_model_b() {
    let out = browser::run_scenario(&url_vars_scenario(
        "url-model-switch-held",
        "traffic?model=sonnet",
        &[(
            "routectlUrlModelSwitch",
            serde_json::json!(["sonnet", "glm"]),
        )],
    ))
    .result;

    let answers = out["answers"].as_array().expect("the answers");
    let a_figures: Vec<String> = answers
        .iter()
        .filter(|a| a["model"] == "sonnet")
        .map(|a| format!("{} req ", a["requests"]))
        .collect();
    let b_figures: Vec<String> = answers
        .iter()
        .filter(|a| a["model"] == "glm")
        .map(|a| format!("{} req ", a["requests"]))
        .collect();
    assert!(
        answers
            .iter()
            .any(|a| a["model"] == "sonnet" && a["held"] == true),
        "positive control: an A answer was held across the switch: {out:#}"
    );
    assert!(!b_figures.is_empty(), "B was answered: {out:#}");
    assert!(
        a_figures.iter().all(|a| !b_figures.contains(a)),
        "positive control: A and B answers carry different figures: {out:#}"
    );
    let a_stats = out["a_shown"]["stats"].as_str().expect("A's strip text");
    assert!(
        a_figures.iter().any(|f| a_stats.starts_with(f.as_str())),
        "positive control: an A figure was on screen before the switch: {out:#}"
    );

    let samples = out["samples"].as_array().expect("the samples");
    let under_b: Vec<&Value> = samples
        .iter()
        .filter(|s| s["title"] == "Scoped to model glm")
        .collect();
    assert!(!under_b.is_empty(), "B's label never showed: {out:#}");
    for sample in &under_b {
        let stats = sample["stats"].as_str().expect("stats text");
        assert!(
            !a_figures.iter().any(|f| stats.starts_with(f.as_str())),
            "an A figure showed under B: {sample} in {out:#}"
        );
    }
    let last = samples.last().expect("a final sample");
    let last_stats = last["stats"].as_str().expect("stats text");
    assert!(
        b_figures.iter().any(|f| last_stats.starts_with(f.as_str())),
        "B's own answer is on screen at the end: {last}"
    );
}

/// A scope far past any provider name is dropped without the page doing
/// more than linear work on it. Each row is one page load on that address,
/// timed across the page's own load handler, which reads it; the address is
/// then canonical once config answers.
///
/// Rows: 300,000 spaces then one letter (the shape a long link takes), and a
/// letter, 300,000 spaces and a letter, the input an edge-trim regex
/// backtracks on (such a trim alone takes over a second at 80,000 spaces in
/// Chrome). Positive control: the plain-scope row is kept, so the handler
/// read the address the row wrote.
///
/// The bound sits far above a dropped load under machine load (single-digit
/// ms) and far below a backtracking pass (seconds at 300,000 spaces).
/// Mutation check: run a backtracking edge trim (`/^\s+|\s+$/g`) over the
/// value at the top of canonicalParam -> red.
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

// ---- source scan: every address value reaches a closed set -----------------

/// Each read of an address value (`params.get(` or `params.getAll(`): the
/// function it sits in and the closed-set check its line hands it to.
/// Nothing is excluded: every site is checked.
const ADDRESS_VALUE_SITES: &[(&str, &str, &str)] = &[
    // WINDOWS
    ("params.get('range')", "parseViewHash", "firstWindow("),
    // GROUP_BYS
    ("params.get('group')", "parseViewHash", "groupParam("),
    // SORT_TABLES
    ("params.get('sort')", "parseViewHash", "sortParam("),
    // scope and model: one canonical spelling here, then the live sets in
    // addressVerdict (the parseViewHash and syncFromLocation pins below)
    ("params.getAll(key)", "canonicalParam", "params.getAll(key)"),
];

/// The address-value reads in `code` (comments stripped): the read, the
/// innermost function it sits in, and the text of its line.
fn address_value_reads(code: &str) -> Vec<(String, String, String)> {
    let mut reads = Vec::new();
    for needle in ["params.get(", "params.getAll("] {
        for (at, _) in code.match_indices(needle) {
            let close = code[at..].find(')').map_or(code.len(), |i| at + i + 1);
            let line_start = code[..at].rfind('\n').map_or(0, |i| i + 1);
            let line_end = code[at..].find('\n').map_or(code.len(), |i| at + i);
            reads.push((
                code[at..close].to_owned(),
                enclosing_function(code, at),
                code[line_start..line_end].to_owned(),
            ));
        }
    }
    reads.sort();
    reads
}

/// The name of the innermost `function <name>(` whose body holds `at`, or
/// an empty string at the top level.
fn enclosing_function(code: &str, at: usize) -> String {
    code[..at]
        .match_indices("function ")
        .filter_map(|(start, _)| {
            let rest = &code[start + "function ".len()..];
            let name = &rest[..rest.find('(')?];
            let is_name = !name.is_empty() && name.chars().all(is_ident_char);
            (is_name && function_span(code, name).contains(&at)).then(|| name.to_owned())
        })
        .last()
        .unwrap_or_default()
}

/// Every address value the page reads is handed to a closed-set check, and
/// the list above is every such read: a new `params.get(` site fails here
/// until it is listed with its check. `scope` and `model` are read only
/// through canonicalParam, which takes a value only when the key appears
/// once and is not empty, and never reach the selection before admitHeld
/// has checked them against their live sets.
///
/// Mutation checks: read `scope` with `params.get('scope')` in parseViewHash
/// -> red (an unlisted site); move the parsed scope straight into the
/// selection in syncFromLocation -> red.
#[test]
fn every_address_value_the_page_reads_reaches_a_closed_set_check() {
    let code = strip_js_comments(&script());

    let reads = address_value_reads(&code);

    let mut found: Vec<&str> = reads.iter().map(|(site, _, _)| site.as_str()).collect();
    let mut listed: Vec<&str> = ADDRESS_VALUE_SITES
        .iter()
        .map(|(site, _, _)| *site)
        .collect();
    found.sort_unstable();
    listed.sort_unstable();
    assert_eq!(found, listed, "the address-value reads changed");
    for (site, function, line) in &reads {
        let (_, want_fn, check) = ADDRESS_VALUE_SITES
            .iter()
            .find(|(listed, _, _)| listed == site)
            .expect("listed");
        assert_eq!(function, want_fn, "`{site}` moved");
        assert!(
            line.contains(check),
            "`{site}` is not handed to `{check}`: {line}"
        );
    }
    let canonical = compact(&function_span_text(&code, "canonicalParam"));
    assert!(
        canonical.contains("if(values.length!==1||values[0]===''){returnnull;}"),
        "canonicalParam takes a duplicate or empty value: {canonical}"
    );
    let parse = compact(&function_span_text(&code, "parseViewHash"));
    for key in ["scope", "model"] {
        assert!(
            parse.contains(&format!("{key}:canonicalparam(query,params,'{key}')")),
            "parseViewHash reads `{key}` outside canonicalParam"
        );
    }
    let sync = compact(&function_span_text(&code, "syncFromLocation"));
    assert!(
        sync.contains(
            "heldaddress={scope:parsed.scope,model:parsed.model};parsed.scope=null;\
             parsed.model=null;varnext=admitheld(parsed);"
        ),
        "syncFromLocation applies an address scope or model before admitHeld: {sync}"
    );
}

/// The read scan reports a site added anywhere in a script, with its
/// function and line, and skips one in a comment.
#[test]
fn the_address_value_scan_reports_each_read_with_its_function_and_line() {
    let src = "function a() { return firstWindow([params.get('range')]); }\n\
               function b() {\n  var x = params.get('scope');\n}\n\
               // params.get('commented')\n";

    let reads = address_value_reads(&strip_js_comments(src));

    assert_eq!(
        reads,
        vec![
            (
                "params.get('range')".to_owned(),
                "a".to_owned(),
                "function a() { return firstWindow([params.get('range')]); }".to_owned()
            ),
            (
                "params.get('scope')".to_owned(),
                "b".to_owned(),
                "  var x = params.get('scope');".to_owned()
            ),
        ]
    );
}

/// parseViewHash, comments stripped and compacted. Any edit to it, a new
/// read of the address under any name or through a second parse included,
/// changes this string and has to be reviewed against the closed sets
/// before the pin moves.
const PARSE_VIEW_HASH_BODY: &str = "functionparseviewhash(hash){\
    varraw=(hash.charat(0)==='#')?hash.slice(1):hash;\
    varmark=raw.indexof(query_mark);\
    varpath=(mark>=0?raw.slice(0,mark):raw).split(hash_sep);\
    varquery=mark>=0?raw.slice(mark+1):'';\
    varparams=newurlsearchparams(query);\
    return{view:canonicalviewid(path[0]),\
    range:firstwindow([params.get('range'),path[1]]),\
    scope:canonicalparam(query,params,'scope'),\
    model:canonicalparam(query,params,'model'),\
    group:groupparam(params.get('group')),\
    sort:sortparam(params.get('sort'))};}";

/// Byte offsets of `word` in `code` as a whole identifier.
fn word_offsets(code: &str, word: &str) -> Vec<usize> {
    code.match_indices(word)
        .map(|(at, _)| at)
        .filter(|&at| {
            let before = code[..at].chars().next_back();
            let after = code[at + word.len()..].chars().next();
            !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char)
        })
        .collect()
}

/// The lines of `body` naming `params` or `query`, each compacted with its
/// count of each word.
fn address_token_lines(body: &str) -> Vec<(String, usize, usize)> {
    body.lines()
        .filter_map(|line| {
            let params = word_offsets(line, "params").len();
            let query = word_offsets(line, "query").len();
            (params + query > 0).then(|| (compact(line), params, query))
        })
        .collect()
}

/// The names of the functions whose parameter list names `params`, in source
/// order: an empty name for an anonymous `function`, `=>` for an arrow.
fn functions_taking_params(code: &str) -> Vec<String> {
    let declared = code.match_indices("function").filter_map(|(at, _)| {
        let rest = &code[at + "function".len()..];
        let open = rest.find('(')?;
        let close = open + rest[open..].find(')')?;
        let name = rest[..open].trim();
        let named = name.chars().all(is_ident_char);
        let takes = !word_offsets(&rest[open + 1..close], "params").is_empty();
        (named && takes).then(|| (at, name.to_owned()))
    });
    let arrows = code.match_indices("=>").filter_map(|(at, _)| {
        let takes = !word_offsets(arrow_parameters(&code[..at]), "params").is_empty();
        takes.then(|| (at, "=>".to_owned()))
    });
    let mut found: Vec<(usize, String)> = declared.chain(arrows).collect();
    found.sort();
    found.into_iter().map(|(_, name)| name).collect()
}

/// The parameter text of the arrow whose `=>` follows `before`: the
/// parenthesized list, or the single bare parameter.
fn arrow_parameters(before: &str) -> &str {
    let head = before.trim_end();
    if !head.ends_with(')') {
        let start = head
            .char_indices()
            .rev()
            .find(|(_, c)| !is_ident_char(*c))
            .map_or(0, |(at, c)| at + c.len_utf8());
        return &head[start..];
    }
    let mut depth = 0i32;
    for (at, c) in head.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return &head[at + 1..head.len() - 1];
                }
            }
            _ => {}
        }
    }
    head
}

/// parseViewHash is pinned whole, so any read of the parsed address it
/// gains, whatever its spelling, fails here. canonicalParam is the only
/// function that takes the parsed address, and it reads it only with
/// `getAll`.
///
/// Mutation checks: add `alias: new URLSearchParams(raw.slice(mark + 1))
/// .get('alias'),` inside parseViewHash -> red; add `params.has('x')` inside
/// parseViewHash -> red; pass `params` to an arrow function -> red.
#[test]
fn parse_view_hash_reads_the_parsed_address_only_at_its_listed_sites() {
    let code = strip_js_comments(&script());

    let parse = compact(&function_span_text(&code, "parseViewHash"));
    let canonical = address_token_lines(&function_span_text(&code, "canonicalParam"));

    assert_eq!(
        parse, PARSE_VIEW_HASH_BODY,
        "parseViewHash changed: review its reads against the closed sets"
    );
    let params_reads: Vec<&str> = canonical
        .iter()
        .filter(|(_, params, _)| *params > 0)
        .map(|(line, _, _)| line.as_str())
        .collect();
    assert_eq!(
        params_reads,
        [
            "functioncanonicalparam(query,params,key){",
            "varvalues=params.getall(key);",
        ],
        "canonicalParam reads the parsed address another way"
    );
    assert_eq!(functions_taking_params(&code), ["canonicalParam"]);
}

/// The token scan counts a read in any spelling, skips a longer identifier
/// that contains the word, and finds an anonymous or arrow function taking
/// `params`.
#[test]
fn the_address_token_scan_counts_every_spelling() {
    let body = "var params = x;\n  if (params.has('a')) {}\n  var canonicalParams = 1;\n\
                for (var k of params) {}\n  var queryText = query;\n";
    let code = "function a(params) {}\nvar f = function (params) {};\n\
                function b(paramsList) {}\nvar g = params => 1;\n\
                var h = (key, params) => 2;\nvar k = (paramsList) => 3;\n\
                var m = (x = (1), params) => 4;\n";

    assert_eq!(
        address_token_lines(body),
        vec![
            ("varparams=x;".to_owned(), 1, 0),
            ("if(params.has('a')){}".to_owned(), 1, 0),
            ("for(varkofparams){}".to_owned(), 1, 0),
            ("varquerytext=query;".to_owned(), 0, 1),
        ]
    );
    assert_eq!(functions_taking_params(code), ["a", "", "=>", "=>", "=>"]);
}

/// Every `URLSearchParams` in the page: the function it sits in and its
/// line, compacted. The parse in parseViewHash, the one-pair re-serialization
/// in canonicalParam, and the writer in formatViewHash.
const URL_SEARCH_PARAMS_SITES: &[(&str, &str)] = &[
    (
        "canonicalParam",
        "varspelled=newurlsearchparams([[key,values[0]]]).tostring();",
    ),
    ("parseViewHash", "varparams=newurlsearchparams(query);"),
    ("formatViewHash", "varparams=newurlsearchparams();"),
];

/// Every `location.hash` in the page, the same way: the one parse in
/// syncFromLocation and the equality check in writeViewHash.
const LOCATION_HASH_SITES: &[(&str, &str)] = &[
    ("writeViewHash", "if(location.hash===hash){return;}"),
    (
        "syncFromLocation",
        "varparsed=parseviewhash(location.hash);",
    ),
];

/// Each occurrence of `needle` in `code`, in source order, as the innermost
/// function holding it and its line compacted.
fn needle_sites(code: &str, needle: &str) -> Vec<(String, String)> {
    code.match_indices(needle)
        .map(|(at, _)| {
            let line_start = code[..at].rfind('\n').map_or(0, |i| i + 1);
            let line_end = code[at..].find('\n').map_or(code.len(), |i| at + i);
            (
                enclosing_function(code, at),
                compact(&code[line_start..line_end]),
            )
        })
        .collect()
}

fn owned_sites(sites: &[(&str, &str)]) -> Vec<(String, String)> {
    sites
        .iter()
        .map(|(function, line)| ((*function).to_owned(), (*line).to_owned()))
        .collect()
}

/// The address is parsed in one place and read from `location.hash` in two:
/// a second `URLSearchParams` over any text, or another read of
/// `location.hash`, in the codec or anywhere else, fails here. Positive
/// control: the scans find exactly the codec's own sites, so a clean result
/// is not an empty search.
///
/// Mutation checks: add `var p = new URLSearchParams(location.hash.slice(1));`
/// inside syncFromLocation -> red; add a second `new URLSearchParams(raw)`
/// inside parseViewHash -> red.
#[test]
fn the_address_is_parsed_once_and_read_from_location_only_at_its_listed_sites() {
    let code = strip_js_comments(&script());

    let parses = needle_sites(&code, "URLSearchParams");
    let hash_reads = needle_sites(&code, "location.hash");

    assert_eq!(
        parses,
        owned_sites(URL_SEARCH_PARAMS_SITES),
        "URLSearchParams sites changed"
    );
    assert_eq!(
        hash_reads,
        owned_sites(LOCATION_HASH_SITES),
        "location.hash reads changed"
    );
}

/// The site scan reports each occurrence in its function, including one
/// nested in an expression on a line that already holds another site.
#[test]
fn the_needle_scan_reports_each_occurrence_with_its_function_and_line() {
    let src = "function a() {\n  var p = new URLSearchParams(location.hash);\n}\n\
               function b() { return location.hash === h; }\n";

    assert_eq!(
        needle_sites(src, "location.hash"),
        vec![
            (
                "a".to_owned(),
                "varp=newurlsearchparams(location.hash);".to_owned()
            ),
            (
                "b".to_owned(),
                "functionb(){returnlocation.hash===h;}".to_owned()
            ),
        ]
    );
    assert_eq!(needle_sites(src, "URLSearchParams").len(), 1);
}

fn function_span_text(code: &str, name: &str) -> String {
    code[function_span(code, name)].to_owned()
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
    "canonicalParam",
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
