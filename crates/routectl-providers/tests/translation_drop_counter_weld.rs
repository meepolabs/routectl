//! The COUNTED contract of the translation-drop census.
//!
//! Exact-set equality, in BOTH directions, between the classes the marked arms
//! declare and the class literals reachable from the counter calls. The marker
//! side is `translation_drop_census/marker.rs` (which
//! `translation_drop_census.rs` pins the population of); the counter side is
//! `translation_drop_census/counter.rs`. Both are shared modules rather than a
//! parse written twice, so the two sides of the weld cannot disagree about what
//! the tree says -- only about what it means, which is the divergence this file
//! exists to surface.
//!
//! # Two disjoint vocabularies, and the disjointness IS a check
//!
//! `lane=` markers weld to `record_translation_drop`'s literals on
//! `(lane, class)`. `policy-action` markers weld to
//! `record_translation_policy_action`'s literals on CLASS ALONE: the grammar
//! carries no `lane=` on that verdict, so there is no lane on the marker side
//! to weld against, and no surface-to-lane mapping table is introduced here to
//! manufacture one. Such a table would cover three call sites while implying
//! every hand-typed lane literal in the tree was covered -- a partial fix that
//! reads as a complete one. The lane's real protection is a constant at the
//! call site, which is a code change, not a test.
//!
//! A class appearing in both vocabularies is a FAILURE, not an overlap to
//! tolerate. That check is what stops a future privacy strip from drifting back
//! into the drop namespace, where its volume would swamp the drop rate the
//! split was made to keep readable.
//!
//! # Set membership, never bijection
//!
//! One class legitimately covers several arms: three `gemini/schema.rs` arms
//! share `schema_keyword_unsupported`, whose only literal lives in
//! `gemini/request.rs` because the cleaner stays log-free and the egress tally
//! owns both the log and the counter. So the weld compares SETS of
//! `(lane, class)` pairs. A bijection would red-fail on correct code, and a
//! file-scoped weld would red-fail on that split -- which is why the counter
//! side is harvested over the whole crate and matched by lane rather than by
//! file. `a_counted_class_whose_literal_lives_in_another_surface_still_welds`
//! and `a_class_marked_on_no_lane_that_counts_it_fails` are the paired controls
//! on exactly that widening: it must not stop a genuine orphan from failing.
//!
//! # Every side fails LOUDLY
//!
//! An unresolvable lane or class expression is an error from the harvester, not
//! a skipped call. Fewer entries on one side means fewer things to match, which
//! is green by having less to check -- the failure mode this census exists to
//! refuse. Same for an empty population on either side.
//!
//! THE CEILING, restated because this weld is the one most likely to be
//! over-read: equality here says the marked vocabulary and the counted
//! vocabulary agree. It says NOTHING about an arm that drops with neither a
//! marker nor a counter. That blind spot is named in
//! `translation_drop_census.rs`'s module doc and registered there.

use std::collections::{BTreeMap, BTreeSet};

#[path = "translation_drop_census/counter.rs"]
mod counter;
#[path = "translation_drop_census/marker.rs"]
mod marker;

use counter::{
    Counter, CounterCall, METRICS_MODULE, code_only, harvest, harvest_crate, lane_seen_sites,
    src_path, vocabulary_overlaps, without_comments,
};
use marker::{
    LANES, MARKER_TOKEN, Marker, Verdict, census, expect, is_test_file, parse_file,
    production_files,
};

/// The lane of a `policy-action` class, resolved from the CALL rather than from
/// the marker. Used only to report which lane an unmatched policy class sits
/// on; never part of the compared key, per the class-alone weld above.
type PolicyLanes = BTreeMap<String, BTreeSet<String>>;

// ---------------------------------------------------------------------------
// The two sides.
// ---------------------------------------------------------------------------

/// The `(lane, class)` pairs the `lane=` markers declare.
fn marked_drop_pairs(markers: &[Marker]) -> BTreeSet<(String, String)> {
    markers
        .iter()
        .filter_map(|m| match (&m.verdict, &m.class) {
            (Verdict::Lane(lane), Some(class)) => Some((lane.clone(), class.clone())),
            _ => None,
        })
        .collect()
}

/// The classes the `policy-action` markers declare.
fn marked_policy_classes(markers: &[Marker]) -> BTreeSet<String> {
    markers
        .iter()
        .filter(|m| m.verdict == Verdict::PolicyAction)
        .filter_map(|m| m.class.clone())
        .collect()
}

/// The `(lane, class)` pairs reachable from the `record_translation_drop`
/// calls.
fn counted_drop_pairs(calls: &[CounterCall]) -> BTreeSet<(String, String)> {
    calls
        .iter()
        .filter(|c| c.counter == Counter::Drop)
        .filter_map(|c| c.class.clone().map(|class| (c.lane.clone(), class)))
        .collect()
}

/// The classes reachable from the `record_translation_policy_action` calls.
fn counted_policy_classes(calls: &[CounterCall]) -> BTreeSet<String> {
    calls
        .iter()
        .filter(|c| c.counter == Counter::PolicyAction)
        .filter_map(|c| c.class.clone())
        .collect()
}

fn policy_lanes(calls: &[CounterCall]) -> PolicyLanes {
    let mut lanes: PolicyLanes = BTreeMap::new();
    for call in calls.iter().filter(|c| c.counter == Counter::PolicyAction) {
        if let Some(class) = &call.class {
            lanes
                .entry(class.clone())
                .or_default()
                .insert(call.lane.clone());
        }
    }
    lanes
}

// ---------------------------------------------------------------------------
// The weld itself: the counted contract.
// ---------------------------------------------------------------------------

#[test]
fn the_marked_drop_vocabulary_equals_the_counted_drop_vocabulary() {
    // THE weld, on `(lane, class)`. Both directions in one assertion: a marked
    // drop with no counter literal is the overclaim defect (documented as a
    // drop nothing measures), and a counter literal no marker declares is its
    // inverse (a counted drop no arm admits to). Neither is a subset check --
    // a subset over an unseen drop converts a gap into a completeness claim
    // future readers stop re-deriving.
    let marked = marked_drop_pairs(&expect(census()));
    let counted = counted_drop_pairs(&expect(harvest_crate()));

    assert_eq!(
        marked, counted,
        "the marked and counted drop vocabularies diverged. A pair present only on the marked \
         side is a declared drop nothing counts; one present only on the counted side is a \
         counted drop no arm declares. Fix the code or the marker -- never widen this into a \
         subset check."
    );
}

#[test]
fn the_marked_policy_vocabulary_equals_the_counted_policy_vocabulary() {
    // On CLASS ALONE, per the class-alone weld in the module doc: the grammar
    // carries no lane on this verdict, and no mapping table is introduced here
    // to recover one.
    let calls = expect(harvest_crate());
    let marked = marked_policy_classes(&expect(census()));
    let counted = counted_policy_classes(&calls);

    assert_eq!(
        marked,
        counted,
        "the marked and counted policy-action vocabularies diverged (counted lanes: {:?}). A \
         class present on one side only is a policy action either declared and never counted or \
         counted and never declared.",
        policy_lanes(&calls)
    );
}

#[test]
fn the_drop_and_policy_action_vocabularies_are_disjoint() {
    // The disjointness is a CHECK, not a side effect of how the two counters
    // were wired. A class in both namespaces means one telemetry label carries
    // two meanings, and the split exists precisely because a policy action's
    // volume swamps the drop rate it shares a namespace with.
    let markers = expect(census());
    let calls = expect(harvest_crate());

    let marked_drop: BTreeSet<String> = marked_drop_pairs(&markers)
        .into_iter()
        .map(|(_, class)| class)
        .collect();
    let marked_policy = marked_policy_classes(&markers);
    let counted_drop: BTreeSet<String> = counted_drop_pairs(&calls)
        .into_iter()
        .map(|(_, class)| class)
        .collect();
    let counted_policy = counted_policy_classes(&calls);

    // Non-vacuity: an empty side satisfies every disjointness claim below.
    assert!(!marked_drop.is_empty() && !marked_policy.is_empty());
    assert!(!counted_drop.is_empty() && !counted_policy.is_empty());

    let (marked_overlap, counted_overlap) =
        vocabulary_overlaps(&marked_drop, &marked_policy, &counted_drop, &counted_policy);
    assert!(
        marked_overlap.is_empty(),
        "these classes are marked in both vocabularies: {marked_overlap:?}. One telemetry label \
         cannot mean both a wire-representability loss and a loss routectl chose."
    );
    assert!(
        counted_overlap.is_empty(),
        "these classes are passed to BOTH counters: {counted_overlap:?}. A privacy strip or a \
         config guard drifting back into the drop namespace is what this check refuses."
    );
}

// ---------------------------------------------------------------------------
// Positive controls on the harvesting mechanism, each aimed at a shape that
// demonstrably breaks a naive harvest.
// ---------------------------------------------------------------------------

#[test]
fn the_harvest_reads_a_call_whose_arguments_rustfmt_wrapped_onto_later_lines() {
    // A fully-qualified counter call exceeds the line width, so rustfmt puts
    // its arguments on FOLLOWING lines. A single-line scan reads no arguments
    // at all and reports every class on three of the four surfaces as
    // marked-but-not-counted. This control is aimed at the real tree, so it
    // also fails if the wrapping stops being the shape the harvest is written
    // against.
    let calls = expect(harvest_crate());
    let population = expect(population_of(&expect(counter::production_files())));
    // Identified by the harvest's OWN recorded line: a wrapped call is one
    // whose `(` is the last thing on its line, so anything a line-scoped scan
    // could have read there is empty.
    let wrapped: Vec<&CounterCall> = calls
        .iter()
        .filter(|call| {
            population
                .iter()
                .find(|(file, _)| *file == call.file)
                .and_then(|(_, source)| source.lines().nth(call.line - 1))
                .is_some_and(|line| line.trim_end().ends_with('('))
        })
        .collect();
    assert!(
        !wrapped.is_empty(),
        "no counter call in the tree has its arguments wrapped onto later lines, so this control \
         is aimed at nothing. Either rustfmt's shape changed or the harvest is reading a \
         different tree; re-aim the control before trusting the weld."
    );
    assert!(
        wrapped
            .iter()
            .all(|c| !c.lane.is_empty() && (c.class.is_some() || c.counter == Counter::LaneSeen)),
        "the harvest read a wrapped call without recovering its lane and class: {wrapped:?}"
    );
    assert!(
        wrapped.iter().any(|c| c.counter == Counter::Drop),
        "the wrapped shape no longer covers a drop call, which is the vocabulary the weld \
         compares: {wrapped:?}"
    );
}

#[test]
fn the_harvest_resolves_a_class_passed_through_a_tally_table() {
    // The gemini egress flushes its per-request flags through a
    // `for (fired, class) in [...]` table, so the class reaching the counter is
    // a loop BINDING and its literals sit in the table. About a third of the
    // counted classes resolve ONLY this way and are adjacent to no call.
    let calls = expect(harvest_crate());
    let from_table: BTreeSet<String> = calls
        .iter()
        .filter(|c| c.counter == Counter::Drop && c.file.starts_with("gemini/"))
        .filter_map(|c| c.class.clone())
        .collect();
    assert!(
        from_table.len() > 1,
        "the tally table resolved to {} classes; a single-literal result means the table form is \
         no longer recognized and its classes are reaching the weld from somewhere else (or not \
         at all): {from_table:?}",
        from_table.len()
    );
    assert!(
        from_table.contains("schema_keyword_unsupported"),
        "the table resolution lost a class the tree demonstrably carries: {from_table:?}"
    );
}

#[test]
fn the_harvest_resolves_a_lane_passed_as_a_constant() {
    // Only two of the four denominator sites pass a lane literal; the others
    // pass `LANE` and `super::PROVIDER_KIND`. A literal-only harvest reads
    // those two lanes as having no denominator site at all -- and a lane with
    // no denominator has a drop rate that reads zero forever.
    let calls = expect(harvest_crate());
    let denominators: BTreeSet<&str> = calls
        .iter()
        .filter(|c| c.counter == Counter::LaneSeen)
        .map(|c| c.lane.as_str())
        .collect();
    assert_eq!(
        denominators,
        LANES.iter().copied().collect::<BTreeSet<&str>>(),
        "the resolved denominator lanes are not the four fixed spellings; a lane missing here \
         passes its lane as an expression the harvest did not resolve"
    );
}

#[test]
fn a_counted_class_whose_literal_lives_in_another_surface_still_welds() {
    // The weld is SURFACE-scoped, not file-scoped: three `gemini/schema.rs`
    // arms carry a class whose only literal is in `gemini/request.rs`, because
    // the cleaner stays log-free and the egress tally owns the log and the
    // counter. A same-file weld red-fails on that correct placement, and a
    // red-failing check on correct code is what gets a check loosened.
    let markers = expect(census());
    let calls = expect(harvest_crate());
    let split: Vec<&Marker> = markers
        .iter()
        .filter(|m| {
            m.class.as_deref() == Some("schema_keyword_unsupported") && m.file == "gemini/schema.rs"
        })
        .collect();
    assert!(
        !split.is_empty(),
        "the cross-file split this control describes is gone, so it proves nothing about the \
         file-to-surface widening. Re-aim it at a split the tree actually carries."
    );
    let literal_files: BTreeSet<&str> = calls
        .iter()
        .filter(|c| c.class.as_deref() == Some("schema_keyword_unsupported"))
        .map(|c| c.file.as_str())
        .collect();
    assert!(
        !literal_files.contains("gemini/schema.rs") && !literal_files.is_empty(),
        "the split is no longer cross-FILE (literals in {literal_files:?}), so this control no \
         longer covers the widening"
    );
    // And the weld holds over it, which is the point.
    assert!(counted_drop_pairs(&calls).contains(&(
        "gemini".to_string(),
        "schema_keyword_unsupported".to_string()
    )));
}

// ---------------------------------------------------------------------------
// Mutation controls: each direction of the equality fails on a planted defect.
// The harvester runs over a PLANTED population, so the control exercises the
// real mechanism rather than a second implementation of it.
// ---------------------------------------------------------------------------

/// A synthetic one-file population: a marked arm plus a counter call, in the
/// shape the tree carries.
fn planted(marker_body: &str, call: &str) -> Vec<(String, String)> {
    let source = format!("// {MARKER_TOKEN} {marker_body}\nfn arm() {{\n    {call}\n}}\n");
    vec![("gemini/planted.rs".to_string(), source)]
}

fn planted_markers(population: &[(String, String)]) -> Vec<Marker> {
    let mut markers = Vec::new();
    for (file, source) in population {
        markers.extend(expect(parse_file(file, source)));
    }
    markers
}

#[test]
fn a_counter_literal_renamed_without_its_marker_fails_the_weld() {
    // Direction one: the literal moved, the marker did not. This is the shape a
    // rename takes when only one end is edited.
    let population = planted(
        "lane=gemini class=planted_drop_declared test=planted_arm_drops",
        r#"crate::translation_drop_metrics::record_translation_drop(
            "gemini",
            "planted_drop_renamed",
        );"#,
    );
    let marked = marked_drop_pairs(&planted_markers(&population));
    let counted = counted_drop_pairs(&expect(harvest(&population)));
    assert_ne!(
        marked, counted,
        "a renamed counter literal must diverge from its unrenamed marker"
    );
}

#[test]
fn a_marker_class_with_no_counter_literal_fails_the_weld() {
    // Direction two: the marker declares a counted drop that reaches no
    // counter. This is the overclaim defect -- a drop the audit says is
    // measured and telemetry never reports.
    let population = planted(
        "lane=gemini class=planted_drop_uncounted test=planted_arm_drops",
        "let _ = ();",
    );
    let marked = marked_drop_pairs(&planted_markers(&population));
    let counted = counted_drop_pairs(&expect(harvest(&population)));
    assert!(
        !marked.is_empty() && counted.is_empty(),
        "the plant must produce a marked class and no counted one; got {marked:?} / {counted:?}"
    );
    assert_ne!(marked, counted);
}

#[test]
fn a_class_marked_on_no_lane_that_counts_it_fails_the_weld() {
    // The control the surface-widening owes: matching by lane instead of by
    // file must NOT let a genuine orphan through. Same class, different lane --
    // which is a real defect (a phantom pair whose rate reads zero) and stays
    // red under the widened scope.
    let population = planted(
        "lane=openai-responses class=planted_cross_lane test=planted_arm_drops",
        r#"crate::translation_drop_metrics::record_translation_drop(
            "gemini",
            "planted_cross_lane",
        );"#,
    );
    let marked = marked_drop_pairs(&planted_markers(&population));
    let counted = counted_drop_pairs(&expect(harvest(&population)));
    assert_ne!(
        marked, counted,
        "a class counted on a lane no marker declares it on must still fail; the widening from \
         file to surface must not have weakened the check it was meant to strengthen"
    );
}

#[test]
fn a_policy_class_reaching_the_drop_counter_crosses_the_two_vocabularies() {
    // NOTE what this asserts, because the name it used to carry claimed more: it
    // tests a CROSS-SIDE pair (marked-policy vs counted-drop), which is not the
    // predicate either real disjointness assertion compares. The control for
    // those two is
    // `a_class_in_both_vocabularies_on_both_sides_breaks_both_overlap_checks`.
    let population = planted(
        "policy-action class=planted_policy_class test=planted_arm_counts",
        r#"crate::translation_drop_metrics::record_translation_drop(
            "bedrock-converse",
            "planted_policy_class",
        );"#,
    );
    let markers = planted_markers(&population);
    let calls = expect(harvest(&population));
    let marked_policy = marked_policy_classes(&markers);
    let counted_drop: BTreeSet<String> = counted_drop_pairs(&calls)
        .into_iter()
        .map(|(_, class)| class)
        .collect();
    assert!(
        !marked_policy.is_disjoint(&counted_drop),
        "a policy-action class reaching the drop counter must break disjointness"
    );
}

// ---------------------------------------------------------------------------
// The `test=` weld: a renamed or deleted pinning test is a red build.
// ---------------------------------------------------------------------------

#[test]
fn every_pinning_test_name_resolves_to_a_function_in_the_tree() {
    // Without this, deleting the test that pins a drop leaves the marker
    // claiming a pin that no longer exists -- and every other weld stays green,
    // because none of them reads `test=`.
    let markers = expect(census());
    let sources = expect(all_rust_sources());
    let named: BTreeSet<&str> = markers.iter().filter_map(|m| m.test.as_deref()).collect();
    assert!(
        !named.is_empty(),
        "no marker names a pinning test; an empty set satisfies the loop below"
    );
    for name in named {
        let hits = sources
            .iter()
            .filter(|(_, source)| holds_fn(source, name))
            .count();
        assert!(
            hits > 0,
            "the marker naming test={name} pins nothing: no `fn {name}` exists in this crate. \
             Restore the test or re-point the marker."
        );
    }
}

#[test]
fn the_pinning_test_resolution_refuses_a_name_no_function_carries() {
    // Paired positive control: the resolver above must be able to fail. A
    // substring match on the name would find `fn <name>_extra` and report a
    // deleted test as present.
    let source = "fn planted_pin_extra() {}\n";
    assert!(!holds_fn(source, "planted_pin"));
    assert!(holds_fn(source, "planted_pin_extra"));
    assert!(holds_fn("    async fn planted_pin() {}\n", "planted_pin"));
}

/// Whether `source` defines `fn <name>`, bounded so a longer name that starts
/// with `name` is not a match.
fn holds_fn(source: &str, name: &str) -> bool {
    let needle = format!("fn {name}");
    source.match_indices(&needle).any(|(at, _)| {
        source[at + needle.len()..].starts_with(|c: char| !c.is_alphanumeric() && c != '_')
    })
}

// ---------------------------------------------------------------------------
// The denominator weld: at most one lane-seen call site per lane.
// ---------------------------------------------------------------------------

#[test]
fn each_lane_has_at_most_one_lane_seen_call_site() {
    // The denominator every drop and policy rate divides by. Two sites on one
    // lane double its denominator and halve every rate on it, and the
    // corruption exists only in the UNION of the four surfaces -- no single
    // surface's own tests can see it.
    let calls = expect(harvest_crate());
    let sites = lane_seen_sites(&calls);
    assert_eq!(
        sites.keys().map(String::as_str).collect::<BTreeSet<&str>>(),
        LANES.iter().copied().collect::<BTreeSet<&str>>(),
        "every lane needs its denominator site; a lane absent here has a rate that reads zero \
         forever"
    );
    for (lane, found) in &sites {
        assert_eq!(
            found.len(),
            1,
            "{lane} is marked seen at {} sites ({found:?}); each extra site inflates the lane's \
             denominator and understates every rate on it",
            found.len()
        );
    }
}

#[test]
fn a_duplicated_lane_seen_call_site_fails() {
    // Planted duplicate: two files, one lane. The real defect shape -- four
    // agents each adding a denominator bump to their own surface.
    let population = vec![
        (
            "gemini/planted_one.rs".to_string(),
            "fn a() {\n    crate::translation_drop_metrics::record_translation_lane_seen(\n        \
             \"gemini\",\n    );\n}\n"
                .to_string(),
        ),
        (
            "gemini/planted_two.rs".to_string(),
            "fn b() {\n    crate::translation_drop_metrics::record_translation_lane_seen(\n        \
             \"gemini\",\n    );\n}\n"
                .to_string(),
        ),
    ];
    let calls = expect(harvest(&population));
    // Run the SAME tally the real assertion runs, then assert the one-site
    // invariant is VIOLATED. Asserting only that the fixture produced two calls
    // proved the plant, not the check: relaxing the real `== 1` to `>= 1` left
    // every test green -- verified.
    let sites = lane_seen_sites(&calls);
    let gemini = sites
        .get("gemini")
        .expect("the plant must register the gemini lane");
    assert_eq!(
        gemini.len(),
        2,
        "the plant must produce two denominator sites on one lane, so the one-site invariant the \
         real assertion enforces is violated here: {gemini:?}"
    );
}

// ---------------------------------------------------------------------------
// Fail-loud guards on the harvest itself.
// ---------------------------------------------------------------------------

#[test]
fn an_unresolvable_lane_expression_is_an_error_rather_than_a_skipped_call() {
    // Skipping the call would take a counted drop off the counter side, and
    // fewer entries there means fewer things for the weld to match: green by
    // having less to check.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        "fn a() {\n    crate::translation_drop_metrics::record_translation_drop(\n        \
         some_lane,\n        \"planted\",\n    );\n}\n"
            .to_string(),
    )];
    let why = harvest(&population).expect_err("an unresolved lane cannot be silently dropped");
    assert!(why.contains("some_lane"), "unexpected reason: {why}");
}

#[test]
fn an_unresolvable_class_expression_is_an_error_rather_than_a_skipped_call() {
    let population = vec![(
        "gemini/planted.rs".to_string(),
        "fn a() {\n    crate::translation_drop_metrics::record_translation_drop(\n        \
         \"gemini\",\n        some_class,\n    );\n}\n"
            .to_string(),
    )];
    let why = harvest(&population).expect_err("an unresolved class cannot be silently dropped");
    assert!(why.contains("some_class"), "unexpected reason: {why}");
}

#[test]
fn a_class_bound_by_a_tally_table_the_call_sits_outside_of_is_an_error() {
    // A table elsewhere in the file must not lend its literals to a call that
    // is not inside its loop body; that would invent counter coverage the code
    // does not have.
    let population = vec![(
        "gemini/planted.rs".to_string(),
        "fn a() {\n    for (fired, class) in [(true, \"planted_one\")] {\n        let _ = \
         (fired, class);\n    }\n    \
         crate::translation_drop_metrics::record_translation_drop(\n        \"gemini\",\n        \
         class,\n    );\n}\n"
            .to_string(),
    )];
    let why = harvest(&population).expect_err("a table the call sits outside of resolves nothing");
    assert!(why.contains("tally table"), "unexpected reason: {why}");
}

#[test]
fn a_harvest_that_recovered_no_call_is_a_failed_harvest() {
    // An empty counter side satisfies the equality against an empty marker
    // side, and both sides emptying at once is exactly what a broken parse
    // looks like.
    let population = vec![("gemini/planted.rs".to_string(), "fn a() {}\n".to_string())];
    let calls = expect(harvest(&population));
    assert!(
        calls.is_empty(),
        "a population with no call legitimately harvests nothing"
    );
    // The crate-wide harvest is where emptiness is refused, per counter.
    let calls = expect(harvest_crate());
    for counter in [Counter::Drop, Counter::PolicyAction, Counter::LaneSeen] {
        assert!(
            calls.iter().any(|c| c.counter == counter),
            "{} has no call in the tree; the harvest is reading the wrong place",
            counter.token()
        );
    }
}

#[test]
fn the_metrics_module_calls_the_counters_only_from_its_own_tests() {
    // The harvest excludes the counter's own module, whose unit tests bind
    // synthetic lanes to locals that resolve to no literal by design. That
    // exclusion is only safe while the module's PRODUCTION half calls nothing
    // -- so it is checked here rather than asserted in a comment. A production
    // call added there would otherwise be invisible to the weld.
    let source = expect(
        std::fs::read_to_string(src_path(METRICS_MODULE))
            .map_err(|err| format!("{METRICS_MODULE} must be readable ({err})")),
    );
    let lines: Vec<&str> = source.lines().collect();
    let first_cfg_test = lines
        .iter()
        .position(|line| line.trim() == "#[cfg(test)]")
        .expect("the metrics module keeps its unit tests behind cfg(test)");
    for (index, line) in lines.iter().enumerate().take(first_cfg_test) {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("///") {
            continue;
        }
        for token in [
            "record_translation_drop(",
            "record_translation_policy_action(",
            "record_translation_lane_seen(",
        ] {
            assert!(
                !trimmed.contains(token) || trimmed.contains("pub fn "),
                "{METRICS_MODULE} calls {token} outside its own tests on line {}; that call is \
                 excluded from the harvest, so it would be invisible to this weld. Move it, or \
                 narrow the exclusion.",
                index + 1
            );
        }
    }
}

#[test]
fn the_harvested_population_covers_more_than_the_four_marked_surfaces() {
    // The counter side is harvested crate-wide on purpose: a
    // `record_translation_drop` call added OUTSIDE the four marked surfaces
    // would be invisible to a surface-scoped harvest, which is the same
    // allowlist hole a file-scoped weld has one level down. Without this
    // guard the widening could be narrowed back with no signal.
    let harvested = expect(counter::production_files());
    let marked = expect(production_files());
    assert!(
        harvested.len() > marked.len(),
        "the crate-wide harvest must reach beyond the four marked surfaces; equal sizes mean it \
         was narrowed back to them"
    );
    // CONTENT-pinned, not size-pinned: `extra.len() > 10` is satisfied by the 18
    // top-level src files alone, so a walk that never descended into a single
    // subdirectory would still pass it. Naming one file per nesting level is what
    // makes "the walk descended" checkable.
    for required in [
        "bedrock/converse/messages.rs",
        "openai_compat/wire_lift/content.rs",
        "anthropic_api/cloak/identity.rs",
    ] {
        assert!(
            harvested.contains(&required.to_string()),
            "the crate walk must reach {required}; its absence means the walk stopped descending"
        );
    }
    assert!(
        harvested.iter().all(|f| !is_test_file(f)),
        "the harvest picked up test source: {harvested:?}"
    );
}

// ---------------------------------------------------------------------------
// Population helpers shared by the controls above.
// ---------------------------------------------------------------------------

fn population_of(files: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut population = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(src_path(file))
            .map_err(|err| format!("{file} must be readable ({err})"))?;
        population.push((file.clone(), source));
    }
    Ok(population)
}

/// Every Rust source of this crate, tests included: a `test=` name resolves
/// against the whole tree, since the pinning tests live in sidecar test files.
fn all_rust_sources() -> Result<Vec<(String, String)>, String> {
    let root = src_path("");
    let mut sources = Vec::new();
    for file in counter::rs_files(&root)? {
        let source = std::fs::read_to_string(root.join(&file))
            .map_err(|err| format!("{file} must be readable ({err})"))?;
        sources.push((file, source));
    }
    if sources.is_empty() {
        return Err("the crate holds no Rust source".to_string());
    }
    Ok(sources)
}

// ---------------------------------------------------------------------------
// Lexer controls. Each pins one shape that silently corrupted the harvest
// before the scan became comment-, string- and char-literal aware: a real call
// SKIPPED (fewer entries on one side is green by having less to match), or a
// phantom class INVENTED (an exact-set equality that red-fails correct code and
// gets loosened). Both are the failure modes this census exists to refuse.
// ---------------------------------------------------------------------------

#[test]
fn a_call_beside_a_string_containing_a_comment_marker_is_still_harvested() {
    // A URL literal on the call's own line made the scan read the line as
    // commented out and DROP the call from the counter side.
    let population = vec![(
        "gemini/probe.rs".to_string(),
        "fn f() {\n    let _u = \"https://example.com//v1\";\n             record_translation_drop(\"gemini\", \"real_class\");\n}\n"
            .to_string(),
    )];
    let calls = expect(harvest(&population));
    assert_eq!(calls.len(), 1, "the call beside a URL literal must be seen");
    assert_eq!(calls[0].class.as_deref(), Some("real_class"));
}

#[test]
fn a_call_inside_a_block_comment_is_not_harvested() {
    // A commented-out call was read as live, inventing a counted class that no
    // marker can match.
    let population = vec![(
        "gemini/probe.rs".to_string(),
        "fn f() {}\n/*\n    record_translation_drop(\"gemini\", \"ghost\");\n*/\n".to_string(),
    )];
    let calls = expect(harvest(&population));
    assert!(
        calls.is_empty(),
        "a call inside a block comment is not a call: {calls:?}"
    );
}

#[test]
fn a_commented_row_of_a_tally_table_contributes_no_class() {
    let population = vec![(
        "gemini/probe.rs".to_string(),
        "fn f() {\n    for (fired, class) in [\n        (a, \"live_class\"),\n                 // (b, \"ghost_class\"),\n    ] {\n                 record_translation_drop(\"gemini\", class);\n    }\n}\n"
            .to_string(),
    )];
    let classes: Vec<String> = expect(harvest(&population))
        .into_iter()
        .filter_map(|c| c.class)
        .collect();
    assert_eq!(classes, vec!["live_class".to_string()]);
}

#[test]
fn a_constant_inside_a_block_comment_does_not_shadow_the_real_one() {
    // A wrong lane resolves SILENTLY, which is worse than failing: a wrong pair
    // looks resolved.
    let population = vec![(
        "gemini/probe.rs".to_string(),
        "const LANE: &str = \"gemini\";\n/*\nconst LANE: &str = \"WRONG\";\n*/\n         fn f() {\n    record_translation_drop(LANE, \"cls\");\n}\n"
            .to_string(),
    )];
    let calls = expect(harvest(&population));
    assert_eq!(calls[0].lane, "gemini", "the commented const must not win");
}

#[test]
fn a_duplicate_str_constant_in_one_file_is_a_failed_harvest() {
    // Unresolvable by a line scanner, so it must fail rather than pick one.
    let population = vec![(
        "gemini/probe.rs".to_string(),
        "const LANE: &str = \"gemini\";\nfn g() {}\nconst LANE: &str = \"other\";\n         fn f() {\n    record_translation_drop(LANE, \"cls\");\n}\n"
            .to_string(),
    )];
    let why = harvest(&population).expect_err("a duplicate constant must fail loud");
    assert!(why.contains("more than once"), "unexpected reason: {why}");
}

#[test]
fn a_wrapped_use_item_of_the_counters_is_not_read_as_a_call() {
    // rustfmt wraps these imports at 100 columns and three sit at 93 today, so
    // one added name puts the token on a continuation line. A single-line test
    // hard-errored the whole harvest on legal formatting.
    let population = vec![(
        "gemini/probe.rs".to_string(),
        "use crate::translation_drop_metrics::{\n    record_translation_drop,\n             record_translation_lane_seen,\n};\nfn f() {}\n"
            .to_string(),
    )];
    let calls = expect(harvest(&population));
    assert!(calls.is_empty(), "an import is not a call: {calls:?}");
}

#[test]
fn blanking_non_code_preserves_the_byte_length() {
    // The invariant three call sites depend on: an offset found in the blanked
    // text is used to slice the ORIGINAL. Blanking char-for-char instead of
    // byte-for-byte shortens the result by `len_utf8()-1` per multibyte char, and
    // every downstream offset then either panics on a char boundary or reads the
    // wrong span. Zero of the other fixtures carry a non-ASCII byte, so nothing
    // else in this file would notice.
    for source in [
        "fn f() {} // plain ascii\n",
        "fn f() {} // caf\u{e9} \u{2014} multibyte\n",
        "let s = \"\u{2014}\u{2014}\u{2014}\";\nfn f() {}\n",
        "/* \u{2014} block \u{e9} */\nfn f() {}\n",
    ] {
        assert_eq!(
            code_only(source).len(),
            source.len(),
            "blanking must preserve byte length for {source:?}"
        );
        assert_eq!(
            without_comments(source).len(),
            source.len(),
            "comment blanking must preserve byte length for {source:?}"
        );
    }
}

#[test]
fn a_multibyte_comment_above_a_tally_table_does_not_shift_the_class_span() {
    // The failure this pins is an offset SHIFT, so the fixture puts multibyte
    // text above the table and asserts the classes still read whole.
    let population = vec![(
        "gemini/probe.rs".to_string(),
        "// caf\u{e9} \u{2014} a comment with multibyte characters\n         fn f() {\n    for (fired, class) in [\n        (a, \"class_alpha\"),\n                 (b, \"class_beta\"),\n    ] {\n                 record_translation_drop(\"gemini\", class);\n    }\n}\n"
            .to_string(),
    )];
    let classes: Vec<String> = expect(harvest(&population))
        .into_iter()
        .filter_map(|c| c.class)
        .collect();
    assert_eq!(
        classes,
        vec!["class_alpha".to_string(), "class_beta".to_string()],
        "a multibyte comment must not truncate or shift the class literals"
    );
}

#[test]
fn a_class_in_both_vocabularies_on_both_sides_breaks_both_overlap_checks() {
    // The control for the two real disjointness assertions. Both compare a set
    // against its SAME-SIDE sibling (marked vs marked, counted vs counted), so a
    // cross-side plant leaves both green -- which is how they shipped with no
    // proof they can fire at all.
    let population = vec![(
        "bedrock/converse/probe.rs".to_string(),
        "// TRANSLATION-DROP: lane=bedrock-converse class=shared_class test=probe_arm\n         fn a() {\n    record_translation_drop(\"bedrock-converse\", \"shared_class\");\n}\n         // TRANSLATION-DROP: policy-action class=shared_class test=probe_arm\n         fn b() {\n    record_translation_policy_action(\"bedrock-converse\", \"shared_class\");\n}\n"
            .to_string(),
    )];
    let markers = planted_markers(&population);
    let calls = expect(harvest(&population));

    let marked_drop: BTreeSet<String> = marked_drop_pairs(&markers)
        .into_iter()
        .map(|(_, class)| class)
        .collect();
    let marked_policy = marked_policy_classes(&markers);
    let counted_drop: BTreeSet<String> = counted_drop_pairs(&calls)
        .into_iter()
        .map(|(_, class)| class)
        .collect();
    let counted_policy = counted_policy_classes(&calls);

    // Run the SAME helper the real assertion runs, so this control exercises the
    // CHECK rather than merely the fixture. Asserting the sets overlap would pass
    // even with both real assertions neutered to an empty vec -- verified.
    let (marked_overlap, counted_overlap) =
        vocabulary_overlaps(&marked_drop, &marked_policy, &counted_drop, &counted_policy);
    assert_eq!(
        marked_overlap,
        vec!["shared_class".to_string()],
        "the marked-side overlap check must fire on a class in both vocabularies"
    );
    assert_eq!(
        counted_overlap,
        vec!["shared_class".to_string()],
        "the counted-side overlap check must fire on a class passed to both counters"
    );
}
