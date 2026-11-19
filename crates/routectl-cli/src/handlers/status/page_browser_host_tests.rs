// The view host owns each section's panel states, in a real headless Chrome.
// The served page's Models view is swapped for one probe section with no
// guard of its own, so every card, fault and recovery seen here is drawn by
// viewSection alone. `include!`d into a child module of `page::tests` beside
// `browser`; the scenario lives in page_browser_host_driver.js.

use serde_json::Value;

const HOST_DRIVER: &str = include_str!("page_browser_host_driver.js");

/// The Models view as the page composes it.
const MODELS_VIEW: &str = "  function buildModels() {
    return viewSection('health', healthTargetSection, 'Dispatch targets');
  }";

/// The Models view composing one probe section through the host helper. The
/// builder throws, returns a self-guarded throw, or returns a node under its
/// own title, as the driver's `mode` says, and never consults the record's
/// state itself.
const PROBE_VIEW: &str = "  function buildModels() {
    var probe = window.routectlHostProbe;
    return viewSection('health', function (rec) {
      probe.calls.push(rec.state);
      if (probe.mode === 'throw') { throw new Error('probe section fault'); }
      if (probe.mode === 'inner-throw') {
        return safeSection(rec, function () { throw new Error('probe section fault'); });
      }
      var node = document.createElement('div');
      node.appendChild(sectionHead('Probe', null));
      var line = node.appendChild(document.createElement('p'));
      line.id = 'host-probe';
      line.textContent = 'probe build ' + probe.calls.length;
      return node;
    }, 'Probe');
  }";

fn with_probe_view(page: String) -> String {
    assert_eq!(
        page.matches(MODELS_VIEW).count(),
        1,
        "the page composes the Models view as this test expects"
    );
    page.replace(MODELS_VIEW, PROBE_VIEW)
}

fn calls(at: &Value) -> Vec<&str> {
    at["calls"]
        .as_array()
        .expect("the probe calls")
        .iter()
        .map(|c| c.as_str().expect("a record state"))
        .collect()
}

fn text<'a>(value: &'a Value, what: &str) -> &'a str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{what} is text: {value:#}"))
}

/// A section builder with no guard of its own gets every panel state from the
/// host: loading and unavailable records draw the host's card under the
/// section title without calling the builder, a throw costs the section one
/// error card under that title and marks its source faulted, a successful
/// build carries only its own title, a clean build after the throw
/// clears the fault in the same pass, and a builder that guards itself as
/// well still shows ONE card and still faults its source.
///
/// The recovery is a repaint with no new health answer (a view switch away
/// and back), since a landing payload clears a source's fault by itself.
///
/// Mutation checks: viewSection calling `build(rec)` directly instead of
/// safeSection -> red (no host card on the loading record); drop the
/// markRenderAttempt call from safeSection -> red (the recovered build
/// leaves the source faulted); title only a state card, not a builder fault
/// -> red (the throwing step has no head); title every section -> red (the
/// successful build has two heads).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_host_draws_every_state_of_an_unguarded_section() {
    let scenario = browser::Scenario {
        name: "host-section-states",
        hash: "models",
        snippets: &[HOST_DRIVER],
        budget_ms: 60_000,
    };
    let result = browser::run_scenario_on_patched_page(&scenario, with_probe_view).result;
    let at = |step: &str| &result[step];

    let loading = at("loading");
    assert_eq!(loading["skeleton"], true, "{loading:#}");
    assert_eq!(loading["head"], "Probe", "{loading:#}");
    assert!(
        calls(loading).is_empty(),
        "built on a loading record: {loading:#}"
    );

    let success = at("success");
    assert_eq!(success["cards"], 0, "{success:#}");
    assert_eq!(success["skeleton"], false, "{success:#}");
    assert!(text(&success["probe"], "the probe node").starts_with("probe build "));
    assert_eq!(success["status"], "", "{success:#}");
    assert_eq!(
        success["heads"], 1,
        "a successful build is titled once, by itself: {success:#}"
    );

    let throwing = at("throwing");
    assert_eq!(throwing["sections"], 1, "{throwing:#}");
    assert_eq!(throwing["cards"], 1, "{throwing:#}");
    assert_eq!(throwing["probe"], Value::Null, "{throwing:#}");
    assert_eq!(throwing["head"], "Probe", "{throwing:#}");
    assert_eq!(throwing["heads"], 1, "{throwing:#}");
    assert!(
        text(&throwing["card"], "the error card").starts_with("unavailable: invalid_payload"),
        "{throwing:#}"
    );
    assert!(
        text(&throwing["status"], "the status line").starts_with("invalid payload"),
        "the throw did not fault its source: {throwing:#}"
    );

    let recovered = at("recovered");
    assert_eq!(recovered["cards"], 0, "{recovered:#}");
    assert!(text(&recovered["probe"], "the probe node").starts_with("probe build "));
    assert_eq!(
        recovered["status"], "",
        "the clean build left its source faulted: {recovered:#}"
    );

    let unavailable = at("unavailable");
    assert_eq!(unavailable["cards"], 1, "{unavailable:#}");
    assert_eq!(unavailable["head"], "Probe", "{unavailable:#}");
    assert!(
        text(&unavailable["card"], "the unavailable card").starts_with("unavailable: probe_down"),
        "{unavailable:#}"
    );
    assert_eq!(
        calls(unavailable).len(),
        calls(recovered).len(),
        "built on an unavailable record: {unavailable:#}"
    );

    let inner = at("inner_throwing");
    assert_eq!(inner["sections"], 1, "{inner:#}");
    assert_eq!(
        inner["cards"], 1,
        "a double-guarded throw drew two cards: {inner:#}"
    );
    assert!(
        text(&inner["status"], "the status line").starts_with("invalid payload"),
        "the host's attempt mark undid the section's own fault: {inner:#}"
    );

    let last = calls(inner);
    assert!(
        last.len() >= 4,
        "positive control: the probe was built: {inner:#}"
    );
    assert!(
        last.iter().all(|state| *state == "live"),
        "the builder saw a record that is not live: {last:?}"
    );
}
