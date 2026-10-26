//! Census over the `SEED-VERDICT:` markers: each retained seed site in the
//! providers and router source records why its baked rule stays baked, with a
//! tag from a closed vocabulary.
//!
//! The grammar and the sweep live in `translation_drop_census/seed.rs`, a child
//! of the drop census's marker parser, so both censuses refuse the same
//! file-path, line-number and planning-id shapes. This binary owns the pinned
//! population and the grammar controls.
//!
//! The pin is exact in both directions over occurrence counts per
//! `(crate, src-relative file, tag)`: a marker missing from the table and a
//! table row with no marker are both red, and a sweep that finds nothing is a
//! failed sweep rather than an empty population. Completeness (a seed nobody
//! marked) is out of reach of any source scan and is not claimed here.

use std::collections::BTreeSet;

#[path = "translation_drop_census/marker.rs"]
mod marker;
use marker::seed::{
    BAKED_PHRASE, BakedBlock, PROVIDERS, ROUTER, SEED_TOKEN, SeedMarker, SeedTag, SweptSource,
    UNNAMED_TEST_FILES, baked_blocks_in, baked_blocks_over, cfg_test_module_files,
    compare_to_table, crate_roots, expect_seed, parse_seed_file, production_sources, seed_census,
    seed_census_over, sweep_all, uncovered_baked_blocks,
};
use marker::{MARKER_TOKEN, Verdict, is_test_file, parse_file};

use SeedTag::{AcceptanceSeed, StaticSecurity, Structural};

/// Every seed marker in the tree, one row per marker. A file holding two
/// markers with the same tag is listed twice.
const EXPECTED_SEED_MARKERS: &[(&str, &str, SeedTag)] = &[
    (PROVIDERS, "anthropic_api/extras.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_api/extras.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_api/messages.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_api/messages.rs", Structural),
    (PROVIDERS, "anthropic_api/output_schema.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_api/parts.rs", StaticSecurity),
    (PROVIDERS, "anthropic_api/parts.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_api/parts.rs", StaticSecurity),
    (PROVIDERS, "anthropic_api/parts.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_api/request.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_api/request.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_api/request.rs", AcceptanceSeed),
    (PROVIDERS, "anthropic_family.rs", Structural),
    (PROVIDERS, "bedrock/converse/extras.rs", Structural),
    (PROVIDERS, "bedrock/converse/extras.rs", Structural),
    (PROVIDERS, "bedrock/invoke.rs", AcceptanceSeed),
    (PROVIDERS, "gemini/cloudcode.rs", AcceptanceSeed),
    (PROVIDERS, "model_profile.rs", Structural),
    (
        PROVIDERS,
        "openai_compat/dialects/deepseek.rs",
        AcceptanceSeed,
    ),
    (
        PROVIDERS,
        "openai_compat/dialects/deepseek.rs",
        AcceptanceSeed,
    ),
    (PROVIDERS, "openai_compat/dialects/util.rs", AcceptanceSeed),
    (
        PROVIDERS,
        "openai_compat/wire_lift/tool_result.rs",
        AcceptanceSeed,
    ),
    (PROVIDERS, "openai_responses/extras.rs", AcceptanceSeed),
    (PROVIDERS, "openai_responses/mod.rs", AcceptanceSeed),
    (PROVIDERS, "openai_responses/request.rs", AcceptanceSeed),
    (PROVIDERS, "tool_id.rs", AcceptanceSeed),
    (ROUTER, "catalog.rs", AcceptanceSeed),
    (ROUTER, "config/schema.rs", AcceptanceSeed),
];

fn stub(body: &str) -> String {
    format!("// {SEED_TOKEN} {body}\nfn item() {{}}\n")
}

fn parse_stub(body: &str) -> Result<Vec<SeedMarker>, String> {
    parse_seed_file("test-crate", "area/item.rs", &stub(body))
}

fn parse_source(source: &str) -> Result<Vec<SeedMarker>, String> {
    parse_seed_file("test-crate", "area/item.rs", source)
}

fn synthetic(krate: &str, file: &str, tag: SeedTag) -> SeedMarker {
    SeedMarker {
        krate: krate.to_string(),
        file: file.to_string(),
        line: 1,
        tag,
        reason: "a reason".to_string(),
    }
}

// ---------------------------------------------------------------------------
// The pinned population.
// ---------------------------------------------------------------------------

#[test]
fn the_seed_population_matches_the_pinned_table() {
    let markers = expect_seed(seed_census());
    if let Err(why) = compare_to_table(&markers, EXPECTED_SEED_MARKERS) {
        panic!(
            "the seed population drifted from the reviewed table: {why}. Confirm the change is \
             intended, then update EXPECTED_SEED_MARKERS."
        );
    }
}

#[test]
fn a_marker_missing_from_the_table_is_red() {
    let mut found: Vec<SeedMarker> = EXPECTED_SEED_MARKERS
        .iter()
        .map(|(k, f, t)| synthetic(k, f, *t))
        .collect();
    found.push(synthetic(PROVIDERS, "unlisted.rs", Structural));
    let why = compare_to_table(&found, EXPECTED_SEED_MARKERS)
        .expect_err("an unlisted marker must be red");
    assert!(why.contains("unlisted.rs"), "unexpected reason: {why}");
}

#[test]
fn a_table_row_with_no_marker_is_red() {
    let dropped = EXPECTED_SEED_MARKERS[0];
    let found: Vec<SeedMarker> = EXPECTED_SEED_MARKERS
        .iter()
        .filter(|row| **row != dropped)
        .map(|(k, f, t)| synthetic(k, f, *t))
        .collect();
    let why = compare_to_table(&found, EXPECTED_SEED_MARKERS)
        .expect_err("a row with no marker must be red");
    assert!(
        why.contains("0 marker(s) in source"),
        "unexpected reason: {why}"
    );
}

#[test]
fn a_duplicated_marker_is_red_against_a_single_row() {
    let found = vec![
        synthetic(PROVIDERS, "a.rs", Structural),
        synthetic(PROVIDERS, "a.rs", Structural),
    ];
    let why = compare_to_table(&found, &[(PROVIDERS, "a.rs", Structural)])
        .expect_err("counts, not sets, are compared");
    assert!(
        why.contains("2 marker(s) in source, 1 in the table"),
        "unexpected reason: {why}"
    );
}

#[test]
fn a_sweep_finding_zero_markers_is_red() {
    let population: Vec<SweptSource> = vec![(
        PROVIDERS.to_string(),
        "plain.rs".to_string(),
        "fn plain() {}\n".to_string(),
    )];
    let why = seed_census_over(&population).expect_err("an empty census is a failed parse");
    assert!(why.contains("no marker"), "unexpected reason: {why}");
}

#[test]
fn a_missing_router_source_root_is_red() {
    let mut roots = crate_roots();
    roots[1].1 = roots[1].1.join("does-not-exist");
    let why = sweep_all(&roots).expect_err("a missing tree must not shrink the sweep");
    assert!(
        why.contains("readable directory"),
        "unexpected reason: {why}"
    );
}

#[test]
fn both_crates_contribute_markers() {
    let markers = expect_seed(seed_census());
    let crates: BTreeSet<&str> = markers.iter().map(|m| m.krate.as_str()).collect();
    assert_eq!(crates, BTreeSet::from([PROVIDERS, ROUTER]));
}

// ---------------------------------------------------------------------------
// The test-file exclusion.
// ---------------------------------------------------------------------------

#[test]
fn the_unnamed_test_file_list_is_exactly_the_unrecognized_test_gated_modules() {
    // Derived, not asserted: every outline module either tree declares under a
    // `#[cfg(...)]` naming `test` that the naming derivation misses must be on
    // the list, and every entry on the list must be such a module.
    let all = expect_seed(sweep_all(&crate_roots()));
    let present: BTreeSet<(String, String)> =
        all.iter().map(|(k, f, _)| (k.clone(), f.clone())).collect();
    let mut derived = BTreeSet::new();
    for (krate, file, source) in &all {
        for module in cfg_test_module_files(file, source) {
            assert!(
                present.contains(&(krate.clone(), module.clone())),
                "{krate} {file} declares the test module {module}, which the sweep did not find"
            );
            if !is_test_file(&module) {
                derived.insert((krate.clone(), module));
            }
        }
    }
    let pinned: BTreeSet<(String, String)> = UNNAMED_TEST_FILES
        .iter()
        .map(|(k, f)| ((*k).to_string(), (*f).to_string()))
        .collect();
    assert_eq!(derived, pinned);
}

#[test]
fn a_compound_cfg_naming_test_gates_a_module_and_a_feature_string_does_not() {
    let source = "#[cfg(all(test, feature = \"lane\"))]\nmod gated;\n\
                  #[cfg(any(test, feature = \"test-utils\"))]\n#[path = \"support.rs\"]\nmod support;\n\
                  #[cfg(feature = \"test-utils\")]\nmod utils_only;\n\
                  #[cfg(not(test))]\nmod release_only;\n\
                  #[cfg(all(test, feature = \"lane\"))]\n#[global_allocator]\nstatic A: u8 = 0;\n";
    assert_eq!(
        cfg_test_module_files("lib.rs", source),
        vec!["gated.rs".to_string(), "support.rs".to_string()]
    );
}

#[test]
fn a_test_gated_module_is_found_through_stacked_attributes_a_wrapped_cfg_and_any_visibility() {
    for (label, source, expected) in [
        (
            "stacked attributes",
            "#[cfg(test)]\n#[allow(clippy::unwrap_used)]\n#[path = \"stacked.rs\"]\n\
             #[rustfmt::skip]\nmod stacked;\n",
            "stacked.rs",
        ),
        (
            "wrapped cfg",
            "#[cfg(all(\n    test,\n    feature = \"lane\"\n))]\nmod wrapped;\n",
            "wrapped.rs",
        ),
        (
            "restricted visibility",
            "#[cfg(test)]\npub(super) mod restricted;\n",
            "restricted.rs",
        ),
        (
            "path-restricted visibility",
            "#[cfg(test)]\npub(in crate::area) mod scoped;\n",
            "scoped.rs",
        ),
    ] {
        assert_eq!(
            cfg_test_module_files("lib.rs", source),
            vec![expected.to_string()],
            "{label}"
        );
    }
}

#[test]
fn a_wrapped_cfg_without_a_test_atom_gates_nothing() {
    let source = "#[cfg(all(\n    feature = \"test-utils\",\n    unix\n))]\nmod utils_only;\n\
                  #[cfg(test)]\nstatic NOT_A_MODULE: u8 = 0;\nmod after;\n";
    assert!(cfg_test_module_files("lib.rs", source).is_empty());
}

#[test]
fn the_production_sweep_drops_every_excluded_file() {
    let all = expect_seed(sweep_all(&crate_roots()));
    let production = expect_seed(production_sources(&crate_roots()));
    let kept: BTreeSet<(&str, &str)> = production
        .iter()
        .map(|(k, f, _)| (k.as_str(), f.as_str()))
        .collect();
    for entry in UNNAMED_TEST_FILES {
        assert!(!kept.contains(entry), "{entry:?} must be excluded");
    }
    assert!(kept.len() < all.len() && !kept.is_empty());
}

// ---------------------------------------------------------------------------
// The grammar.
// ---------------------------------------------------------------------------

#[test]
fn each_tag_parses_with_its_reason() {
    for (body, tag, reason) in [
        (
            "acceptance-seed -- the host accepts it",
            AcceptanceSeed,
            "the host accepts it",
        ),
        (
            "static-security -- never relaxed",
            StaticSecurity,
            "never relaxed",
        ),
        (
            "structural -- routectl cannot build the shape",
            Structural,
            "routectl cannot build the shape",
        ),
    ] {
        let markers = parse_stub(body).unwrap_or_else(|why| panic!("{body}: {why}"));
        assert_eq!(markers.len(), 1, "{body}");
        assert_eq!(
            (markers[0].tag, markers[0].reason.as_str(), markers[0].line),
            (tag, reason, 1),
            "{body}"
        );
    }
}

#[test]
fn representability_is_refused_naming_the_drop_marker() {
    let why = parse_stub("representability -- the wire cannot carry it")
        .expect_err("representability belongs to the drop census");
    assert!(why.contains(MARKER_TOKEN), "unexpected reason: {why}");
}

#[test]
fn an_unknown_tag_is_refused() {
    let why = parse_stub("learned -- something").expect_err("the vocabulary is closed");
    assert!(why.contains("no seed tag"), "unexpected reason: {why}");
}

#[test]
fn a_marker_with_no_dashes_is_refused() {
    let why = parse_stub("acceptance-seed the host accepts it").expect_err("no `--`");
    assert!(why.contains("-- <reason>"), "unexpected reason: {why}");
}

#[test]
fn a_marker_with_an_empty_reason_is_refused() {
    let why = parse_stub("acceptance-seed --   ").expect_err("empty reason");
    assert!(why.contains("empty reason"), "unexpected reason: {why}");
}

#[test]
fn a_marker_with_no_tag_is_refused() {
    let why = parse_stub("-- a reason").expect_err("no tag");
    assert!(why.contains("no tag"), "unexpected reason: {why}");
}

#[test]
fn a_token_between_the_tag_and_the_dashes_is_refused() {
    let why = parse_stub("acceptance-seed extra -- a reason").expect_err("exact shape");
    assert!(why.contains("between its tag"), "unexpected reason: {why}");
}

#[test]
fn a_marker_outside_a_comment_is_refused() {
    let source = format!("let note = \"{SEED_TOKEN} structural -- x\";\nfn item() {{}}\n");
    let why = parse_source(&source).expect_err("a string literal is not a marker");
    assert!(why.contains("not a comment"), "unexpected reason: {why}");
}

#[test]
fn a_marker_anchoring_no_code_is_refused() {
    let source =
        format!("fn before() {{}}\n\n// {SEED_TOKEN} structural -- x\n\n\nfn later() {{}}\n");
    let why = parse_source(&source).expect_err("the item moved out from under it");
    assert!(why.contains("anchors no code"), "unexpected reason: {why}");
}

#[test]
fn a_reason_carrying_forbidden_content_is_refused_anywhere_in_the_block() {
    // Each shape on the marker line, on a wrapped line, and below a blank
    // comment line that ends the reason.
    let shapes = [
        ("file path", "see gemini/request.rs"),
        ("line number", "the flush on line 118"),
        ("planning id", "from placeholder-slug.f2.7"),
    ];
    for (label, text) in shapes {
        let sources = [
            stub(&format!("acceptance-seed -- {text}")),
            format!("// {SEED_TOKEN} acceptance-seed -- a reason\n// {text}\nfn item() {{}}\n"),
            format!("// {SEED_TOKEN} acceptance-seed -- a reason\n//\n// {text}\nfn item() {{}}\n"),
        ];
        for source in sources {
            let why =
                parse_source(&source).expect_err(&format!("{label} must be refused in:\n{source}"));
            assert!(why.contains(label), "{label}: unexpected reason: {why}");
        }
    }
}

#[test]
fn a_wrapped_reason_stops_at_a_drop_marker() {
    let source = format!(
        "// {SEED_TOKEN} acceptance-seed -- the host accepts it\n\
         // {MARKER_TOKEN} structural -- nothing is lost\nfn item() {{}}\n"
    );
    let markers = expect_seed(parse_source(&source));
    assert_eq!(markers[0].reason, "the host accepts it");
}

#[test]
fn two_stacked_seed_lines_parse_as_two_markers() {
    let source = format!(
        "/// {SEED_TOKEN} static-security -- never\n/// relaxed\n\
         /// {SEED_TOKEN} acceptance-seed -- accepted today\nconst LIST: &[&str] = &[];\n"
    );
    let markers = expect_seed(parse_source(&source));
    assert_eq!(markers.len(), 2);
    assert_eq!(
        (markers[0].tag, markers[0].reason.as_str()),
        (StaticSecurity, "never relaxed")
    );
    assert_eq!(
        (markers[1].tag, markers[1].reason.as_str()),
        (AcceptanceSeed, "accepted today")
    );
}

#[test]
fn a_seed_line_above_a_drop_marker_leaves_the_drop_parse_unchanged() {
    for drop in [
        "structural -- nothing is lost",
        "lane=gemini class=some_class test=some_test",
    ] {
        let plain = format!("// {MARKER_TOKEN} {drop}\nfn arm() {{}}\n");
        let seeded = format!("// {SEED_TOKEN} acceptance-seed -- a reason\n{plain}");
        let strip_line = |source: &str| {
            let mut parsed = parse_file("surface/arm.rs", source).expect("the drop marker parses");
            for marker in &mut parsed {
                marker.line = 0;
            }
            parsed
        };
        let before = strip_line(&plain);
        let after = strip_line(&seeded);
        assert_eq!(before.len(), 1, "{drop}");
        assert_eq!(before, after, "{drop}");
        assert!(after[0].verdict != Verdict::Unresolved);
        assert_eq!(expect_seed(parse_source(&seeded)).len(), 1, "{drop}");
    }
}

// ---------------------------------------------------------------------------
// Comment-block handling.
// ---------------------------------------------------------------------------

#[test]
fn a_seed_line_directly_below_a_prose_drop_marker_is_refused() {
    for verdict in ["structural", "fidelity-risk", "unresolved"] {
        let source = format!(
            "// {MARKER_TOKEN} {verdict} -- nothing is lost\n\
             // {SEED_TOKEN} acceptance-seed -- a reason\nfn item() {{}}\n"
        );
        let why =
            parse_source(&source).expect_err(&format!("{verdict}: the drop reason absorbs it"));
        assert!(
            why.contains("above the TRANSLATION-DROP marker"),
            "{verdict}: unexpected reason: {why}"
        );
        assert!(!why.contains("  "), "{verdict}: broken wrap in: {why}");
    }
}

#[test]
fn a_seed_line_below_a_counted_drop_marker_or_a_blank_comment_line_is_accepted() {
    let sources = [
        format!(
            "// {MARKER_TOKEN} lane=gemini class=some_class test=some_test\n\
             // {SEED_TOKEN} acceptance-seed -- a reason\nfn item() {{}}\n"
        ),
        format!(
            "// {MARKER_TOKEN} structural -- nothing is lost\n//\n\
             // {SEED_TOKEN} acceptance-seed -- a reason\nfn item() {{}}\n"
        ),
    ];
    for source in sources {
        let markers = parse_source(&source).unwrap_or_else(|why| panic!("{source}: {why}"));
        assert_eq!(markers[0].reason, "a reason", "{source}");
    }
}

#[test]
fn prose_after_the_seed_paragraph_in_the_same_comment_run_is_refused() {
    let source = format!(
        "// {SEED_TOKEN} acceptance-seed -- the host\n/// accepts it\n//\n// unrelated prose\nfn item() {{}}\n"
    );
    let why = parse_source(&source).expect_err("the marker must close its comment run");
    assert!(why.contains("last paragraph"), "unexpected reason: {why}");
    assert!(!why.contains("  "), "broken wrap in: {why}");
}

#[test]
fn a_wrapped_reason_ends_at_the_end_of_its_comment_block() {
    let sources = [
        format!("// {SEED_TOKEN} acceptance-seed -- the host\n/// accepts it\nfn item() {{}}\n"),
        format!(
            "// {SEED_TOKEN} acceptance-seed -- the host\n/// accepts it\n//\nfn item() {{}}\n"
        ),
        format!("//! {SEED_TOKEN} acceptance-seed -- the host\n//! accepts it\nfn item() {{}}\n"),
    ];
    for source in sources {
        let markers = expect_seed(parse_source(&source));
        assert_eq!(markers[0].reason, "the host accepts it", "{source}");
    }
}

#[test]
fn a_blank_inner_doc_line_ends_a_seed_block() {
    let source = format!(
        "//! {SEED_TOKEN} acceptance-seed -- the host accepts it\n//!\n//! unrelated prose\nfn item() {{}}\n"
    );
    let why = parse_source(&source).expect_err("a blank `//!` line ends the block");
    assert!(why.contains("last paragraph"), "unexpected reason: {why}");
}

#[test]
fn a_drop_reason_wrapped_across_inner_doc_lines_reads_without_the_bang() {
    let source = format!("//! {MARKER_TOKEN} structural -- nothing\n//! is lost\nfn arm() {{}}\n");
    let parsed = parse_file("surface/arm.rs", &source).expect("the drop marker parses");
    assert_eq!(parsed[0].reason, "nothing is lost");
}

// ---------------------------------------------------------------------------
// The baked-verdict prose guard.
// ---------------------------------------------------------------------------

/// Comments in the production sweep stating a baked seed verdict. Each is
/// one comment however many paragraphs it spans.
const BAKED_BLOCK_TOTAL: usize = 56;

fn baked(source: &str) -> Vec<BakedBlock> {
    expect_seed(baked_blocks_in("test-crate", "area/item.rs", source))
}

#[test]
fn every_baked_verdict_comment_holds_a_marker() {
    let blocks = expect_seed(baked_blocks_over(&expect_seed(production_sources(
        &crate_roots(),
    ))));
    let uncovered = uncovered_baked_blocks(&blocks);
    assert!(uncovered.is_empty(), "{}", uncovered.join("\n"));
    assert_eq!(
        blocks.len(),
        BAKED_BLOCK_TOTAL,
        "the baked-verdict comment count drifted; recount from source, then update \
         BAKED_BLOCK_TOTAL"
    );
}

#[test]
fn a_baked_comment_with_neither_marker_is_red() {
    let blocks = baked(&format!(
        "/// {BAKED_PHRASE}: kept because\nfn item() {{}}\n"
    ));
    assert_eq!(blocks.len(), 1);
    assert_eq!(uncovered_baked_blocks(&blocks).len(), 1);
}

#[test]
fn a_drop_marker_in_the_same_comment_covers_and_its_removal_uncovers() {
    let prose = format!("    // Kept. {BAKED_PHRASE}: it stands.\n");
    let drop = format!("    // {MARKER_TOKEN} lane=gemini class=some_class test=some_test\n");
    let covered = baked(&format!("fn f() {{\n{prose}{drop}    arm();\n}}\n"));
    let bare = baked(&format!("fn f() {{\n{prose}    arm();\n}}\n"));
    assert!(uncovered_baked_blocks(&covered).is_empty());
    assert_eq!(uncovered_baked_blocks(&bare).len(), 1);
}

#[test]
fn a_seed_line_as_the_last_paragraph_of_the_doc_comment_covers() {
    let source = format!(
        "/// A list.\n///\n/// {BAKED_PHRASE}: kept.\n///\n\
         /// {SEED_TOKEN} acceptance-seed -- a reason\nconst LIST: &[&str] = &[];\n"
    );
    let blocks = baked(&source);
    assert_eq!(blocks.len(), 1);
    assert!(uncovered_baked_blocks(&blocks).is_empty());
}

#[test]
fn a_phrase_wrapped_across_comment_lines_is_matched() {
    for source in [
        "/// the rule. Baked\n/// seed verdict: kept\nfn item() {}\n",
        "    // the rule. Baked seed\n    //   verdict: kept\n    arm();\n",
        "//! the rule. Baked seed\n//! verdict: kept\nfn item() {}\n",
        "/// the arms are baked seed\n/// verdicts: kept\nfn item() {}\n",
    ] {
        let blocks = baked(source);
        assert_eq!(blocks.len(), 1, "{source}");
        assert_eq!(uncovered_baked_blocks(&blocks).len(), 1, "{source}");
    }
}

#[test]
fn a_marker_in_another_comment_does_not_cover() {
    let source = format!(
        "/// {SEED_TOKEN} acceptance-seed -- a reason\nfn first() {{}}\n\
         /// {BAKED_PHRASE}: kept.\nfn second() {{}}\n"
    );
    assert_eq!(uncovered_baked_blocks(&baked(&source)).len(), 1);
}

#[test]
fn a_token_that_does_not_parse_covers_nothing() {
    // An unparseable seed token is a parse error, never quiet coverage.
    let source = format!("/// {BAKED_PHRASE}: kept.\n/// {SEED_TOKEN} nonsense\nfn item() {{}}\n");
    assert!(baked_blocks_in("test-crate", "area/item.rs", &source).is_err());
}

#[test]
fn a_sweep_finding_no_baked_comment_is_red() {
    let population: Vec<SweptSource> = vec![(
        PROVIDERS.to_string(),
        "plain.rs".to_string(),
        "fn plain() {}\n".to_string(),
    )];
    let why = baked_blocks_over(&population).expect_err("an empty sweep is a failed sweep");
    assert!(why.contains("empty sweep"), "unexpected reason: {why}");
}
