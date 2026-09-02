//! The SCOPE weld: every non-test `.rs` file in the four request-translation
//! surfaces is classified, exactly once, as either in scope for the drop
//! census or explicitly out of it with a stated reason.
//!
//! # Why the scope itself needs a weld
//!
//! Every other part of this census reads an ALLOWLIST of files: the marker
//! population is pinned per file, the counter weld resolves class literals per
//! surface, the declared-loss weld attributes markers to symbols. All three
//! are silent about a file that appears on NO list -- a new module dropped into
//! `gemini/` yields no arms to find, no markers to expect, and a green run.
//! That is the false green an allowlist buys, and this weld is what pays it
//! back: an unclassified file fails the equality below, which forces whoever
//! added it to say which side it is on.
//!
//! The allowlist is unavoidable rather than convenient. These four directories
//! also hold response translators, SSE state machines, transport auth, cookie
//! persistence and wire-type definitions; a directory-wide sweep for
//! drop-shaped code red-fails on all of them forever, and a check that
//! red-fails on correct code gets loosened rather than fixed.
//!
//! # Both sides are CONTENT-pinned, and counted rather than set-collapsed
//!
//! [`INSCOPE_FILES`] and [`EXPECTED_OUT_OF_SCOPE`] name every file. Neither is
//! pinned by SIZE: a size pin lets one file swap for another with no signal,
//! which is the difference between an exemption and a hole. The comparison
//! runs over OCCURRENCE COUNTS, not sets, so a file listed twice -- or listed
//! on both sides at once, where a union of sets would still look complete --
//! is red.
//!
//! # What this weld cannot do
//!
//! It checks that every file is classified, never that a classification is
//! CORRECT: nothing derivable distinguishes "response-side, so no caller
//! content can be lost here" from a wrong reading of the same file. The
//! reasons exist so that judgement is on the page for the next reader to
//! re-take, and
//! [`every_file_carrying_a_marker_is_classified_in_scope`] is the one
//! direction that is derivable -- a file the sweeps already marked cannot be
//! out of scope.
//!
//! Behind that sits the census-wide ceiling, stated in full in the module doc
//! of `translation_drop_census.rs`: no source-derived side of this census can
//! see a fully silent drop, because a silent drop is defined by the ABSENCE of
//! evidence. Classifying a file in scope does not assert its arms are found;
//! it asserts only that the file is being looked at.

use std::collections::BTreeMap;

#[path = "translation_drop_census/marker.rs"]
mod marker;
use marker::{SURFACES, census, expect, holds_task_id, production_files};

/// Files the drop sweeps read: the canonical-request-to-wire-body path of each
/// surface, plus the two files whose already-authored markers record a
/// response-side non-loss. Content-pinned. A file here is IN the census's
/// population; it is not a claim that the file carries a marker, because most
/// arms on this path lose nothing.
const INSCOPE_FILES: &[&str] = &[
    "bedrock/converse/extras.rs",
    "bedrock/converse/messages.rs",
    "bedrock/converse/request.rs",
    "bedrock/converse/system.rs",
    "bedrock/converse/tools.rs",
    "gemini/cloudcode.rs",
    "gemini/mod.rs",
    "gemini/request.rs",
    "gemini/schema.rs",
    "openai_compat/wire_lift/content.rs",
    "openai_compat/wire_lift/mod.rs",
    "openai_compat/wire_lift/response_format.rs",
    "openai_compat/wire_lift/thinking.rs",
    "openai_compat/wire_lift/tool_choice.rs",
    "openai_compat/wire_lift/tool_result.rs",
    "openai_compat/wire_lift/tool_use.rs",
    "openai_compat/wire_lift/tools.rs",
    "openai_responses/extras.rs",
    "openai_responses/messages.rs",
    "openai_responses/request.rs",
    "openai_responses/system.rs",
    "openai_responses/tools.rs",
];

/// Files that live in the four swept directories and carry no request
/// translation, each with the one-line reason it is exempt. The reason is the
/// whole value of the entry: the equality only proves the file was classified,
/// so the reason is what a reviewer re-checks when the file changes.
const EXPECTED_OUT_OF_SCOPE: &[(&str, &str)] = &[
    (
        "bedrock/converse/eventstream.rs",
        "decodes ConverseStream frames arriving from upstream; every arm reads response bytes, \
         never caller content",
    ),
    (
        "bedrock/converse/mod.rs",
        "provider entry point: hands the whole request to the translator in scope and the whole \
         response body to the response translator",
    ),
    (
        "bedrock/converse/response.rs",
        "translates the upstream response into canonical shape, so a loss here costs model \
         output rather than request content",
    ),
    (
        "bedrock/converse/response_types.rs",
        "deserialize-only response and stream wire types; no arm chooses between forwarding and \
         dropping",
    ),
    (
        "bedrock/converse/types.rs",
        "serialize-only request wire structs; serde omits absent optionals, and the choice to \
         leave one absent is made by the builders in scope",
    ),
    (
        "gemini/auth.rs",
        "injects the API-key header and never sees the request body",
    ),
    (
        "gemini/response.rs",
        "translates the upstream response into canonical shape, so a loss here costs model \
         output rather than request content",
    ),
    (
        "gemini/sse.rs",
        "response-side streaming state machine: accumulates upstream deltas into canonical \
         chunks",
    ),
    (
        "gemini/types.rs",
        "wire type definitions for both directions, with no arm that branches on caller content",
    ),
    (
        "openai_responses/auth.rs",
        "auth header dispatch per auth kind; carries no request body",
    ),
    (
        "openai_responses/client.rs",
        "provider construction and header assembly; reads the request only for its operator \
         header extras, which are not translated content",
    ),
    (
        "openai_responses/cookies.rs",
        "Cloudflare cookie-jar persistence; transport state with no request body in hand",
    ),
    (
        "openai_responses/mod.rs",
        "provider dispatch and SSE drain: the only REQUEST-body edit is setting the stream flag, and the \
         translation it delegates to is in scope",
    ),
    (
        "openai_responses/quota_headers.rs",
        "parses the upstream quota response-header family; deliberately tolerant because it runs \
         on an already-successful response",
    ),
    (
        "openai_responses/response.rs",
        "translates the upstream response into canonical shape, so a loss here costs model \
         output rather than request content",
    ),
    (
        "openai_responses/response_types.rs",
        "response-side wire types whose fieldless `#[serde(other)]` catchalls DISCARD an \
         unmodeled block's payload, so the loss costs model output rather than caller content; \
         note this differs from the converse response types, which preserve via `Other(Value)`",
    ),
    (
        "openai_responses/sse.rs",
        "response-side streaming state machine: reassembles interleaved upstream output items",
    ),
    (
        "openai_responses/types.rs",
        "serialize-only request wire structs; serde omits absent optionals, and the choice to \
         leave one absent is made by the builders in scope. The reasoning struct's \
         `#[serde(flatten)]` extras map CAN shadow a typed field (to_value is last-write-wins), \
         and what keeps that safe is the in-scope extras builder scrubbing the colliding keys \
         before construction -- a third typed field added here inherits that dependency",
    ),
];

/// Occurrence counts of the two pinned lists together. Counted rather than
/// collected into a set so a file named twice, or named on both sides at once,
/// is visible -- a set union would absorb either and still look complete.
fn classified_counts() -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for file in INSCOPE_FILES {
        *counts.entry((*file).to_string()).or_default() += 1;
    }
    for (file, _) in EXPECTED_OUT_OF_SCOPE {
        *counts.entry((*file).to_string()).or_default() += 1;
    }
    counts
}

/// The weld itself, over a SUPPLIED population, so the controls below can
/// exercise an added or a removed file without writing into the source tree.
fn classification_of(population: &[String]) -> Result<(), String> {
    classification_against(population, &classified_counts())
}

/// The comparison itself, with the classified side INJECTED so a control can
/// drive the duplicate leg. Occurrence COUNTS, not key sets: a key-set
/// comparison passes a file that appears on both classification lists, which
/// looks complete while classifying one file twice.
fn classification_against(
    population: &[String],
    classified: &BTreeMap<String, usize>,
) -> Result<(), String> {
    let mut present: BTreeMap<String, usize> = BTreeMap::new();
    for file in population {
        *present.entry(file.clone()).or_default() += 1;
    }
    let classified = classified.clone();
    if present == classified {
        return Ok(());
    }
    let unclassified: Vec<&String> = present
        .keys()
        .filter(|f| !classified.contains_key(*f))
        .collect();
    let stale: Vec<&String> = classified
        .keys()
        .filter(|f| !present.contains_key(*f))
        .collect();
    let duplicated: Vec<&String> = classified
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(file, _)| file)
        .collect();
    Err(format!(
        "the four surfaces are not exactly the classified set. Unclassified files (add each to \
         INSCOPE_FILES, or to EXPECTED_OUT_OF_SCOPE with a reason): {unclassified:?}. Pinned but \
         absent from the tree: {stale:?}. Classified twice: {duplicated:?}."
    ))
}

#[test]
fn the_four_surfaces_hold_exactly_the_classified_files() {
    // THE weld. A new file in any of the four directories lands on neither
    // list and fails here, which is the only thing that forces it to be
    // classified: with a bare allowlist it would contribute no arms, expect no
    // markers, and pass every other check in this census.
    let population = expect(production_files());
    if let Err(why) = classification_of(&population) {
        panic!("{why}");
    }
}

#[test]
fn neither_side_of_the_scope_split_is_empty() {
    // Non-vacuity on the pins themselves. An emptied INSCOPE_FILES with
    // everything moved to the exemption list satisfies the equality above
    // while asserting nothing about any drop arm.
    assert!(
        !INSCOPE_FILES.is_empty(),
        "no file is in scope; the census is looking at nothing"
    );
    assert!(
        !EXPECTED_OUT_OF_SCOPE.is_empty(),
        "no file is exempt, yet these directories demonstrably hold response-side and transport \
         code; the exemption list was emptied rather than earned"
    );
}

#[test]
fn every_out_of_scope_entry_carries_a_one_line_reason() {
    // The exemption, not the file name, is what a reviewer re-checks. An entry
    // whose reason is blank or a placeholder is an unexplained hole wearing the
    // shape of a decision.
    for (file, reason) in EXPECTED_OUT_OF_SCOPE {
        let words = reason.split_whitespace().count();
        // Eight, not four: the shortest real reason here runs to ten words, and a
        // four-word floor is cleared by a placeholder ("TODO fill this in"), so the
        // comment above would claim a placeholder check the assertion did not make.
        assert!(
            words >= 8,
            "{file} is exempt on {reason:?}, which is too short to state a reason a reader can \
             check"
        );
        assert!(
            !reason.contains('\n'),
            "{file}'s exemption reason spans lines; one line keeps the list readable at a glance"
        );
        assert!(
            !holds_task_id(reason),
            "{file}'s exemption reason carries a planning id; state a reason a reader of this \
             repo can check instead of pointing at a board"
        );
    }
}

#[test]
fn every_file_carrying_a_marker_is_classified_in_scope() {
    // NOTE on the population floor: this test asserts over whatever `census()`
    // recovered, and `census()` errors only on a census-wide EMPTY parse. The
    // floor that makes a SMALL non-empty parse fail is the per-file marker pin in
    // the sibling census binary -- deliberately not duplicated here, so the two
    // cannot drift. Do not add a second floor without retiring that one.
    // The one direction of this split that IS derivable. A `TRANSLATION-DROP:`
    // marker is the sweeps' own record that they read the file, so a
    // marker-carrying file on the exemption list is a contradiction between two
    // pins that would otherwise never meet.
    let markers = expect(census());
    for marker in &markers {
        assert!(
            INSCOPE_FILES.contains(&marker.file.as_str()),
            "{} carries a drop marker but is not on INSCOPE_FILES; a file the sweeps marked \
             cannot be out of the census's scope",
            marker.file
        );
    }
}

// ---------------------------------------------------------------------------
// Controls. Each drives the weld with a population it must refuse, so a
// classification that silently stopped comparing is visible.
// ---------------------------------------------------------------------------

#[test]
fn a_newly_added_surface_file_fails_the_weld() {
    // The false green this weld exists for: a module dropped into a swept
    // directory that no other check in this census can see.
    let mut population = expect(production_files());
    population.push("gemini/newly_added.rs".to_string());
    let why = classification_of(&population)
        .expect_err("an unclassified file must not pass the scope weld");
    assert!(
        why.contains("gemini/newly_added.rs") && why.contains("Unclassified"),
        "unexpected reason: {why}"
    );
}

#[test]
fn a_pinned_file_that_left_the_tree_fails_the_weld() {
    // The other direction: a deleted or renamed file leaves a pin claiming a
    // file that is no longer swept, which reads as coverage that no longer
    // exists.
    let mut population = expect(production_files());
    let removed = population
        .pop()
        .expect("the surfaces hold production source");
    let why = classification_of(&population)
        .expect_err("a pin with no file behind it must not pass the scope weld");
    assert!(
        why.contains(&removed) && why.contains("absent from the tree"),
        "unexpected reason: {why}"
    );
}

#[test]
fn the_real_population_is_neither_empty_nor_a_single_surface() {
    // Positive control on the population the weld reads. `production_files()`
    // failing loudly is its own contract, but a population that quietly
    // collapsed to one directory would still satisfy an equality against pins
    // trimmed to match it, so the shape is asserted here.
    let population = expect(production_files());
    for surface in SURFACES {
        assert!(
            population.iter().any(|f| f.starts_with(surface)),
            "no production file recovered from {surface}, which demonstrably holds them"
        );
    }
}

#[test]
fn a_file_classified_twice_fails_the_weld() {
    // Pins the occurrence-COUNT comparison against a key-set simplification.
    // Swapping the map comparison for a key-set one passes every other test here
    // while reopening the both-lists hole: a file present once but classified
    // twice looks complete to a set. This control drives the comparison directly
    // so the duplicate leg of the error is reachable by a test.
    let population = vec!["gemini/auth.rs".to_string()];
    let mut classified: BTreeMap<String, usize> = BTreeMap::new();
    classified.insert("gemini/auth.rs".to_string(), 2);

    let why = classification_against(&population, &classified)
        .expect_err("a file classified twice must fail the weld");
    assert!(
        why.contains("Classified twice"),
        "the duplicate leg must name the file: {why}"
    );

    // Paired control: the same population against a correct classified side
    // passes, so the failure above is the duplication and not the fixture.
    let mut healthy: BTreeMap<String, usize> = BTreeMap::new();
    healthy.insert("gemini/auth.rs".to_string(), 1);
    classification_against(&population, &healthy).expect("a one-to-one classification passes");
}
