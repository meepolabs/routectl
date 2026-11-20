// The shared sortable table in headless Chrome, with Config "Configured
// models" as its consumer, plus a source check that the sort registry names
// exactly the columns the tables declare sortable. `include!`d into a child
// module of `page::tests` beside `browser`; every browser test here is
// `#[ignore]` and runs with `cargo test -p routectl-cli page_browser -- --ignored`.

use serde_json::Value;

const SORT_DRIVER: &str = include_str!("page_browser_sort_driver.js");

const SORT_BUDGET_MS: u64 = 30_000;

fn sort_scenario(name: &'static str, hash: &'static str) -> browser::Scenario {
    browser::Scenario {
        name,
        hash,
        snippets: &[SORT_DRIVER],
        budget_ms: SORT_BUDGET_MS,
    }
}

/// The (nickname, provider, upstream) of every model the populated config
/// panel serves, in the order it serves them.
fn fixture_models() -> Vec<(String, String, Option<String>)> {
    let aggregate = crate::handlers::status::panel_fixtures::aggregate_json();
    let text = |m: &Value, key: &str| m[key].as_str().map(str::to_owned);
    aggregate["panels"]["config"]["data"]["models"]
        .as_array()
        .expect("the config panel carries models")
        .iter()
        .map(|m| {
            (
                text(m, "nickname").expect("a nickname"),
                text(m, "provider").expect("a provider"),
                text(m, "upstream"),
            )
        })
        .collect()
}

/// The orders below are written out by hand; this pins the fixture they were
/// read from, so a fixture change fails here rather than as a puzzling order.
fn assert_fixture_models_are_the_known_five() {
    let some = |s: &str| Some(s.to_owned());
    let expected = vec![
        (
            "bedclaude".into(),
            "bed".into(),
            some("anthropic.claude-test-v1:0"),
        ),
        ("codex-mini".into(), "codex".into(), some("gpt-5-codex")),
        ("glm".into(), "compat".into(), some("glm-5")),
        (
            "seat-sonnet".into(),
            "claude-seat".into(),
            some("claude-sonnet-4-5"),
        ),
        (
            "sonnet".into(),
            "anthropic".into(),
            some("claude-sonnet-4-5"),
        ),
    ];
    assert_eq!(fixture_models(), expected);
}

fn names(report: &Value) -> Vec<&str> {
    report["order"]
        .as_array()
        .expect("a row order")
        .iter()
        .map(|n| n.as_str().expect("a nickname"))
        .collect()
}

fn assert_table(label: &str, report: &Value, order: &[&str], sort: Value, hash: &str) {
    assert_eq!(names(report), order, "{label}: {report:#}");
    assert_eq!(report["aria_sort"], sort, "{label}: aria-sort {report:#}");
    assert_eq!(report["hash"], hash, "{label}: address");
    assert_eq!(report["details_follow_rows"], true, "{label}: detail rows");
}

/// A header click sorts the rows ascending, a second descending, each one
/// history entry with `sort=` in the address and `aria-sort` on the header;
/// a reload restores the descending order, and a third click goes back to the
/// order the builder gave.
///
/// Mutation checks: drop the `aria-sort` attribute in headerCell -> red;
/// write nothing to the address (commitViewState without `sort`) -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_header_click_sorts_and_the_order_survives_a_reload() {
    assert_fixture_models_are_the_known_five();
    let out = browser::run_scenario(&sort_scenario("sort-click-reload", "config")).result;

    let builder = ["bedclaude", "codex-mini", "glm", "seat-sonnet", "sonnet"];
    let asc = ["sonnet", "bedclaude", "seat-sonnet", "codex-mini", "glm"];
    let desc = ["glm", "codex-mini", "seat-sonnet", "bedclaude", "sonnet"];
    let desc_hash = "#config?sort=config-models.provider.desc";
    assert_table(
        "unsorted",
        &out["unsorted"],
        &builder,
        serde_json::json!({}),
        "#config",
    );
    assert_table(
        "ascending",
        &out["asc"],
        &asc,
        serde_json::json!({"provider": "ascending"}),
        "#config?sort=config-models.provider.asc",
    );
    assert_eq!(out["asc"]["added"], 1, "one history entry per click");
    assert_table(
        "descending",
        &out["desc"],
        &desc,
        serde_json::json!({"provider": "descending"}),
        desc_hash,
    );
    assert_eq!(out["desc"]["added"], 2, "one history entry per click");
    assert_table(
        "after reload",
        &out["reloaded"],
        &desc,
        serde_json::json!({"provider": "descending"}),
        desc_hash,
    );
    assert_table(
        "third click",
        &out["cleared"],
        &builder,
        serde_json::json!({}),
        "#config",
    );
    assert_eq!(out["cleared"]["added"], 1);
}

/// The focused header button is the same node after the sort and after each
/// refresh, and an open row keeps its open state and its detail node while
/// the sort moves it. Positive controls: the sort really reordered the rows,
/// and a figure changed every round.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_sort_and_a_refresh_keep_the_focused_header_and_the_open_row() {
    assert_fixture_models_are_the_known_five();
    let out = browser::run_scenario(&sort_scenario("sort-keeps-nodes", "config")).result;

    let sorted_order = ["bedclaude", "seat-sonnet", "sonnet", "glm", "codex-mini"];
    assert_ne!(
        out["sorted"]["order"], out["unsorted"],
        "the sort moved no row"
    );
    let rounds = out["rounds"]["rounds"].as_array().expect("two rounds");
    assert_eq!(rounds.len(), 2);
    let obs: Vec<&Value> = std::iter::once(&out["sorted"])
        .chain(std::iter::once(&out["rounds"]["baseline"]))
        .chain(rounds)
        .collect();
    for (i, o) in obs.iter().enumerate() {
        assert_eq!(names(o), sorted_order, "observation {i}: {o:#}");
        assert_eq!(o["aria_sort"], serde_json::json!({"upstream": "ascending"}));
        for flag in [
            "same_header",
            "header_focused",
            "same_detail",
            "expanded",
            "detail_shown",
            "details_follow_rows",
        ] {
            assert_eq!(o[flag], true, "`{flag}` broke at observation {i}: {o:#}");
        }
    }
    let figures: Vec<&Value> = std::iter::once(&out["rounds"]["baseline"])
        .chain(rounds)
        .map(|o| &o["figure"])
        .collect();
    assert_ne!(figures[0], figures[1], "the first round repainted nothing");
    assert_ne!(figures[1], figures[2], "the second round repainted nothing");
}

/// A model without an upstream sorts last in both directions, and the two
/// models sharing an upstream sort by nickname in both directions, though the
/// reading serves them in the opposite order.
///
/// Mutation checks: put absent values first (flip `aAbsent ? 1 : -1`) -> red;
/// drop the row-key tie break -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_absent_values_sort_last_and_ties_sort_by_row_key() {
    assert_fixture_models_are_the_known_five();
    let out = browser::run_scenario(&sort_scenario("sort-null-and-tie", "config")).result;

    let served = ["sonnet", "seat-sonnet", "glm", "codex-mini", "bedclaude"];
    assert_eq!(names(&out["unsorted"]), served, "{out:#}");
    assert_eq!(
        out["null_cell"], "-",
        "the absent upstream is drawn as absent"
    );
    assert_eq!(
        names(&out["asc"]),
        ["seat-sonnet", "sonnet", "glm", "codex-mini", "bedclaude"],
        "{out:#}"
    );
    assert_eq!(
        names(&out["desc"]),
        ["codex-mini", "glm", "seat-sonnet", "sonnet", "bedclaude"],
        "{out:#}"
    );
}

/// Two models sharing a nickname make the models section an error card and
/// leave every other config section drawn. The same page before the duplicate
/// arrives is the control: models drawn, no error card.
///
/// Mutation check: skip checkRowKeys in fillRows -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_duplicate_row_key_errors_only_its_section() {
    let out = browser::run_scenario(&sort_scenario("sort-duplicate-key", "config")).result;

    let others = [
        "config-aliases",
        "config-providers",
        "config-capabilities",
        "config-activation",
        "config-classes",
    ];
    let tables = |o: &Value| -> Vec<String> {
        o["tables"]
            .as_array()
            .expect("table ids")
            .iter()
            .map(|t| t.as_str().expect("an id").to_owned())
            .collect()
    };
    let clean = &out["clean"];
    assert_eq!(clean["error_cards"], 0, "{out:#}");
    assert!(
        tables(clean).contains(&"config-models".to_owned()),
        "{out:#}"
    );

    let dup = &out["duplicate"];
    assert_eq!(dup["error_cards"], 1, "{out:#}");
    assert!(
        !tables(dup).contains(&"config-models".to_owned()),
        "{out:#}"
    );
    for id in others {
        assert!(
            tables(dup).contains(&id.to_owned()),
            "`{id}` vanished: {out:#}"
        );
    }
    let titles = dup["titles"].as_array().expect("titles");
    assert!(!titles.contains(&Value::from("Models")), "{out:#}");
    assert!(titles.contains(&Value::from("Aliases")), "{out:#}");
}

// ---- source check: the sort registry and the sortable columns ---------------

use view_contract::{call_args, call_end, quoted_literal, split_args};

/// Table id -> sortable column ids, as the `mkTable` calls in `src` declare
/// them through `sortBy(<col>, '<id>', ...)` in their column list.
fn declared_sort_columns(src: &str) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out = std::collections::BTreeMap::new();
    for args in call_args(src, "mkTable") {
        let id =
            quoted_literal(args.first().map_or("", String::as_str)).expect("a literal table id");
        let cols: Vec<String> = call_args(args.get(2).map_or("", String::as_str), "sortBy")
            .iter()
            .map(|a| {
                let arg = a.get(1).map_or("", String::as_str);
                quoted_literal(arg)
                    .unwrap_or_else(|| panic!("{id}: sort column `{arg}` is not a literal"))
            })
            .collect();
        if !cols.is_empty() {
            out.insert(id, cols);
        }
    }
    out
}

/// The `var SORT_TABLES = { '<table>': ['<col>', ...], ... }` registry in `src`.
fn sort_registry(src: &str) -> std::collections::BTreeMap<String, Vec<String>> {
    let code = strip_js_comments(src);
    let at = code
        .find("var SORT_TABLES = {")
        .expect("the script declares `var SORT_TABLES = {`");
    let chars: Vec<char> = code[at..].chars().collect();
    let open = chars
        .iter()
        .position(|c| *c == '{')
        .expect("the registry body");
    let end = call_end(&chars, open);
    let mut out = std::collections::BTreeMap::new();
    for entry in split_args(&chars[open + 1..end - 1]) {
        let (key, list) = entry.split_once(':').expect("a `key: [...]` entry");
        let list: Vec<char> = list.trim().chars().collect();
        assert!(
            list.first() == Some(&'[') && list.last() == Some(&']'),
            "{entry}"
        );
        let cols = split_args(&list[1..list.len() - 1])
            .iter()
            .map(|c| quoted_literal(c).expect("a literal column id"))
            .collect();
        out.insert(quoted_literal(key).expect("a literal table id"), cols);
    }
    out
}

/// The address accepts a sort exactly for the columns a table draws as
/// sortable: a registered column no table sorts by, or a sortable column the
/// address would drop, is drift.
///
/// Mutation check: remove `'upstream'` from SORT_TABLES -> red.
#[test]
fn the_sort_registry_names_exactly_the_sortable_columns() {
    let script = script();

    let declared = declared_sort_columns(&script);

    assert!(
        declared.contains_key("config-models"),
        "positive control: the models table is sortable"
    );
    assert_eq!(sort_registry(&script), declared);
}

#[test]
fn the_sort_registry_scan_reports_drift() {
    let src = "var SORT_TABLES = {\n  't-a': ['x', 'y']\n};\n\
               var a = mkTable('t-a', 'A', [sortBy(C('x'), 'x', f), C('z')], {});\n\
               var b = mkTable('t-b', 'B', [C('x')]);";

    let declared = declared_sort_columns(src);
    let registry = sort_registry(src);

    assert_eq!(declared.len(), 1, "an unsorted table declares nothing");
    assert_eq!(declared["t-a"], ["x"]);
    assert_eq!(registry["t-a"], ["x", "y"]);
    assert_ne!(registry, declared);
}
