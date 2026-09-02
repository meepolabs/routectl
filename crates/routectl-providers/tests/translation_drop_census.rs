//! Census over the `TRANSLATION-DROP:` verdict markers carried by the four
//! request-translation surfaces (`openai_compat/wire_lift/`,
//! `bedrock/converse/`, `gemini/`, `openai_responses/`).
//!
//! This file owns the GRAMMAR RULES and the pinned population. The parser
//! itself lives in `translation_drop_census/marker.rs` as a shared module, so
//! the welds that cross-reference a marker against other code (counter
//! literals, loss-declaring logs, the file scope itself) consume the same
//! parse rather than each re-deriving one that could disagree.
//!
//! # The grammar, and why each rule is a PARSE ERROR
//!
//! Four verdict shapes, one line each, in a `//` or `///` comment:
//!
//! ```text
//! // TRANSLATION-DROP: lane=<lane> class=<class> test=<fn>
//! // TRANSLATION-DROP: policy-action class=<class> test=<fn>
//! // TRANSLATION-DROP: structural -- <reason nothing is lost>
//! // TRANSLATION-DROP: fidelity-risk -- <reason>
//! ```
//!
//! `unresolved -- <reason>` is a fifth, legal-but-empty verdict: an arm that
//! genuinely cannot be classified has somewhere honest to go instead of being
//! laundered into a passing completeness claim.
//!
//! Refusals, each with its own test below:
//!
//! - A prose verdict (`structural`, `fidelity-risk`, `unresolved`) carrying
//!   `class=`. That forbids the hybrid lie -- labelled as losing nothing,
//!   still counted as a loss.
//! - A counted verdict (`lane=`, `policy-action`) missing `class=` or `test=`.
//!   A counted loss with no class has no counter literal to weld against; one
//!   with no test has nothing pinning the behavior.
//! - A `lane=` value outside the four fixed spellings.
//! - `lane=` on a `policy-action` marker. The policy vocabulary welds on class
//!   alone; a lane on the marker would cover three call sites while implying
//!   every hand-typed lane literal in the tree was covered.
//! - A marker carrying a file path, a line number, or a board task id. A path
//!   or line rots on the next move with no signal, and a planning id is
//!   meaningless to a reader of this repo.
//! - An unrecognized tag. The tag vocabulary is closed (`class`, `test`,
//!   `silent`), so a retired tag spelling fails loudly instead of being
//!   ignored.
//!
//! # Test code is excluded by FILE LIST, never by `#[cfg(test)]` position
//!
//! `gemini/schema.rs` declares a test-only HELPER under `#[cfg(test)]` far
//! above its test module, and five production markers -- including all three
//! `schema_keyword_unsupported` arms -- live BELOW that attribute. A parser
//! that stopped at the first `#[cfg(test)]` would silently lose them, and
//! fewer markers on this side means fewer things for every downstream weld to
//! match, which means GREEN by having less to check. That is a false green of
//! exactly the class this census exists to refuse. So the exclusion is a
//! content-pinned list of test files, derived from their naming shape and
//! pinned against that derivation, and
//! [`the_parse_recovers_the_markers_below_a_test_only_helper_attribute`] is
//! the positive control aimed at the one place a naive cut demonstrably loses
//! data.
//!
//! # Every parse fails LOUDLY, and the ceiling is stated rather than implied
//!
//! An absent source, a marker that does not parse, or a census-wide empty
//! result is an error, never an empty set that satisfies every assertion below
//! by classifying nothing.
//!
//! THE CEILING: no source-derived side of this census can see a fully silent
//! drop, because a silent drop is defined by the ABSENCE of evidence -- there
//! is no log to harvest, no counter literal to resolve, and no marker unless a
//! human wrote one. [`EXPECTED_SILENT`] is therefore a HUMAN REGISTER, not a
//! derivation: it records arms a reviewer found to drop with no log and no
//! counter, and it can only shrink honestly. A census that names its blind
//! spot is honest; one that leaves it unstated gets read as a coverage
//! guarantee it never made.

use std::collections::{BTreeMap, BTreeSet};

#[path = "translation_drop_census/marker.rs"]
mod marker;
use marker::{
    LANES, MARKER_TOKEN, Marker, SURFACES, Verdict, census, census_over, expect, holds_line_number,
    holds_task_id, is_test_file, parse_file, parse_stub, read_source, src_root, surface_files,
};

/// Marker population per production FILE, content-pinned. Per-file rather than
/// per-surface: a per-surface count would absorb a whole file's loss inside its
/// surface's total, which is precisely how the test-code cut described above
/// would have hidden the `gemini/schema.rs` markers.
const EXPECTED_MARKERS_PER_FILE: &[(&str, usize)] = &[
    ("bedrock/converse/extras.rs", 3),
    ("bedrock/converse/messages.rs", 23),
    ("bedrock/converse/system.rs", 1),
    ("bedrock/converse/tools.rs", 8),
    ("gemini/cloudcode.rs", 2),
    ("gemini/mod.rs", 1),
    ("gemini/request.rs", 17),
    ("gemini/schema.rs", 5),
    ("openai_compat/wire_lift/content.rs", 6),
    ("openai_compat/wire_lift/response_format.rs", 4),
    ("openai_compat/wire_lift/thinking.rs", 3),
    ("openai_compat/wire_lift/tool_choice.rs", 5),
    ("openai_compat/wire_lift/tool_result.rs", 11),
    ("openai_compat/wire_lift/tool_use.rs", 2),
    ("openai_compat/wire_lift/tools.rs", 1),
    ("openai_responses/extras.rs", 6),
    ("openai_responses/messages.rs", 9),
    ("openai_responses/request.rs", 2),
    ("openai_responses/system.rs", 2),
    ("openai_responses/tools.rs", 4),
];

/// Population per verdict shape. A cheap review signal on bulk retagging: a
/// counted arm relabelled `structural` keeps the per-file total unchanged.
const EXPECTED_LANE_MARKERS: usize = 60;
const EXPECTED_POLICY_ACTION_MARKERS: usize = 5;
const EXPECTED_STRUCTURAL_MARKERS: usize = 49;

/// The `fidelity-risk` register: a same-dialect-reachable candidate, which is
/// a worse defect to be FILED rather than an accepted drop. Pinned by CONTENT
/// (file plus a phrase from the reason), never by size -- a size pin lets one
/// entry swap for another with no signal.
const EXPECTED_FIDELITY_RISK: &[(&str, &str)] = &[(
    "openai_responses/messages.rs",
    "a summary-only reasoning item loses its summary",
)];

/// The `unresolved` register, pinned as exactly empty: every marked arm in the
/// tree carries a verdict today. Pinned rather than left unchecked, so the
/// first arm nobody can classify is a review moment instead of a silent
/// widening.
const EXPECTED_UNRESOLVED: &[(&str, &str)] = &[];

/// The `silent` register: arms that drop with NO log and NO counter, keyed by
/// `(file, class)`. Pinned as exactly empty -- every counted arm in the tree
/// declares its loss through a log, a counter, or both, so nothing qualifies
/// today.
///
/// This list is a human register and not a derivation (see the ceiling in the
/// module doc), which is why it is pinned in BOTH directions: an entry added
/// here without a reviewer noticing is as much a defect as one removed.
const EXPECTED_SILENT: &[(&str, &str)] = &[];

/// Test files in the four surfaces, excluded from the census. Content-pinned
/// against the naming derivation in [`is_test_file`], so a test file added
/// under a name that derivation does not recognize is red here rather than
/// scanned as production source.
const EXPECTED_TEST_FILES: &[&str] = &[
    "bedrock/converse/eventstream_history_compat_tests.rs",
    "bedrock/converse/eventstream_tests.rs",
    "bedrock/converse/messages_content_drop_counter_tests.rs",
    "bedrock/converse/messages_document_policy_tests.rs",
    "bedrock/converse/messages_image_policy_tests.rs",
    "bedrock/converse/messages_other_role_tests.rs",
    "bedrock/converse/messages_reasoning_warn_tests.rs",
    "bedrock/converse/messages_tests.rs",
    "bedrock/converse/messages_tool_result_cache_control_tests.rs",
    "bedrock/converse/request_tests.rs",
    "bedrock/converse/request_tests_field_translation.rs",
    "bedrock/converse/request_tests_parity.rs",
    "bedrock/converse/response_history_compat_tests.rs",
    "gemini/cloud_project_id_tests.rs",
    "gemini/redirect_tests.rs",
    "gemini/request_drop_counter_tests.rs",
    "gemini/sse_tests.rs",
    "openai_responses/auth_wiring_tests.rs",
    "openai_responses/e2e_tests.rs",
    "openai_responses/excerpt_tests.rs",
    "openai_responses/header_merge_tests.rs",
    "openai_responses/lane_mapping_tests.rs",
    "openai_responses/lane_tag_emission_tests.rs",
    "openai_responses/reasoning_continuity_tests.rs",
    "openai_responses/redirect_tests.rs",
    "openai_responses/request_content_tests.rs",
    "openai_responses/request_drop_policy_tests.rs",
    "openai_responses/request_extras_tests.rs",
    "openai_responses/request_lane_observability_tests.rs",
    "openai_responses/request_test_support.rs",
    "openai_responses/request_tests.rs",
    "openai_responses/response_tests.rs",
    "openai_responses/sse_tests.rs",
];

// ---------------------------------------------------------------------------
// The pinned population.
// ---------------------------------------------------------------------------

#[test]
fn every_production_marker_in_the_four_surfaces_parses() {
    // THE grammar contract over the real tree: one unparseable marker is a
    // red build, so the grammar cannot drift into a second dialect the way
    // the prose it replaced did.
    let markers = expect(census());
    // The floor is the SUM of the pinned per-file counts, not the number of
    // pinned files: `>= 19` against a real population near 100 would let 80
    // markers vanish under a message that reads as a discharged non-vacuity
    // guard, which is worse than no guard.
    let pinned_total: usize = EXPECTED_MARKERS_PER_FILE.iter().map(|(_, n)| n).sum();
    assert_eq!(
        markers.len(),
        pinned_total,
        "recovered {} markers against {pinned_total} pinned; the parse broke or the tree moved",
        markers.len()
    );
}

#[test]
fn the_marker_population_matches_the_pinned_per_file_counts() {
    let markers = expect(census());
    let mut found: BTreeMap<&str, usize> = BTreeMap::new();
    for marker in &markers {
        *found.entry(marker.file.as_str()).or_default() += 1;
    }
    let pinned: BTreeMap<&str, usize> = EXPECTED_MARKERS_PER_FILE.iter().copied().collect();

    assert_eq!(
        found, pinned,
        "the per-FILE marker population drifted from the reviewed set. Confirm the change is \
         intended, then update EXPECTED_MARKERS_PER_FILE. A file whose count fell is the \
         signal this pin exists for: fewer markers means fewer things for every downstream \
         weld to match."
    );
}

#[test]
fn the_verdict_shapes_match_the_pinned_population() {
    // Per-verdict as well as per-file: a counted arm relabelled `structural`
    // leaves its file's total untouched.
    let markers = expect(census());
    let count = |predicate: fn(&Marker) -> bool| markers.iter().filter(|m| predicate(m)).count();

    assert_eq!(
        count(|m| matches!(m.verdict, Verdict::Lane(_))),
        EXPECTED_LANE_MARKERS,
        "the counted drop population changed"
    );
    assert_eq!(
        count(|m| m.verdict == Verdict::PolicyAction),
        EXPECTED_POLICY_ACTION_MARKERS,
        "the policy-action population changed"
    );
    assert_eq!(
        count(|m| m.verdict == Verdict::Structural),
        EXPECTED_STRUCTURAL_MARKERS,
        "the structural population changed"
    );
}

/// Match a register of `(file, phrase)` entries against the markers of one
/// verdict, in BOTH directions: every marker is claimed by an entry and every
/// entry claims a marker. Content-pinned by construction -- a size check would
/// let one entry swap for another with no signal.
fn assert_register(markers: &[Marker], verdict: &Verdict, register: &[(&str, &str)], label: &str) {
    let selected: Vec<&Marker> = markers.iter().filter(|m| m.verdict == *verdict).collect();

    for (file, phrase) in register {
        let hits = selected
            .iter()
            .filter(|m| m.file == *file && m.reason.contains(phrase))
            .count();
        assert_eq!(
            hits, 1,
            "the {label} register claims a marker in {file} whose reason holds {phrase:?}, and \
             the census found {hits}. Either the arm changed or the register is stale."
        );
    }
    for marker in &selected {
        let claimed = register
            .iter()
            .any(|(file, phrase)| marker.file == *file && marker.reason.contains(phrase));
        assert!(
            claimed,
            "{} carries a `{label}` marker no register entry claims: {}. Add it with its reason, \
             or reclassify the arm.",
            marker.file, marker.reason
        );
    }
    assert_eq!(
        selected.len(),
        register.len(),
        "the {label} population is {} while the register holds {} entries",
        selected.len(),
        register.len()
    );
}

#[test]
fn the_fidelity_risk_register_holds_exactly_the_reviewed_set() {
    // A same-dialect-reachable candidate is a defect to FILE, never an
    // accepted drop, so it cannot wear the ordinary marker and its population
    // is pinned by content.
    let markers = expect(census());
    assert_register(
        &markers,
        &Verdict::FidelityRisk,
        EXPECTED_FIDELITY_RISK,
        "fidelity-risk",
    );
}

#[test]
fn the_unresolved_register_holds_exactly_the_reviewed_set() {
    let markers = expect(census());
    assert_register(
        &markers,
        &Verdict::Unresolved,
        EXPECTED_UNRESOLVED,
        "unresolved",
    );
}

#[test]
fn the_silent_register_holds_exactly_the_reviewed_set() {
    // The one register no derivation can produce (see the ceiling in the
    // module doc), pinned in both directions for exactly that reason: an
    // entry added here is a claim a reviewer made, and an entry removed is a
    // defect someone fixed.
    let markers = expect(census());
    // COUNTED, not set-collapsed. Two silent arms in one file sharing a class
    // would collapse to one entry in a set, so deleting one of them would leave
    // this green -- reproducing inside the human register the exact
    // many-arms-per-class hole the counter weld already has. The shape is one
    // classification change away from live: `tool_choice.rs` already carries two
    // markers sharing `tool_choice_shape_unrepresentable`.
    let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();
    for marker in markers.iter().filter(|m| m.silent) {
        *found
            .entry((
                marker.file.clone(),
                marker.class.clone().unwrap_or_default(),
            ))
            .or_default() += 1;
    }
    let mut pinned: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (file, class) in EXPECTED_SILENT {
        *pinned
            .entry(((*file).to_string(), (*class).to_string()))
            .or_default() += 1;
    }

    assert_eq!(
        found, pinned,
        "the `silent` register drifted. An entry means an arm drops with no log and no counter; \
         confirm the arm really declares its loss nowhere, then update EXPECTED_SILENT."
    );
}

#[test]
fn the_test_file_exclusion_list_is_content_pinned() {
    // The exclusion is a FILE LIST and this is what keeps it honest: the
    // naming derivation and the reviewed list must agree exactly, so an entry
    // added or removed from either side is red here.
    //
    // Note what this direction CANNOT catch: both sets derive from
    // `is_test_file`, so an unrecognized test file (one named outside the
    // shape) is absent from both and agrees silently. That direction is
    // covered by `no_production_file_is_gated_entirely_behind_cfg_test`.
    let excluded: BTreeSet<String> = expect(surface_files())
        .into_iter()
        .filter(|f| is_test_file(f))
        .collect();
    let pinned: BTreeSet<String> = EXPECTED_TEST_FILES
        .iter()
        .map(|f| (*f).to_string())
        .collect();

    assert_eq!(
        excluded, pinned,
        "the test-file exclusion list drifted from the reviewed set. Confirm each entry really \
         is test code, then update EXPECTED_TEST_FILES."
    );
}

// ---------------------------------------------------------------------------
// Positive controls: the parse recovers real tokens, in the one place a naive
// test-code cut demonstrably loses them.
// ---------------------------------------------------------------------------

#[test]
fn the_parse_recovers_the_markers_below_a_test_only_helper_attribute() {
    // THE control on the test-code cut. `gemini/schema.rs` carries a
    // `#[cfg(test)]` test-only HELPER far above its test module, and
    // production markers live BELOW that attribute. A parser cutting at the
    // first `#[cfg(test)]` would return a shorter, still-plausible marker set
    // -- and every downstream weld would go green by having less to match.
    const FILE: &str = "gemini/schema.rs";
    let source = expect(read_source(FILE));
    let markers = expect(parse_file(FILE, &source));

    let first_cfg_test = source
        .lines()
        .position(|line| line.trim().starts_with("#[cfg(test)]"))
        .expect("the control needs the attribute it exists to describe");
    let below: Vec<&Marker> = markers
        .iter()
        .filter(|m| m.line > first_cfg_test + 1)
        .collect();

    assert_eq!(
        below.len(),
        markers.len(),
        "the control is no longer aimed at anything: every {FILE} marker now sits above the \
         first `#[cfg(test)]`, so a naive cut would lose nothing and this control proves \
         nothing. Re-aim it at a file where the hazard is live."
    );
    assert!(
        below
            .iter()
            .filter(|m| m.class.as_deref() == Some("schema_keyword_unsupported"))
            .count()
            >= 3,
        "the parse lost the schema-keyword arms below the test-only helper: {below:?}"
    );
    assert!(
        below
            .iter()
            .any(|m| m.verdict == Verdict::Lane("gemini".to_string())
                && m.test.as_deref() == Some("unresolvable_ref_reports_a_drop")),
        "the parse did not recover the marker {FILE} demonstrably carries; it recovered \
         {below:?}"
    );
}

#[test]
fn every_surface_contributes_markers_the_parse_recovered() {
    // Positive control on the harvest as a whole: without it a parse that
    // silently dropped a whole surface would still satisfy the per-file pin
    // for the surfaces it did read.
    let markers = expect(census());
    for surface in SURFACES {
        assert!(
            markers.iter().any(|m| m.file.starts_with(surface)),
            "no marker recovered from {surface}, which demonstrably carries them"
        );
    }
    let lanes: BTreeSet<&str> = markers
        .iter()
        .filter_map(|m| match &m.verdict {
            Verdict::Lane(lane) => Some(lane.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        lanes,
        LANES.iter().copied().collect::<BTreeSet<&str>>(),
        "the recovered lane values are not the four fixed spellings"
    );
}

#[test]
fn every_counted_marker_carries_a_class_and_a_test() {
    // Restates the grammar over the real population rather than over a stub:
    // the parse refuses a counted marker without both, so a green run here is
    // a statement about the tree.
    let markers = expect(census());
    for marker in markers.iter().filter(|m| m.verdict.is_counted()) {
        assert!(
            marker.class.is_some() && marker.test.is_some(),
            "{} on line {} parsed as counted without both tags",
            marker.file,
            marker.line
        );
    }
}

// ---------------------------------------------------------------------------
// Grammar controls: one per refusal.
// ---------------------------------------------------------------------------

#[test]
fn a_counted_marker_may_carry_the_silent_tag() {
    // Positive control on the tag whose register is empty today: without it
    // nothing exercises the `silent` parse, so the register could go green
    // because the tag stopped parsing rather than because no arm is silent.
    let markers = expect(parse_stub(
        "lane=gemini class=tool_def_unnamed test=t_drops silent",
    ));
    let [marker] = markers.as_slice() else {
        panic!("expected exactly one marker, got {markers:?}");
    };
    assert!(marker.silent, "the silent tag did not parse: {marker:?}");
    assert_eq!(marker.class.as_deref(), Some("tool_def_unnamed"));
}

#[test]
fn a_prose_verdict_carrying_the_silent_tag_is_a_parse_error() {
    // A verdict claiming nothing is lost cannot also claim the loss goes
    // undeclared.
    let why = parse_stub("structural silent -- nothing is lost")
        .expect_err("the counted tags belong only to a counted verdict");
    assert!(why.contains("silent"), "unexpected reason: {why}");
}

#[test]
fn a_structural_verdict_carrying_a_class_is_a_parse_error() {
    // The no-hybrid-lie rule: labelled as losing nothing, still counted.
    let why = parse_stub("structural class=image_source_unrepresentable -- nothing is lost")
        .expect_err("a structural verdict cannot also name a counted class");
    assert!(why.contains("carrying a class"), "unexpected reason: {why}");
}

#[test]
fn a_fidelity_risk_verdict_carrying_a_class_is_a_parse_error() {
    let why = parse_stub("fidelity-risk class=image_source_unrepresentable -- reachable")
        .expect_err("a fidelity-risk candidate is filed, not counted");
    assert!(why.contains("carrying a class"), "unexpected reason: {why}");
}

#[test]
fn a_structural_verdict_carrying_a_test_is_a_parse_error() {
    let why = parse_stub("structural test=some_arm_drops_and_warns -- nothing is lost")
        .expect_err("the counted tags belong only to a counted verdict");
    assert!(why.contains("test="), "unexpected reason: {why}");
}

#[test]
fn a_counted_verdict_with_no_class_is_a_parse_error() {
    let why = parse_stub("lane=gemini test=some_arm_drops_and_warns")
        .expect_err("a counted loss with no class has no literal to weld against");
    assert!(why.contains("no class"), "unexpected reason: {why}");
}

#[test]
fn a_counted_verdict_with_no_test_is_a_parse_error() {
    let why = parse_stub("lane=gemini class=image_source_no_inline_bytes")
        .expect_err("a counted loss with no test is unpinned");
    assert!(why.contains("no test"), "unexpected reason: {why}");
}

#[test]
fn a_policy_action_verdict_with_no_class_is_a_parse_error() {
    // Same rule as the drop vocabulary: a policy action is counted, so it has
    // a literal to weld against and must name it.
    let why = parse_stub("policy-action test=fingerprint_strip_bumps_the_counter_once")
        .expect_err("a counted policy action with no class has no literal to weld against");
    assert!(why.contains("no class"), "unexpected reason: {why}");
}

#[test]
fn a_lane_outside_the_four_spellings_is_a_parse_error() {
    let why = parse_stub("lane=bedrock-converce class=image_url_unrepresentable test=t_drops")
        .expect_err("a misspelled lane is a phantom lane whose rate reads zero forever");
    assert!(
        why.contains("bedrock-converce") && why.contains("fixed"),
        "unexpected reason: {why}"
    );
}

#[test]
fn a_policy_action_verdict_carrying_a_lane_is_a_parse_error() {
    let why = parse_stub(
        "policy-action lane=bedrock-converse class=client_fingerprint_stripped test=t_counts",
    )
    .expect_err("the policy vocabulary welds on class alone");
    assert!(why.contains("carries a lane"), "unexpected reason: {why}");
}

#[test]
fn a_marker_carrying_a_file_path_is_a_parse_error() {
    let why = parse_stub("structural -- handled in gemini/request.rs instead")
        .expect_err("a path field rots on the next move");
    assert!(why.contains("file path"), "unexpected reason: {why}");
}

#[test]
fn a_marker_carrying_a_line_number_is_a_parse_error() {
    let why = parse_stub("structural -- the counter fires at the flush on line 118")
        .expect_err("a line reference rots on the next edit above it");
    assert!(why.contains("line number"), "unexpected reason: {why}");
}

#[test]
fn a_marker_carrying_a_planning_id_is_a_parse_error() {
    let why = parse_stub("structural -- classified by the sweep in placeholder-slug.f2.7")
        .expect_err("a planning id is meaningless to a reader of this repo");
    assert!(why.contains("planning id"), "unexpected reason: {why}");
}

#[test]
fn a_forbidden_reference_on_a_wrapped_reason_line_is_a_parse_error() {
    // The reason wraps, so the refusal has to read the continuation too --
    // otherwise moving the offending words one line down evades the rule.
    let source = format!(
        "// {MARKER_TOKEN} structural -- the caller already emitted this\n\
         // content, see gemini/request.rs\nfn arm() {{}}\n"
    );
    let why = parse_file("surface/arm.rs", &source)
        .expect_err("a path on the wrapped line is the same leak");
    assert!(why.contains("file path"), "unexpected reason: {why}");
}

#[test]
fn an_unrecognized_tag_is_a_parse_error() {
    // The tag vocabulary is closed, so a retired tag spelling fails loudly
    // instead of riding along unread -- which is what let one marker dialect
    // drift into two before this grammar existed.
    let why = parse_stub("lane=gemini class=tool_def_unnamed test=t_drops shape=let-else")
        .expect_err("an unknown tag is not something to skip past");
    assert!(why.contains("unrecognized tag"), "unexpected reason: {why}");
}

#[test]
fn a_duplicated_tag_is_a_parse_error() {
    let why = parse_stub("lane=gemini class=tool_def_unnamed class=file_no_inline_bytes test=t")
        .expect_err("two classes on one marker name two different losses");
    assert!(why.contains("class twice"), "unexpected reason: {why}");
}

#[test]
fn a_tag_value_that_is_not_snake_case_is_a_parse_error() {
    let why = parse_stub("lane=gemini class=ToolDefUnnamed test=t_drops")
        .expect_err("the class is an operator-facing metrics literal, in one casing");
    assert!(why.contains("snake_case"), "unexpected reason: {why}");
}

#[test]
fn an_empty_tag_value_is_a_parse_error() {
    let why = parse_stub("lane=gemini class= test=t_drops")
        .expect_err("an empty class welds against nothing");
    assert!(why.contains("snake_case"), "unexpected reason: {why}");
}

#[test]
fn an_unknown_verdict_token_is_a_parse_error() {
    let why = parse_stub("TRANSLATION -- not a drop")
        .expect_err("a second dialect of this marker is what the fixed grammar refuses");
    assert!(why.contains("no verdict"), "unexpected reason: {why}");
}

#[test]
fn a_prose_verdict_with_no_reason_is_a_parse_error() {
    let why =
        parse_stub("structural").expect_err("the reason IS a structural verdict's whole evidence");
    assert!(why.contains("no `--"), "unexpected reason: {why}");
}

#[test]
fn a_prose_verdict_with_an_empty_reason_is_a_parse_error() {
    let why = parse_stub("structural --").expect_err("an empty reason asserts nothing");
    assert!(why.contains("reason is empty"), "unexpected reason: {why}");
}

#[test]
fn a_marker_declaring_no_verdict_is_a_parse_error() {
    let why = parse_stub("").expect_err("a bare marker token classifies nothing");
    assert!(
        why.contains("declares no verdict"),
        "unexpected reason: {why}"
    );
}

#[test]
fn a_marker_outside_a_comment_is_a_parse_error() {
    let source = format!("let note = \"{MARKER_TOKEN} structural -- x\";\nfn arm() {{}}\n");
    let why = parse_file("surface/arm.rs", &source)
        .expect_err("a verdict in a string literal describes no arm");
    assert!(why.contains("not a comment"), "unexpected reason: {why}");
}

#[test]
fn a_mid_file_marker_whose_arm_was_deleted_is_a_parse_error() {
    // The case the EOF-wide scan could not catch, and the one that actually
    // happens. TWO blank lines, because one is legitimately allowed: a deleted
    // arm leaves its marker separated from whatever follows by the blank line
    // that bounded the arm plus the one that bounded the next item.
    // happens: an arm deleted from under a marker in the MIDDLE of a file, with
    // unrelated code still below. An any()-to-EOF scan is satisfied by that
    // later code, so the guard would only ever fire for a marker in a file's
    // final comment block -- green for every real deletion.
    let source = format!(
        "fn before() {{}}\n\n// {MARKER_TOKEN} structural -- nothing is lost\n\n\nfn unrelated() {{}}\n"
    );
    let why = parse_file("surface/arm.rs", &source)
        .expect_err("a marker separated from the code below it anchors no arm");
    assert!(why.contains("anchors no code"), "unexpected reason: {why}");
}

#[test]
fn a_marker_anchoring_no_code_is_a_parse_error() {
    // The analogue of an unclosed block: a declaration whose subject is gone.
    let source = format!("fn arm() {{}}\n// {MARKER_TOKEN} structural -- nothing is lost\n");
    let why = parse_file("surface/arm.rs", &source)
        .expect_err("a marker with no code below it describes no arm");
    assert!(why.contains("anchors no code"), "unexpected reason: {why}");
}

// ---------------------------------------------------------------------------
// Fail-loud guards on the census itself.
// ---------------------------------------------------------------------------

#[test]
fn an_absent_source_is_an_error_rather_than_an_empty_set() {
    let why = read_source("gemini/schema.rs.absent")
        .expect_err("a source that does not exist cannot be read");
    assert!(why.contains("readable"), "unexpected reason: {why}");
}

#[test]
fn a_census_that_recovered_no_marker_is_a_failed_parse() {
    // The lesson this project already paid for: an empty parse result is a
    // FAILED parse, not a safe answer. Every pinned register is satisfied by
    // a population of zero.
    let population = vec![("surface/arm.rs".to_string(), "fn arm() {}\n".to_string())];
    let why =
        census_over(&population).expect_err("a census with no markers cannot pin a population");
    assert!(why.contains("no marker"), "unexpected reason: {why}");
}

#[test]
fn a_production_file_with_no_marker_is_not_itself_an_error() {
    // Most production files in these directories carry no drop arm, so a
    // per-FILE empty result has to be legal -- it is the census-wide empty
    // result above that is a failed parse.
    let markers = expect(parse_file("surface/plain.rs", "fn plain() {}\n"));
    assert!(markers.is_empty(), "unexpected markers: {markers:?}");
}

#[test]
fn a_surface_directory_that_holds_no_source_is_an_error() {
    let why = std::fs::read_dir(src_root().join("gemini/absent"))
        .map_err(|err| format!("must be a readable directory ({err})"))
        .expect_err("a surface that does not exist cannot be swept");
    assert!(
        why.contains("readable directory"),
        "unexpected reason: {why}"
    );
}

#[test]
fn the_task_id_scan_accepts_the_shapes_real_reasons_carry() {
    // Paired positive control on the strictest refusal: the must-be-ACCEPTED
    // side is part of the contract, because moving this boundary in one
    // direction moves it in both. Every string here is drawn from a reason
    // the tree actually carries.
    for accepted in [
        "the else arm cannot be reached (the borrow above proved it)",
        "every caller wraps this None as a Json content block",
        "f32 and f64 round-trip through the canonical schema",
        "build_system_instruction already reached the wire",
    ] {
        assert!(
            !holds_task_id(accepted),
            "the planning-id scan rejects a legitimate reason: {accepted}"
        );
    }
    assert!(
        holds_task_id("marked by the sweep in placeholder-slug.f2.7"),
        "the planning-id scan would not fire on the shape it exists to refuse"
    );
}

#[test]
fn the_line_number_scan_accepts_a_path_free_type_reference() {
    // Paired control for the line-number refusal, in both shapes it takes.
    // `::` is everywhere in these reasons and "two lines below" is a real one
    // the tree carries, so a scan that read either as a line reference would
    // red-fail on correct markers.
    assert!(!holds_line_number(
        "every caller wraps this as a ConverseToolResultContent::Json"
    ));
    assert!(!holds_line_number(
        "the freshly-built chunk content replaces it two lines below"
    ));
    assert!(holds_line_number("see the flush on line 118 of the tally"));
    assert!(holds_line_number("the tally at request.rs:118"));
}

#[test]
fn the_planning_id_scan_accepts_rust_float_type_prose() {
    // Paired control for the float-width exclusion. These surfaces translate
    // numeric wire fields, so `f32`/`f64` prose is likely; a scan that reads it
    // as a planning id would red-fail a legitimate reason and get loosened.
    for accepted in [
        "clamped to f32.0 before the cast",
        "widened to f64.5 on the way out",
        "f32.0 and f64.0 both round-trip",
    ] {
        assert!(
            !holds_task_id(accepted),
            "the planning-id scan rejects Rust float prose: {accepted}"
        );
    }
    // The positive half: a ONE-digit id is still refused, so the exclusion
    // above narrowed the scan by float width rather than by digit count.
    assert!(
        holds_task_id("classified by the sweep in placeholder-slug.f2.7"),
        "the planning-id scan must still refuse a one-digit id"
    );
}

#[test]
fn spelling_the_silent_tag_as_a_key_value_pair_is_a_parse_error() {
    // `silent` is a bare tag. Spelled `silent=<value>` it would otherwise fall
    // through to the `test` slot, BOTH fabricating a test name and clearing the
    // flag -- so the arm would satisfy the counted-verdict rule with a bogus
    // pin and vanish from EXPECTED_SILENT, the one register no derivation can
    // rebuild.
    let why = parse_stub("lane=gemini class=some_class silent=true")
        .expect_err("a bare tag spelled as key=value must not parse");
    assert!(why.contains("bare tag"), "unexpected reason: {why}");
}

#[test]
fn a_prose_verdict_may_use_the_word_silently_in_its_reason() {
    // `silent` is a BARE tag, so the leak check needs a whitespace boundary.
    // "silently" is the natural word for describing a non-loss and appears
    // throughout these surfaces' prose; a substring test would refuse a
    // legitimate reason and blame the counted-tag rule for it.
    let markers = expect(parse_stub(
        "structural -- the counter fires at the flush, so nothing vanishes silently",
    ));
    assert_eq!(markers.len(), 1);
    assert!(!markers[0].silent, "prose must not set the bare tag");

    // The positive half: the bare tag on a prose verdict is still refused.
    let why = parse_stub("structural -- nothing is lost silent")
        .expect_err("the bare tag belongs only to a counted verdict");
    assert!(why.contains("silent"), "unexpected reason: {why}");
}

#[test]
fn every_cfg_test_path_sidecar_is_on_the_exclusion_list() {
    // The direction the exclusion pin cannot see, as a REAL second producer.
    // `is_test_file` keys on the NAME, so a test sidecar named outside that
    // shape is scanned as production source -- and if it holds no marker it
    // agrees with every pin silently.
    //
    // The repo's convention is an OUTER attribute at the includer:
    //     #[cfg(test)]
    //     #[path = "messages_tests.rs"]
    //     mod sidecar_tests;
    // so the gating is what identifies test code, independent of the filename.
    // (An earlier version of this test scanned for an INNER `#![cfg(test)]`
    // attribute, which appears in zero files repo-wide -- a green test proving
    // nothing.)
    for file in expect(surface_files()) {
        let source = expect(read_source(&file));
        let lines: Vec<&str> = source.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if line.trim() != "#[cfg(test)]" {
                continue;
            }
            let Some(next) = lines.get(index + 1) else {
                continue;
            };
            let Some(rest) = next.trim().strip_prefix("#[path = \"") else {
                continue;
            };
            let Some(sidecar) = rest.split('"').next() else {
                continue;
            };
            let surface = file.rsplit_once('/').map_or("", |(dir, _)| dir);
            let relative = format!("{surface}/{sidecar}");
            assert!(
                EXPECTED_TEST_FILES.contains(&relative.as_str()),
                "{file} gates {sidecar} behind cfg(test), so it is test code, but it is not on \
                 EXPECTED_TEST_FILES -- it would be scanned as production source. Add it, or \
                 rename it to the test-file shape."
            );
        }
    }
}

#[test]
fn a_counted_markers_following_prose_is_not_part_of_the_marker() {
    // A counted verdict discards its continuation, so the free prose below it
    // is not part of the marker and carries none of the marker's restrictions.
    // Scanning it would turn an ordinary cross-reference into a parse error
    // that blames the marker for content it does not carry.
    let source = format!(
        "// {MARKER_TOKEN} lane=gemini class=some_class test=some_test\n         // See the sibling handler in schema.rs, two lines below the guard.\n         fn arm() {{}}\n"
    );
    let markers = expect(parse_file("surface/arm.rs", &source));
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].class.as_deref(), Some("some_class"));

    // But a PLANNING ID below a counted marker is still refused: that check is a
    // threat-surface rule, not a rot rule, so it is NOT line-scoped -- and for a
    // one-digit id this census is the only gate that sees it.
    let source = format!(
        "// {MARKER_TOKEN} lane=gemini class=some_class test=some_test\n\
         // classified by the sweep in placeholder-slug.f2.7\n\
         fn arm() {{}}\n"
    );
    let why = parse_file("surface/arm.rs", &source)
        .expect_err("a planning id below a counted marker is still refused");
    assert!(why.contains("planning id"), "unexpected reason: {why}");

    // The positive half: the same forbidden content ON the marker line is still
    // refused, so narrowing the scope did not disable the check.
    let why = parse_stub("lane=gemini class=c test=t -- see schema.rs")
        .expect_err("a file path on the marker line is still refused");
    assert!(why.contains("file path"), "unexpected reason: {why}");
}

#[test]
fn one_blank_line_between_a_marker_and_its_arm_still_anchors() {
    // Paired control for the anchoring window's lower bound. A `//` comment
    // block separated from its arm by ONE blank line is correct code; refusing
    // it would be a red build on a correct marker, which is what gets a refusal
    // loosened rather than fixed.
    let source = format!("// {MARKER_TOKEN} structural -- nothing is lost\n\nfn arm() {{}}\n");
    let markers = expect(parse_file("surface/arm.rs", &source));
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].reason, "nothing is lost");
}

#[test]
fn the_line_number_scan_accepts_ordinary_numeric_prose() {
    // Paired control for the colon form's file-reference bound. These surfaces
    // translate numeric wire fields, so JSON fragments, ratios and clock times
    // are plausible in a reason; refusing them is a red build on correct code.
    for accepted in [
        "the wire carries {\"budget_tokens\":1024} intact",
        "a 3:1 ratio is preserved",
        "the window expires at 12:30 upstream",
    ] {
        assert!(
            !holds_line_number(accepted),
            "the line-number scan rejects ordinary numeric prose: {accepted}"
        );
    }
    // The positive half: a real file:line reference is still refused, so the
    // bound narrowed the scan without disabling it.
    assert!(
        holds_line_number("the guard in request.rs:118"),
        "the line-number scan must still refuse a file:line reference"
    );
}

#[test]
fn a_planning_id_below_a_blank_comment_line_is_still_a_parse_error() {
    // The reason accumulation stops at a blank `//` line, so an author
    // reformatting a long reason into two paragraphs could otherwise move a
    // planning id out of every scan's reach with no signal. The threat-surface
    // scan reads the whole contiguous comment block instead.
    for verdict in [
        "structural -- nothing is lost",
        "lane=gemini class=some_class test=some_test",
    ] {
        let source = format!(
            "// {MARKER_TOKEN} {verdict}\n\
             //\n\
             // carried over from placeholder-slug.f2.7\n\
             fn arm() {{}}\n"
        );
        let why = parse_file("surface/arm.rs", &source)
            .expect_err("a planning id below a blank comment line must not evade the scan");
        assert!(why.contains("planning id"), "unexpected reason: {why}");
    }
}

#[test]
fn a_prose_verdict_carrying_a_retired_tag_is_a_parse_error() {
    // The closed vocabulary has to cover BOTH halves of the population. A prose
    // verdict discards everything between its verdict token and `--`, so the
    // retired `pattern:` spelling would otherwise survive unread on the 51 prose
    // markers while being refused on the counted ones.
    for body in [
        "structural pattern: explicit -- nothing is lost",
        "structural shape=let-else -- nothing is lost",
    ] {
        let why = parse_stub(body).expect_err("a tag between the verdict and `--` must not parse");
        assert!(
            why.contains("between its verdict"),
            "unexpected reason: {why}"
        );
    }

    // The positive half: the ordinary shape still parses.
    let markers = expect(parse_stub("structural -- nothing is lost"));
    assert_eq!(markers[0].reason, "nothing is lost");
}
