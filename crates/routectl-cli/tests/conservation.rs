//! Wire-conservation driver: captured ingress vs captured outgoing.
//!
//! Walks BOTH fixture roots -- the live-box captures under
//! `tests/fixtures/captured/` and the driver-generated corpus under
//! `tests/fixtures/driver/` -- and adjudicates each fixture's
//! `ingress_request.json` against its `outgoing_request.json` through the
//! lane class and the exception table. No routectl code re-runs here: both
//! files came from the same real request, so the comparison is pure data.
//!
//! The two roots differ only in GATING eligibility, which the harness
//! reads off the committed gated-lane list rather than off mere presence
//! under a root. Live-box bodies are real prompt traffic and are
//! report-only whatever that list says; a lane may be gated only on the
//! driver corpus.
//!
//! Absence of either root is not a failure. The live-box root is
//! per-contributor and gitignored, so a fresh checkout has neither and
//! reports DEGRADED with a loud note rather than red. An empty DRIVER
//! root additionally emits [`NO_DRIVER_CORPUS`], the one greppable line
//! that says the gating half of this leg walked nothing.
//!
//! Output is BOUNDED: fixture names, divergence paths, divergence kinds,
//! and counts. No body value is printed, because a captured body is the
//! operator's real traffic.

mod common;

use common::replay::{
    ConservationRun, CorpusSlice, Fixture, Verdict, adjudicate, discover_driver_fixtures,
    discover_fixtures, driver_root, local_root, make_conserved, plant_driver_case,
    plant_unloadable_driver_case, read_translation_baseline, resolve_gated_lanes,
};

/// The one line a checkout with no driver fixtures must print.
///
/// CI has no driver corpus (the root is gitignored), so the gating half
/// of this leg walks nothing there. A leg that proves nothing must SAY
/// it proves nothing: a bare green test result over an empty corpus is
/// indistinguishable from a real gate, and the whole point of this
/// harness is that the distinction stays visible.
///
/// Keyed on the ENTRIES WALKED rather than on the fixtures LOADED, and
/// the difference is the whole point: a corpus of present-but-unloadable
/// fixtures loads zero and is the opposite of absent. Reporting it as
/// "no driver corpus in this checkout" would hand the loudest state on
/// disk the quietest line, and CI greps for exactly this line.
const NO_DRIVER_CORPUS: &str = "conservation: NOT RUN (no driver corpus in this checkout)";

/// One root's loaded state.
struct LoadedRoot {
    label: &'static str,
    fixtures: Vec<Fixture>,
    unloadable: usize,
    gateable: bool,
    /// Set when the root itself is absent, so the run can degrade loudly
    /// instead of silently walking nothing.
    absent: Option<String>,
}

/// The two roots' walks differ in DEPTH: the live-box corpus is flat and
/// request-id-keyed, the driver corpus is `<lane>/<case_id>`.
#[derive(Clone, Copy)]
enum Walk {
    LiveBox,
    Driver,
}

fn load_root(
    label: &'static str,
    root: &std::path::Path,
    walk: Walk,
    gateable: bool,
) -> LoadedRoot {
    if !root.exists() {
        return LoadedRoot {
            label,
            fixtures: Vec::new(),
            unloadable: 0,
            gateable,
            absent: Some(format!("{label} fixture root is not present")),
        };
    }
    let walked = match walk {
        Walk::LiveBox => discover_fixtures(root),
        Walk::Driver => discover_driver_fixtures(root),
    };
    match walked {
        Ok(corpus) => LoadedRoot {
            label,
            fixtures: corpus.fixtures,
            unloadable: corpus.skipped,
            gateable,
            absent: None,
        },
        // A filesystem-level failure reading a root that EXISTS is not a
        // thin corpus, it is an unknown corpus -- and an unknown corpus
        // cannot be reported as clean.
        Err(e) => panic!(
            "failed to walk the {label} fixture root {}: {e}",
            root.display()
        ),
    }
}

/// The driver root, ALWAYS walked at driver depth and ALWAYS gateable.
///
/// The pairing of root to walk depth is fixed HERE rather than chosen at
/// each call site: `Walk` makes depth a parameter, so a mis-paired call
/// would reinstate exactly the bug this module fixes -- and with no driver
/// corpus in CI the symptom is invisible, so nothing would catch it. Both
/// the production path and the planted-corpus test go through this
/// function, which is what makes the mis-pairing unwritable rather than
/// merely untested.
fn driver_root_loaded(root: &std::path::Path) -> LoadedRoot {
    load_root("driver", root, Walk::Driver, true)
}

/// The live-box root, ALWAYS walked single-level and NEVER gateable: its
/// bodies are real prompt traffic, report-only whatever the gated-lane
/// list says.
fn live_box_root_loaded(root: &std::path::Path) -> LoadedRoot {
    load_root("live-box", root, Walk::LiveBox, false)
}

fn run_conservation() -> ConservationRun {
    let gated = match resolve_gated_lanes() {
        Ok(lanes) => lanes,
        // Fail closed: an unreadable list leaves the gated set UNKNOWN,
        // and an unknown gated set cannot adjudicate coverage. Only the
        // deliberately-empty state resolves to "no lane gated", and the
        // harness's reader is what draws that distinction.
        Err(e) => panic!("cannot resolve the gated-lane list: {e}"),
    };
    let baseline = match read_translation_baseline() {
        Ok(entries) => entries,
        Err(e) => panic!("cannot read the translation-lane baseline: {e}"),
    };

    let roots = [
        live_box_root_loaded(&local_root()),
        driver_root_loaded(&driver_root()),
    ];
    adjudicate_roots(&roots, &gated, &baseline)
}

/// Adjudicate already-walked roots. Split out from [`run_conservation`]
/// so a test can drive the whole path -- walk, note, adjudication --
/// over a PLANTED corpus. Asserting `driver_corpus_note` alone proves
/// only that a function returns what it was handed.
fn adjudicate_roots(
    roots: &[LoadedRoot],
    gated: &common::replay::GatedLanes,
    baseline: &[common::replay::BaselineEntry],
) -> ConservationRun {
    for root in roots {
        if let Some(note) = &root.absent {
            eprintln!("conservation: {note}");
        }
    }
    if let Some(note) = driver_corpus_note(driver_entries_walked(roots)) {
        eprintln!("{note}");
    }
    let slices: Vec<CorpusSlice<'_>> = roots
        .iter()
        .map(|root| CorpusSlice {
            label: root.label,
            fixtures: &root.fixtures,
            unloadable: root.unloadable,
            gateable: root.gateable,
        })
        .collect();

    let run = adjudicate(&slices, gated, baseline);
    for line in run.report_lines() {
        eprintln!("{line}");
    }
    run
}

/// Entries WALKED under the gateable (driver) roots: loaded plus
/// present-but-unloadable. Not `fixtures.len()` -- see
/// [`NO_DRIVER_CORPUS`].
fn driver_entries_walked(roots: &[LoadedRoot]) -> usize {
    roots
        .iter()
        .filter(|root| root.gateable)
        .map(|root| root.fixtures.len() + root.unloadable)
        .sum()
}

/// [`NO_DRIVER_CORPUS`] when the driver roots held no entry at all,
/// `None` otherwise. A separate function so the emptiness rule is
/// asserted directly rather than through stderr scraping.
const fn driver_corpus_note(driver_entries: usize) -> Option<&'static str> {
    if driver_entries == 0 {
        Some(NO_DRIVER_CORPUS)
    } else {
        None
    }
}

/// Walk a PLANTED driver root through the same `load_root` the real run
/// uses, and adjudicate it. The live-box slice is deliberately absent so
/// the assertions below are about the driver half alone.
fn planted_driver_run(root: &std::path::Path) -> (ConservationRun, Option<&'static str>) {
    // The SAME helper the production path uses, so this test covers the
    // real pairing rather than re-asserting a pairing it chose itself.
    let roots = [driver_root_loaded(root)];
    let note = driver_corpus_note(driver_entries_walked(&roots));
    let gated = resolve_gated_lanes().expect("the committed gated-lane list must resolve");
    let baseline = read_translation_baseline().expect("the committed baseline must read");
    (adjudicate_roots(&roots, &gated, &baseline), note)
}

/// The named-skip line tracks what the walk actually FOUND, over a real
/// corpus rather than over `driver_corpus_note` in isolation.
///
/// Three states, and the middle one is the reason this test replaced a
/// predecessor that only called the note function with 0 and 1: an
/// EMPTY corpus is not run, a corpus of one PRESENT-BUT-UNLOADABLE
/// fixture is a broken corpus that must not claim absence, and a
/// populated corpus withholds the line and asserts.
#[test]
fn the_not_run_line_tracks_what_the_driver_walk_found() {
    // Arrange / Act / Assert: empty root.
    let empty = tempfile::tempdir().unwrap();
    let (empty_run, empty_note) = planted_driver_run(empty.path());
    assert_eq!(empty_note, Some(NO_DRIVER_CORPUS));
    assert_eq!(empty_run.asserted(), 0);

    // Present but unloadable: the line is WITHHELD -- a broken corpus is
    // the opposite of an absent one -- and nothing is asserted either,
    // so the withheld line is not standing in for a pass.
    let broken = tempfile::tempdir().unwrap();
    plant_unloadable_driver_case(broken.path(), "anthropic-api", "plain-turn-01");
    let (broken_run, broken_note) = planted_driver_run(broken.path());
    assert_eq!(
        broken_note, None,
        "a present-but-unloadable driver fixture reported as `no driver corpus`",
    );
    assert_eq!(broken_run.unloadable, 1);
    assert_eq!(broken_run.asserted(), 0);

    // Populated: the line is withheld AND the run adjudicates something.
    // This is the positive control D2.A requires -- `asserted` off zero
    // over a real walk, which no assertion on `driver_corpus_note` alone
    // can produce.
    let populated = tempfile::tempdir().unwrap();
    let case = plant_driver_case(populated.path(), "anthropic-api", "plain-turn-01");
    make_conserved(&case);
    let (populated_run, populated_note) = planted_driver_run(populated.path());
    assert_eq!(populated_note, None);
    assert_eq!(
        populated_run.asserted(),
        1,
        "a valid fixture two levels deep did not reach adjudication",
    );
    // `make_conserved` above is what makes this fixture actually conserved,
    // and THIS is what makes that call load-bearing: `asserted` counts every
    // fixture that reached the comparator, divergent or not, so reachability
    // alone cannot tell a conserved fixture from a diverging one. Paired with
    // the divergent control below.
    assert_eq!(
        populated_run
            .lanes
            .iter()
            .map(|lane| lane.unexplained)
            .sum::<usize>(),
        0,
        "a conserved fixture must leave no unexplained divergence",
    );

    // DIVERGENT CONTROL for the assertion above: the same plant WITHOUT
    // make_conserved diverges, so the zero-unexplained claim is a real
    // measurement rather than a property of the harness.
    let divergent = tempfile::tempdir().unwrap();
    plant_driver_case(divergent.path(), "anthropic-api", "plain-turn-01");
    let (divergent_run, _) = planted_driver_run(divergent.path());
    assert!(
        divergent_run
            .lanes
            .iter()
            .map(|lane| lane.unexplained)
            .sum::<usize>()
            > 0,
        "the unconserved plant must diverge, or the conserved assertion above \
         is satisfied by a harness that never compares bodies",
    );
}

/// A single conserved driver case -- the shape the first committed
/// fixture has -- reaches adjudication and does NOT indict the exception
/// table.
///
/// Over a real walk, not over the scoping helper in isolation: the walk is
/// what puts `asserted` above zero, and it is `asserted > 0` that arms the
/// unexercised-exception rule at all. Before the two-level walk landed,
/// this fixture never loaded, so the same assertion would have passed
/// without the rule ever being reached -- a green that proved nothing.
///
/// The zero-hit precondition is asserted rather than assumed: a plain
/// base-url turn sends no system turn, leaves thinking off, and names a
/// model whose alias needs no suffix, so every anthropic entry is
/// unreachable from it. If some later change made one of them fire here,
/// this test would still pass while proving nothing about the scoping, so
/// the zeros are checked first.
#[test]
fn one_conserved_driver_case_adjudicates_without_indicting_the_exception_table() {
    let populated = tempfile::tempdir().unwrap();
    let case = plant_driver_case(populated.path(), "anthropic-api", "plain-turn-01");
    make_conserved(&case);

    let (run, _) = planted_driver_run(populated.path());

    assert_eq!(
        run.asserted(),
        1,
        "the planted case must reach adjudication"
    );
    assert!(
        run.exception_hits.iter().all(|hit| hit.hits == 0),
        "this case is meant to exercise NO exception; the scoping assertion below \
         is vacuous otherwise: {:?}",
        run.exception_hits,
    );
    assert!(
        !run.failures
            .iter()
            .any(|failure| failure.contains("zero divergences")),
        "a one-case gateable slice indicted the exception table: {:?}",
        run.failures,
    );
    assert_ne!(
        run.verdict(),
        Verdict::Fail,
        "{} conservation failure(s):\n  - {}",
        run.failures.len(),
        run.failures.join("\n  - "),
    );
}

/// Fixtures adjudicated across every lane of a run.
#[test]
fn conservation_over_both_fixture_roots() {
    let run = run_conservation();

    assert_ne!(
        run.verdict(),
        Verdict::Fail,
        "{} conservation failure(s):\n  - {}",
        run.failures.len(),
        run.failures.join("\n  - "),
    );
}

/// Every fixture in the COMMITTED driver corpus pins a non-empty
/// `ingress_kind` and a non-empty `wire_pattern`.
///
/// `ingress_kind` matters because this harness SKIPS a fixture whose
/// value is empty, and on a gated lane a skip turns the gate red for zero
/// coverage -- a symptom that reads as unrelated to the lost token.
/// `wire_pattern` matters because it is the claim the capture rig's
/// promotion gate verifies against the captured bytes: an empty one is a
/// fixture that reached the corpus with nothing to verify.
///
/// Stated over the whole corpus rather than against one named case: a
/// per-fixture assertion is a special case that the general rule covers,
/// and it would go on passing while a second fixture landed unpinned.
#[test]
fn every_committed_driver_fixture_pins_its_ingress_kind_and_wire_pattern() {
    let root = driver_root();
    let corpus = match discover_driver_fixtures(&root) {
        Ok(corpus) => corpus,
        Err(e) => panic!("the driver root {} must walk: {e}", root.display()),
    };
    if corpus.fixtures.is_empty() {
        eprintln!(
            "conservation: driver root `{}` holds no loadable fixture; the pinned-metadata \
             assertion is SKIPPED.",
            root.display(),
        );
        return;
    }

    let unpinned: Vec<String> = corpus
        .fixtures
        .iter()
        .filter(|fixture| fixture.meta.ingress_kind.is_empty())
        .map(|fixture| fixture.name.clone())
        .collect();
    assert!(
        unpinned.is_empty(),
        "committed driver fixture(s) pin no ingress_kind, so this harness skips them and a \
         gated lane goes red for zero coverage: {unpinned:?}",
    );

    let unclaimed: Vec<String> = corpus
        .fixtures
        .iter()
        .filter(|fixture| fixture.meta.wire_pattern.is_empty())
        .map(|fixture| fixture.name.clone())
        .collect();
    assert!(
        unclaimed.is_empty(),
        "committed driver fixture(s) record no wire_pattern, so nothing verified what shape \
         they carry: {unclaimed:?}",
    );
}

/// The COMMITTED corpus still loads after `client.binary_version` was
/// added to the schema.
///
/// The field is additive by construction (a serde default on
/// [`common::replay::FixtureClient`]), and this is the assertion that the
/// construction holds against the fixtures ACTUALLY ON DISK rather than
/// against a planted variant: every committed fixture predates the key, so
/// a required field -- or a default that failed to apply -- would show up
/// here as a corpus that stopped walking. A required field is a major
/// fixture-format bump, which under the committed-corpus regime orphans
/// every contributed cell whose session cannot be re-driven.
///
/// It asserts the LOAD and the field's absence-value, never a populated
/// value: the corpus was captured before the driver-side read reached the
/// rig, so demanding a value here would demand a recapture the regime is
/// built to avoid.
///
/// The absent-corpus skip keys on ENTRIES WALKED, not on fixtures LOADED.
/// Keying on the loaded count would make this test SKIP in exactly the
/// state it exists to catch: a required field stops every fixture loading,
/// which leaves the loaded set empty and reads as "no corpus here".
#[test]
fn the_committed_corpus_loads_with_the_binary_client_version_key_absent() {
    let root = driver_root();
    let corpus = match discover_driver_fixtures(&root) {
        Ok(corpus) => corpus,
        Err(e) => panic!("the driver root {} must walk: {e}", root.display()),
    };
    let walked = corpus.fixtures.len() + corpus.skipped;
    if walked == 0 {
        eprintln!(
            "conservation: driver root `{}` holds no fixture entry at all; the \
             additive-field tolerance assertion is SKIPPED.",
            root.display(),
        );
        return;
    }

    assert_eq!(
        corpus.skipped,
        0,
        "adding client.binary_version must not skip a committed fixture: an additive key \
         carries a default, and a fixture that stopped loading means the key became \
         required -- a major fixture-format bump that orphans contributed cells. \
         {walked} entr(ies) walked, {} loaded",
        corpus.fixtures.len(),
    );

    // The wire-side value is what these fixtures DO carry, so asserting it
    // alongside is the positive control: without it, a loader that returned
    // an all-empty `client` would satisfy the absence check above.
    for fixture in &corpus.fixtures {
        assert!(
            !fixture.meta.client.version.is_empty(),
            "committed fixture `{}` carries no wire-side client.version, so the \
             absence assertion below proves nothing about the new key",
            fixture.name,
        );
        assert!(
            fixture.meta.client.binary_version.is_empty(),
            "committed fixture `{}` records a binary-side client version; it was \
             captured before the driver-side read reached the rig, so this is a \
             recapture (update this assertion) or a hand edit",
            fixture.name,
        );
    }
}

/// The measured baseline of the live-box corpus, asserted only WHEN THAT
/// CORPUS IS PRESENT.
///
/// The four classes and their counts were reproduced independently before
/// this harness existed; pinning them here makes a change in the corpus or
/// in a transform visible as a specific delta rather than as a vague
/// "conservation moved". Absence of the corpus skips loudly -- the root is
/// per-contributor and gitignored, so CI has none and its absence must
/// never fail.
///
/// The numbers are deliberately EXACT. A soft assertion (`>= 1`) would
/// keep passing while a transform silently stopped firing, which is
/// exactly the drift this harness exists to catch. When a recapture moves
/// them, the diff on these constants is the review moment that change
/// deserves.
///
/// # The constants below are STALE and UNREPRODUCIBLE (2026-08-28)
///
/// They were measured against a 250-fixture corpus that no longer exists: a
/// path-quoting error in a cleanup command removed it (command substitution
/// strips a trailing newline, so the argument resolved to the live tree rather
/// than the intended sibling). That corpus was live-box traffic: gitignored by
/// design, never committed, and not reproducible -- the container rig builds
/// the DRIVER corpus, which is synthetic and a different corpus entirely.
///
/// So `250 / 250 / 238 / 133 / 6 / 4` are a HISTORICAL RECORD, not a
/// currently-verifiable measurement. Live capture has been re-enabled and the
/// corpus is rebuilding from current traffic; the counts it produces will
/// legitimately differ (different traffic mix, a newer client, and shipped
/// transforms that did not exist when the originals were taken).
///
/// WHEN THE REBUILT CORPUS IS LARGE ENOUGH TO BE A BASELINE: re-measure, then
/// replace these constants in ONE commit whose body records the new corpus
/// size and the date -- and do NOT soften them to ranges to make them fit.
/// The exactness is the mechanism. Until then this test SKIPS on an absent or
/// too-small corpus, which is honest but proves nothing: read the skip line,
/// never a green result, as the signal.
/// Fixture count the pinned baseline counts below were measured against.
/// The gate for asserting them at all: a rebuilding corpus of any other size
/// skips rather than reporting legitimate traffic as a regression.
const BASELINE_CORPUS_FIXTURES: usize = 250;

#[test]
fn the_live_box_corpus_reduces_to_four_explained_classes_with_zero_unexplained() {
    let root = local_root();
    if !root.exists() {
        eprintln!(
            "conservation: live-box root `{}` absent; the measured-baseline assertion is \
             SKIPPED (this corpus is per-contributor and gitignored).",
            root.display(),
        );
        return;
    }
    let corpus = discover_fixtures(&root).expect("the live-box root exists, so it must walk");
    if corpus.fixtures.is_empty() {
        eprintln!("conservation: live-box root holds 0 fixtures; baseline assertion SKIPPED.");
        return;
    }
    // The pinned counts below describe the DESTROYED 250-fixture corpus (see
    // the staleness note above). A corpus that is rebuilding holds a different,
    // smaller, entirely legitimate set -- asserting the old exact numbers
    // against it would report a REAL corpus as a regression and invite someone
    // to soften the assertions into ranges, which is what would actually break
    // the harness. Skip loudly until the rebuild reaches the pinned size, and
    // re-measure then.
    if corpus.fixtures.len() != BASELINE_CORPUS_FIXTURES {
        eprintln!(
            "conservation: live-box root holds {} fixtures, not the {} the pinned \
             baseline was measured against; baseline assertion SKIPPED. The pinned \
             counts are a historical record of a corpus that no longer exists -- \
             re-measure and replace them once the rebuild is large enough, and do \
             not soften them to ranges.",
            corpus.fixtures.len(),
            BASELINE_CORPUS_FIXTURES,
        );
        return;
    }

    let gated = resolve_gated_lanes().expect("the committed gated-lane list must resolve");
    let baseline = read_translation_baseline().expect("the committed baseline must read");
    let run = adjudicate(
        &[CorpusSlice {
            label: "live-box",
            fixtures: &corpus.fixtures,
            unloadable: corpus.skipped,
            gateable: false,
        }],
        &gated,
        &baseline,
    );
    for line in run.report_lines() {
        eprintln!("{line}");
    }

    let unexplained: usize = run.lanes.iter().map(|l| l.unexplained).sum();
    assert_eq!(
        unexplained,
        0,
        "every divergence on this corpus is an explained transform; \
         widening an exception to absorb a new one is never the fix:\n  - {}",
        run.failures.join("\n  - "),
    );

    let anthropic = run
        .lanes
        .iter()
        .find(|l| l.ingress == "anthropic" && l.egress == "anthropic-api")
        .expect("every corpus fixture rides the anthropic fidelity lane");
    assert_eq!(anthropic.fixtures, BASELINE_CORPUS_FIXTURES);
    assert_eq!(anthropic.asserted, BASELINE_CORPUS_FIXTURES);
    assert_eq!(anthropic.skipped, 0);
    assert_eq!(
        anthropic.normalized, 238,
        "the system-turn lift touched 238 fixtures",
    );

    for (id, expected) in [
        ("system-turn-lift", 238),
        ("thinking-temperature-clamp", 133),
        ("model-alias-suffix-resolved", 6),
        ("disabled-thinking-dropped", 4),
    ] {
        let hits = run
            .exception_hits
            .iter()
            .find(|h| h.id == id)
            .unwrap_or_else(|| panic!("no exception `{id}`"))
            .hits;
        assert_eq!(hits, expected, "`{id}` hit count moved");
    }

    assert_eq!(run.verdict(), Verdict::Pass, "{:?}", run.failures);
}
