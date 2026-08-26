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
//! reports DEGRADED with a loud note rather than red.
//!
//! Output is BOUNDED: fixture names, divergence paths, divergence kinds,
//! and counts. No body value is printed, because a captured body is the
//! operator's real traffic.

mod common;

use common::replay::{
    ConservationRun, CorpusSlice, Fixture, Verdict, adjudicate, discover_fixtures, driver_root,
    local_root, read_translation_baseline, resolve_gated_lanes,
};

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

fn load_root(label: &'static str, root: &std::path::Path, gateable: bool) -> LoadedRoot {
    if !root.exists() {
        return LoadedRoot {
            label,
            fixtures: Vec::new(),
            unloadable: 0,
            gateable,
            absent: Some(format!("{label} fixture root is not present")),
        };
    }
    match discover_fixtures(root) {
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
        load_root("live-box", &local_root(), false),
        load_root("driver", &driver_root(), true),
    ];
    for root in &roots {
        if let Some(note) = &root.absent {
            eprintln!("conservation: {note}");
        }
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

    let run = adjudicate(&slices, &gated, &baseline);
    for line in run.report_lines() {
        eprintln!("{line}");
    }
    run
}

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
    assert_eq!(anthropic.fixtures, 250);
    assert_eq!(anthropic.asserted, 250);
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
