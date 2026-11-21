// A refresh holds back the section holding a text selection and keeps page
// and table scroll, in a real headless Chrome. `include!`d into a child
// module of `page::tests` beside `browser`; the scenarios live in
// page_browser_locks_driver.js. Every browser test here is `#[ignore]` and
// runs with `cargo test -p routectl-cli page_browser -- --ignored`.

use serde_json::Value;

const LOCKS_DRIVER: &str = include_str!("page_browser_locks_driver.js");

const LOCKS_BUDGET_MS: u64 = 40_000;

fn locks_scenario(name: &'static str, hash: &'static str) -> browser::Scenario {
    browser::Scenario {
        name,
        hash,
        snippets: &[LOCKS_DRIVER],
        budget_ms: LOCKS_BUDGET_MS,
    }
}

fn held_observations(result: &Value) -> Vec<&Value> {
    let rounds = result["rounds"].as_array().expect("rounds observed");
    assert_eq!(rounds.len(), 2, "two held rounds were observed");
    std::iter::once(&result["baseline"]).chain(rounds).collect()
}

/// While the selection stands: the selected text node and its text survive,
/// the findings section shows what it showed when the selection was made,
/// although the server sent something else (the positive control that the
/// section would have changed), and a figure in another section repainted.
/// After the selection collapses, the next round brings the section up to
/// what the server sent.
fn assert_held_then_released(result: &Value) {
    let obs = held_observations(result);
    let shown_at_selection = &obs[0]["details"];
    for (i, o) in obs.iter().enumerate() {
        for flag in ["same_text", "same_anchor", "connected"] {
            assert_eq!(o[flag], true, "`{flag}` broke at observation {i}: {o:#}");
        }
        assert_eq!(
            &o["details"], shown_at_selection,
            "the held section changed at observation {i}: {o:#}"
        );
    }
    for (i, o) in obs.iter().enumerate().skip(1) {
        assert_ne!(
            o["served"], o["details"],
            "positive control: round {i} served findings the held section does not show"
        );
    }
    for pair in obs.windows(2) {
        assert_ne!(
            pair[0]["figure"], pair[1]["figure"],
            "a figure outside the held section stayed the same: {obs:#?}"
        );
    }
    let released = &result["released"];
    assert_eq!(
        released["details"], released["served"],
        "the section caught up on the first round after the selection collapsed: {released:#}"
    );
    assert_ne!(
        &released["details"], shown_at_selection,
        "the released round changed the section"
    );
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_selected_finding_holds_its_section_while_a_sibling_changes() {
    let report = browser::run_scenario(&locks_scenario("lock-retained", "doctor"));

    assert_held_then_released(&report.result);
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_selected_finding_holds_its_place_while_the_server_reorders() {
    let report = browser::run_scenario(&locks_scenario("lock-moved", "doctor"));

    assert_held_then_released(&report.result);
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_selected_finding_stays_until_released_after_the_server_drops_it() {
    let report = browser::run_scenario(&locks_scenario("lock-removed", "doctor"));

    assert_held_then_released(&report.result);
    let released: Vec<&str> = report.result["released"]["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|d| d.as_str().expect("detail text"))
        .collect();
    assert!(
        !released.contains(&"alias alpha names no target"),
        "the dropped finding left after the release: {released:?}"
    );
}

/// A held section is held only against its own rebuild: when the findings
/// section leaves the page, the section the morph pairs with its node is
/// drawn, not the stale findings, and the selected text node leaves with it.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_selected_section_gives_way_when_the_section_leaves() {
    let report = browser::run_scenario(&locks_scenario("lock-section-gone", "doctor"));

    let result = &report.result;
    assert_eq!(result["findings_shown"], false, "{result:#}");
    assert_eq!(result["stale_text_shown"], false, "{result:#}");
    assert_eq!(
        result["selected_connected"], false,
        "the selected text node outlived its section: {result:#}"
    );
}

/// Two findings sharing section and name swap places every round. Each card
/// keeps its id and its node with its own record, so the morph moves the
/// cards rather than rewriting one into the other.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_same_named_findings_keep_their_own_ids_when_they_swap() {
    let report = browser::run_scenario(&locks_scenario("doctor-ids-swap", "doctor"));

    let obs = held_observations(&report.result);
    let orders: Vec<Vec<&str>> = obs
        .iter()
        .map(|cards| {
            cards
                .as_array()
                .expect("cards")
                .iter()
                .map(|c| c["detail"].as_str().expect("detail"))
                .collect()
        })
        .collect();
    for pair in orders.windows(2) {
        assert_ne!(pair[0], pair[1], "positive control: the order swapped");
    }
    let mut id_of = std::collections::BTreeMap::new();
    for cards in &obs {
        for card in cards.as_array().expect("cards") {
            assert_eq!(card["same_node"], true, "a card was rebuilt: {card:#}");
            let id = card["id"].as_str().expect("id");
            let detail = card["detail"].as_str().expect("detail");
            assert_eq!(
                *id_of.entry(id.to_owned()).or_insert(detail),
                detail,
                "id {id} moved to another record"
            );
        }
    }
    assert_eq!(id_of.len(), 2, "two distinct ids: {id_of:?}");
}

/// The table scroller and a row in it are the same nodes across two changed
/// rounds, and both the table and the page keep their scroll. Scroll alone is
/// not evidence of an in-place update: restoreScroll puts it back after a
/// wholesale replace too, so the node identity is what pins the morph.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_table_and_page_scroll_survive_changed_rounds() {
    let report = browser::run_scenario(&locks_scenario("lock-scroll", "config"));

    let obs = held_observations(&report.result);
    let base = obs[0];
    assert!(
        base["table_left"].as_f64().is_some_and(|x| x > 0.0),
        "positive control: the table was scrolled: {base:#}"
    );
    assert!(
        base["page_y"].as_f64().is_some_and(|y| y > 0.0),
        "positive control: the page was scrolled: {base:#}"
    );
    for (i, o) in obs.iter().enumerate() {
        assert_eq!(
            o["same_wrap"], true,
            "the table scroller was rebuilt at {i}: {o:#}"
        );
        assert_eq!(
            o["same_row"], true,
            "the table row was rebuilt at {i}: {o:#}"
        );
        assert_eq!(o["table_left"], base["table_left"], "table scroll at {i}");
        assert_eq!(o["page_y"], base["page_y"], "page scroll at {i}");
    }
    for pair in obs.windows(2) {
        assert_ne!(
            pair[0]["figure"], pair[1]["figure"],
            "a figure stayed the same across a round: {obs:#?}"
        );
    }
}

/// The record discriminator decides a Doctor card's id; an ordinal is added
/// only for byte-identical records, and severity stays out of it.
#[test]
fn doctor_item_ids_come_from_the_whole_record() {
    let item_id = compact(&function_body("doctorItemId"));

    for part in [
        "idkey(f.section)",
        "idkey(f.name)",
        "idkey(f.detail)",
        "idkey(f.remediation)",
    ] {
        assert!(item_id.contains(part), "`{part}` must enter the id");
    }
    assert!(
        !item_id.contains("f.status"),
        "severity must not enter the id"
    );
}

/// Every hand-built card and section wrapper the selection skip relies on is
/// marked with its own key.
#[test]
fn section_wrappers_mark_themselves_as_refresh_units() {
    let card = compact(&function_body("card"));

    assert!(card.contains("markrefreshunit(document.createelement('div'),title)"));
    assert!(
        compact(&function_body("modelsSection"))
            .contains("markrefreshunit(document.createelement('div'),'models-lanes')")
    );
    for name in ["doctorHeadline", "doctorFindings", "doctorPasses"] {
        assert!(
            function_body(name).contains("markRefreshUnit(document.createElement('div'), '"),
            "{name} marks its wrapper"
        );
    }
}
