//! Weld between the wire-pattern vocabulary and the predicates that decide it,
//! in both directions and in both languages.
//!
//! Two independent welds, one file because they share the same two inputs:
//!
//! 1. COVERAGE. `scripts/drivers/lib/validate_case.py` owns the closed pattern
//!    vocabulary and `scripts/drivers/lib/verify_pattern.py` owns the predicate
//!    table that decides each token. Both are parsed here as TEXT out of
//!    sentinel-delimited blocks, and the covered set must equal the vocabulary
//!    minus an explicit deferred list. Exact-set rather than a count: a count
//!    can never distinguish "covered" from "enough rows", and a pattern added
//!    to the vocabulary without a predicate would otherwise promote fixtures
//!    nothing verifies.
//!
//! 2. TWO-LANGUAGE SEMANTICS. The three STRUCTURAL predicates exist twice --
//!    ported into `verify_pattern.py` and as the reference logic below -- and a
//!    shape means whatever each implementation independently says it means.
//!    `scripts/drivers/lib/wire_pattern_classification.tsv` records the verdict
//!    for each structural line; `scripts/drivers.test.sh` runs every record
//!    through the Python predicates and this file runs the same records through
//!    the reference logic, so a divergence between the two is a red test on one
//!    side or the other rather than a silent difference of opinion.
//!
//! The two body-census patterns (`tool-use-multiturn`, `large-context`) are out
//! of scope for weld 2: they read the captured ingress body, have no Rust
//! counterpart to drift from, and a structural line carries nothing that
//! decides them.
//!
//! Every parse fails LOUDLY. An absent or unparseable source produces an error,
//! never an empty set that satisfies every assertion below by classifying
//! nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const VOCABULARY_PATH: &str = "scripts/drivers/lib/validate_case.py";
const PREDICATE_PATH: &str = "scripts/drivers/lib/verify_pattern.py";
const CLASSIFICATION_PATH: &str = "scripts/drivers/lib/wire_pattern_classification.tsv";

/// Sentinels bounding each parsed declaration. Renaming one in Python without
/// updating it here turns the parse into a loud failure rather than a silently
/// empty set.
const VOCABULARY_SENTINELS: (&str, &str) = (
    "# --- BEGIN WIRE_PATTERNS ---",
    "# --- END WIRE_PATTERNS ---",
);
const PREDICATE_SENTINELS: (&str, &str) =
    ("# --- BEGIN PREDICATES ---", "# --- END PREDICATES ---");
const DEFERRED_SENTINELS: (&str, &str) = (
    "# --- BEGIN DEFERRED_PATTERNS ---",
    "# --- END DEFERRED_PATTERNS ---",
);

/// The one token the predicate table is allowed to omit: it has no case and no
/// fixture can claim it yet. Pinned as the exact deferred set, so extending the
/// vocabulary with it (or deferring anything else) is a review moment.
const EXPECTED_DEFERRED: &[&str] = &["mcp-tools"];

/// The patterns a structural summary line alone decides.
const STRUCTURAL_PATTERNS: &[&str] = &["baseline", "thinking", "cache-breakpoints"];

/// Floor on the parsed classification record count. A guard on the PARSE, never
/// the contract -- an empty TSV parse would satisfy every per-record assertion.
const MIN_CLASSIFICATION_RECORDS: usize = 8;

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// Read a declared source, or report why it cannot be read. Absence is an
/// error: these files ARE the contract, so a checkout missing one has nothing
/// to weld.
fn read_source(relative: &str) -> Result<String, String> {
    let path = repo_path(relative);
    std::fs::read_to_string(&path).map_err(|err| format!("{relative} must be readable ({err})"))
}

/// The text between a matched sentinel pair.
fn sentinel_block<'a>(
    source: &'a str,
    label: &str,
    sentinels: (&str, &str),
) -> Result<&'a str, String> {
    let (begin, end) = sentinels;
    let after = source
        .split_once(begin)
        .ok_or_else(|| format!("no `{begin}` line bounding {label}; there is no block to parse"))?
        .1;
    Ok(after
        .split_once(end)
        .ok_or_else(|| format!("the {label} block is not closed by `{end}`"))?
        .0)
}

/// Tokens declared one double-quoted entry per line inside `block`. A line
/// carrying a quote in any other shape is a parse failure rather than something
/// to skip past; a line with no quote at all is Python punctuation (`{`, `)`).
fn quoted_tokens(block: &str, label: &str) -> Result<BTreeSet<String>, String> {
    let mut tokens = BTreeSet::new();
    for line in block.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('"') {
            continue;
        }
        let entry = line
            .strip_prefix('"')
            .and_then(|rest| rest.split_once('"'))
            .ok_or_else(|| {
                format!("unparseable line {line:?} inside {label}: entries must open with a double-quoted token")
            })?
            .0;
        if entry.is_empty() {
            return Err(format!("empty token declared inside {label}"));
        }
        if !tokens.insert(entry.to_owned()) {
            return Err(format!("token {entry:?} declared twice inside {label}"));
        }
    }
    if tokens.is_empty() {
        return Err(format!(
            "{label} declared no tokens; an empty parse is a failed parse, not a safe answer"
        ));
    }
    Ok(tokens)
}

fn parse_vocabulary(source: &str) -> Result<BTreeSet<String>, String> {
    let block = sentinel_block(source, "WIRE_PATTERNS", VOCABULARY_SENTINELS)?;
    if !block.contains("WIRE_PATTERNS") {
        return Err("the WIRE_PATTERNS block declares no such name".to_string());
    }
    quoted_tokens(block, "WIRE_PATTERNS")
}

fn parse_deferred(source: &str) -> Result<BTreeSet<String>, String> {
    let block = sentinel_block(source, "DEFERRED_PATTERNS", DEFERRED_SENTINELS)?;
    quoted_tokens(block, "DEFERRED_PATTERNS")
}

fn parse_covered(source: &str) -> Result<BTreeSet<String>, String> {
    let block = sentinel_block(source, "PREDICATES", PREDICATE_SENTINELS)?;
    quoted_tokens(block, "PREDICATES")
}

fn expect<T>(parsed: Result<T, String>) -> T {
    parsed.unwrap_or_else(|why| panic!("the wire-pattern weld cannot be evaluated: {why}"))
}

/// THE coverage contract, as a function so the paired controls below can prove
/// it fires. The covered set must be exactly the vocabulary minus whatever the
/// deferred list holds back.
fn coverage_gap(
    vocabulary: &BTreeSet<String>,
    covered: &BTreeSet<String>,
    deferred: &BTreeSet<String>,
) -> Result<(), String> {
    let expected: BTreeSet<String> = vocabulary.difference(deferred).cloned().collect();
    if *covered == expected {
        return Ok(());
    }
    let missing: Vec<&String> = expected.difference(covered).collect();
    let unexpected: Vec<&String> = covered.difference(&expected).collect();
    Err(format!(
        "the predicate table in {PREDICATE_PATH} does not match the vocabulary in \
         {VOCABULARY_PATH}: no predicate for {missing:?}, predicate for the \
         unknown token(s) {unexpected:?}"
    ))
}

#[test]
fn every_vocabulary_token_has_a_predicate_or_is_explicitly_deferred() {
    let vocabulary = expect(parse_vocabulary(&expect(read_source(VOCABULARY_PATH))));
    let predicate_source = expect(read_source(PREDICATE_PATH));
    let covered = expect(parse_covered(&predicate_source));
    let deferred = expect(parse_deferred(&predicate_source));

    if let Err(why) = coverage_gap(&vocabulary, &covered, &deferred) {
        panic!(
            "{why}. Add the predicate, or add the token to DEFERRED_PATTERNS with \
             the reason no fixture can claim it yet -- a silently missing \
             predicate promotes fixtures nothing verifies."
        );
    }
}

#[test]
fn the_deferred_list_holds_exactly_the_one_reviewed_token() {
    // The deferred list is the only place "no predicate on purpose" is
    // recorded, so its contents are pinned rather than merely consulted:
    // supplying a predicate for `mcp-tools`, or deferring a second token, both
    // turn this red instead of widening the omission unreviewed.
    let deferred = expect(parse_deferred(&expect(read_source(PREDICATE_PATH))));
    let expected: BTreeSet<String> = EXPECTED_DEFERRED.iter().map(|t| (*t).to_string()).collect();

    assert_eq!(
        deferred, expected,
        "DEFERRED_PATTERNS in {PREDICATE_PATH} drifted from the reviewed set. \
         Each entry is a pattern token no predicate decides; confirm the change \
         is intended, then update EXPECTED_DEFERRED here."
    );
}

#[test]
fn no_token_is_both_covered_and_deferred() {
    // A token in both lists reads as covered from one file and as deliberately
    // uncovered from the other, and the coverage contract above would credit
    // the deferral.
    let predicate_source = expect(read_source(PREDICATE_PATH));
    let covered = expect(parse_covered(&predicate_source));
    let deferred = expect(parse_deferred(&predicate_source));
    let both: Vec<&String> = covered.intersection(&deferred).collect();

    assert!(
        both.is_empty(),
        "{PREDICATE_PATH} both defers and implements {both:?}"
    );
}

// ---------------------------------------------------------------------------
// Controls on the coverage weld: each proves the exact-set assertion can fire.
// ---------------------------------------------------------------------------

fn token_set(tokens: &[&str]) -> BTreeSet<String> {
    tokens.iter().map(|t| (*t).to_string()).collect()
}

#[test]
fn coverage_check_rejects_a_shortened_predicate_table() {
    let vocabulary = expect(parse_vocabulary(&expect(read_source(VOCABULARY_PATH))));
    let deferred = expect(parse_deferred(&expect(read_source(PREDICATE_PATH))));
    let mut shortened: BTreeSet<String> = vocabulary.difference(&deferred).cloned().collect();
    let dropped = shortened
        .iter()
        .next()
        .expect("the vocabulary is non-empty")
        .clone();
    shortened.remove(&dropped);

    let why = coverage_gap(&vocabulary, &shortened, &deferred)
        .expect_err("a table missing a vocabulary token is not full coverage");
    assert!(why.contains(&dropped), "unexpected reason: {why}");
}

#[test]
fn coverage_check_rejects_a_predicate_for_an_unknown_token() {
    let vocabulary = expect(parse_vocabulary(&expect(read_source(VOCABULARY_PATH))));
    let deferred = expect(parse_deferred(&expect(read_source(PREDICATE_PATH))));
    let mut over_long: BTreeSet<String> = vocabulary.difference(&deferred).cloned().collect();
    over_long.insert("baesline".to_string());

    let why = coverage_gap(&vocabulary, &over_long, &deferred)
        .expect_err("a predicate for a token outside the closed vocabulary is a mismatch");
    assert!(why.contains("baesline"), "unexpected reason: {why}");
}

#[test]
fn coverage_check_accepts_the_deferred_token_once_the_vocabulary_names_it() {
    // Forward-looking control: the deferral is what a token in the vocabulary
    // with no predicate is allowed to rely on, so the contract must credit it
    // rather than pass only because `mcp-tools` is absent upstream today.
    let vocabulary = token_set(&["baseline", "mcp-tools"]);
    let covered = token_set(&["baseline"]);
    let deferred = token_set(&["mcp-tools"]);

    assert_eq!(coverage_gap(&vocabulary, &covered, &deferred), Ok(()));
}

// ---------------------------------------------------------------------------
// Fail-closed guards on the parses themselves.
// ---------------------------------------------------------------------------

#[test]
fn an_absent_vocabulary_source_is_an_error_rather_than_an_empty_set() {
    let why = read_source("scripts/drivers/lib/validate_case.py.absent")
        .expect_err("a source that does not exist cannot be read");
    assert!(why.contains("readable"), "unexpected reason: {why}");
}

#[test]
fn a_vocabulary_source_carrying_no_declaration_is_a_failed_parse() {
    // The lesson this project already paid for: an empty parse result is a
    // FAILED parse, not a safe answer. Every token trivially "has a predicate"
    // when the vocabulary is empty.
    let stub = "#!/usr/bin/env python3\n\"\"\"A module with no vocabulary.\"\"\"\n";
    let why = parse_vocabulary(stub).expect_err("a source with no sentinels has no vocabulary");
    assert!(
        why.contains("BEGIN WIRE_PATTERNS"),
        "unexpected reason: {why}"
    );
}

#[test]
fn an_unclosed_vocabulary_block_is_a_failed_parse() {
    let stub = format!("{}\n    \"baseline\",\n", VOCABULARY_SENTINELS.0);
    let why = parse_vocabulary(&stub).expect_err("an unclosed block means the declaration moved");
    assert!(why.contains("not closed"), "unexpected reason: {why}");
}

#[test]
fn an_empty_vocabulary_block_is_a_failed_parse() {
    let stub = format!(
        "{}\nWIRE_PATTERNS = frozenset(\n    {{\n    }}\n)\n{}\n",
        VOCABULARY_SENTINELS.0, VOCABULARY_SENTINELS.1
    );
    let why = parse_vocabulary(&stub).expect_err("a vocabulary of zero tokens asserts nothing");
    assert!(why.contains("no tokens"), "unexpected reason: {why}");
}

#[test]
fn an_unquoted_entry_inside_a_block_is_a_failed_parse() {
    let stub = format!(
        "{}\nWIRE_PATTERNS = frozenset(\n    {{\n        baseline\",\n    }}\n)\n{}\n",
        VOCABULARY_SENTINELS.0, VOCABULARY_SENTINELS.1
    );
    let why =
        parse_vocabulary(&stub).expect_err("an entry not opening with a quote is unparseable");
    assert!(why.contains("unparseable"), "unexpected reason: {why}");
}

#[test]
fn the_real_vocabulary_parse_recovers_a_known_token() {
    // Positive control for the extraction: without it a parse yielding
    // plausible-looking garbage would satisfy the coverage contract for the
    // wrong reason.
    let vocabulary = expect(parse_vocabulary(&expect(read_source(VOCABULARY_PATH))));
    assert!(
        vocabulary.contains("baseline"),
        "the parsed vocabulary {vocabulary:?} omits `baseline`, which \
         {VOCABULARY_PATH} demonstrably declares"
    );
}

// ---------------------------------------------------------------------------
// Reference logic: the three structural predicates, over one summary line.
// ---------------------------------------------------------------------------

/// Value of the `key=value` token named `key`, or `None` when the line carries
/// no such token.
///
/// Token-exact by construction: a substring search for `thinking_shape=` also
/// matches `output_thinking_shape=...`, which would let an unrelated field
/// satisfy a clause about a missing one.
fn token_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .filter_map(|token| token.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

/// A count token, parsed. An ABSENT count is not a zero: a summary missing the
/// token was not emitted by the shape this corpus pins, and guessing a value
/// classifies a shape nobody observed.
fn count_token(line: &str, key: &str) -> Result<i64, String> {
    let raw = token_value(line, key).ok_or_else(|| format!("{key} token absent"))?;
    raw.parse()
        .map_err(|_| format!("{key}={raw} is not a count"))
}

/// Both spellings of "thinking off". The real client sends
/// `thinking: {"type": "disabled"}`, which the summary renders as the explicit
/// token `thinking_shape=disabled` rather than as an absent field, so a
/// predicate that only knew the absent form would read a disabled block as an
/// active one.
fn is_inactive_thinking(shape: &str) -> bool {
    shape.is_empty() || shape == "disabled"
}

fn line_is_baseline(line: &str) -> Result<(), String> {
    let tools_len = count_token(line, "tools_len")?;
    if tools_len != 0 {
        return Err(format!("tools_len={tools_len}, want 0"));
    }
    if let Some(shape) = token_value(line, "thinking_shape")
        && !is_inactive_thinking(shape)
    {
        return Err(format!("thinking_shape={shape} is active"));
    }
    let cache_control_count = count_token(line, "cache_control_count")?;
    if cache_control_count != 0 {
        return Err(format!("cache_control_count={cache_control_count}, want 0"));
    }
    Ok(())
}

fn line_is_thinking(line: &str) -> Result<(), String> {
    let shape = token_value(line, "thinking_shape").ok_or("thinking_shape token absent")?;
    if is_inactive_thinking(shape) {
        return Err(format!(
            "thinking_shape={shape:?} is not an active thinking block"
        ));
    }
    Ok(())
}

fn line_is_cache_breakpoints(line: &str) -> Result<(), String> {
    let count = count_token(line, "cache_control_count")?;
    if count < 1 {
        return Err(format!("cache_control_count={count}, want at least 1"));
    }
    Ok(())
}

fn classify(line: &str, pattern: &str) -> Result<(), String> {
    match pattern {
        "baseline" => line_is_baseline(line),
        "thinking" => line_is_thinking(line),
        "cache-breakpoints" => line_is_cache_breakpoints(line),
        other => panic!(
            "{other:?} is not decided by a structural line; the classification \
             set is scoped to {STRUCTURAL_PATTERNS:?}"
        ),
    }
}

// ---------------------------------------------------------------------------
// The two-language semantics weld, over the shared classification set.
// ---------------------------------------------------------------------------

/// One classification record: a structural line plus the patterns it does and
/// does not satisfy.
#[derive(Debug)]
struct Record {
    satisfies: Vec<String>,
    denies: Vec<String>,
    line: String,
}

/// A comma-separated pattern field, with `-` spelling the empty list. Every
/// named pattern must be one a structural line can decide: the set is scoped to
/// the three structural predicates, so a body-census token here is a record
/// asserting something no reader of this file can evaluate.
fn pattern_field(field: &str, label: &str) -> Result<Vec<String>, String> {
    if field == "-" {
        return Ok(Vec::new());
    }
    let mut patterns = Vec::new();
    for pattern in field.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if !STRUCTURAL_PATTERNS.contains(&pattern) {
            return Err(format!(
                "the {label} field names {pattern:?}, which no structural line \
                 decides; the set is scoped to {STRUCTURAL_PATTERNS:?}"
            ));
        }
        patterns.push(pattern.to_owned());
    }
    Ok(patterns)
}

fn parse_records(source: &str) -> Result<Vec<Record>, String> {
    let mut records = Vec::new();
    for raw in source.lines() {
        if raw.trim().is_empty() || raw.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = raw.split('\t').collect();
        let [satisfies, denies, line] = fields.as_slice() else {
            return Err(format!(
                "record {raw:?} does not carry exactly three TAB-separated fields"
            ));
        };
        let satisfies = pattern_field(satisfies, "satisfies")?;
        let denies = pattern_field(denies, "denies")?;
        if satisfies.is_empty() && denies.is_empty() {
            return Err(format!(
                "record {line:?} names no pattern, so it asserts nothing"
            ));
        }
        records.push(Record {
            satisfies,
            denies,
            line: (*line).to_owned(),
        });
    }
    if records.is_empty() {
        return Err(format!(
            "{CLASSIFICATION_PATH} yielded no records; an empty parse cannot \
             pass this weld"
        ));
    }
    Ok(records)
}

#[test]
fn the_reference_logic_agrees_with_every_recorded_classification() {
    // THE two-language contract. `scripts/drivers.test.sh` runs these same
    // records through the Python predicates, so agreement with the recorded
    // verdict on both sides is agreement with each other -- and a divergence
    // is red on whichever side moved.
    let records = expect(parse_records(&expect(read_source(CLASSIFICATION_PATH))));

    for record in &records {
        for pattern in &record.satisfies {
            if let Err(why) = classify(&record.line, pattern) {
                panic!(
                    "the record claims {pattern} but the reference logic refuses \
                     it ({why}); the Python predicate accepts it, so the two \
                     implementations disagree. Line: {}",
                    record.line
                );
            }
        }
        for pattern in &record.denies {
            assert!(
                classify(&record.line, pattern).is_err(),
                "the record denies {pattern} but the reference logic accepts \
                 it; the Python predicate refuses it, so the two \
                 implementations disagree. Line: {}",
                record.line
            );
        }
    }
}

#[test]
fn the_classification_set_parsed_as_a_populated_record_set() {
    // Non-vacuity guard on the PARSE, not the contract: the loop above is
    // satisfied by zero records. Deliberately a floor and not the detector.
    let records = expect(parse_records(&expect(read_source(CLASSIFICATION_PATH))));

    assert!(
        records.len() >= MIN_CLASSIFICATION_RECORDS,
        "parsed only {} records out of {CLASSIFICATION_PATH}; the parse broke",
        records.len()
    );
    assert!(
        records
            .iter()
            .any(|record| record.satisfies.iter().any(|p| p == "baseline")),
        "no parsed record claims `baseline`, which {CLASSIFICATION_PATH} \
         demonstrably records; the parse recovered the wrong fields"
    );
    assert!(
        records
            .iter()
            .any(|record| record.denies.iter().any(|p| p == "baseline")),
        "no parsed record denies `baseline`; the set must exercise both \
         directions or the reference logic is only ever asked to say yes"
    );
}

#[test]
fn an_absent_classification_set_is_an_error_rather_than_an_empty_run() {
    let why = read_source("scripts/drivers/lib/wire_pattern_classification.tsv.absent")
        .expect_err("a classification set that does not exist cannot be read");
    assert!(why.contains("readable"), "unexpected reason: {why}");
}

#[test]
fn a_classification_set_carrying_only_comments_is_a_failed_parse() {
    let why = parse_records("# a header and nothing else\n\n")
        .expect_err("a set with no records asserts nothing");
    assert!(why.contains("no records"), "unexpected reason: {why}");
}

#[test]
fn a_record_asserting_nothing_is_a_failed_parse() {
    let line = "structural summary direction=\"ingress\" tools_len=0 cache_control_count=0";
    let why = parse_records(&format!("-\t-\t{line}\n"))
        .expect_err("a record naming no pattern in either field asserts nothing");
    assert!(why.contains("asserts nothing"), "unexpected reason: {why}");
}

#[test]
fn a_record_naming_a_body_census_pattern_is_a_failed_parse() {
    let line = "structural summary direction=\"ingress\" tools_len=0 cache_control_count=0";
    let why = parse_records(&format!("large-context\t-\t{line}\n"))
        .expect_err("a structural line cannot decide a body-census pattern");
    assert!(why.contains("large-context"), "unexpected reason: {why}");
}

#[test]
fn a_record_with_the_wrong_field_count_is_a_failed_parse() {
    let why = parse_records("baseline\tthinking\n")
        .expect_err("a two-field record carries no structural line");
    assert!(
        why.contains("three TAB-separated"),
        "unexpected reason: {why}"
    );
}

// ---------------------------------------------------------------------------
// Controls on the reference logic: each failing line flips exactly ONE clause.
// ---------------------------------------------------------------------------

/// Hand-built ingress summary in the real field order, with the three
/// predicate fields supplied by the caller.
fn structural_line(tools_len: &str, thinking: Option<&str>, cache_control_count: &str) -> String {
    let thinking_token = match thinking {
        Some(shape) => format!("thinking_shape={shape} "),
        None => String::new(),
    };
    format!(
        "structural summary direction=\"ingress\" kind=\"ingress\" id=\"anthropic\" \
         model=claude-sonnet-4-5 max_tokens=32000 {thinking_token}output_config_effort= \
         tool_choice_shape= cache_control_count={cache_control_count} messages_len=1 \
         tools_len={tools_len} anthropic_beta= provider_extras_keys= stream=true"
    )
}

#[test]
fn baseline_accepts_both_spellings_of_thinking_off() {
    assert_eq!(
        line_is_baseline(&structural_line("0", Some("disabled"), "0")),
        Ok(())
    );

    let absent = structural_line("0", None, "0");
    assert!(
        !absent.contains("thinking_shape"),
        "control must omit the token"
    );
    assert_eq!(line_is_baseline(&absent), Ok(()));
}

#[test]
fn baseline_rejects_a_line_carrying_tools() {
    let why = line_is_baseline(&structural_line("16", Some("disabled"), "0"))
        .expect_err("tools_len=16 is not baseline");
    assert!(why.contains("tools_len"), "unexpected reason: {why}");
}

#[test]
fn baseline_rejects_an_active_thinking_shape() {
    let why = line_is_baseline(&structural_line("0", Some("enabled:31999"), "0"))
        .expect_err("thinking_shape=enabled:31999 is not baseline");
    assert!(why.contains("thinking_shape"), "unexpected reason: {why}");
}

#[test]
fn baseline_rejects_cache_breakpoints() {
    let why = line_is_baseline(&structural_line("0", Some("disabled"), "3"))
        .expect_err("cache_control_count=3 is not baseline");
    assert!(
        why.contains("cache_control_count"),
        "unexpected reason: {why}"
    );
}

#[test]
fn an_absent_count_token_is_not_read_as_a_zero() {
    let line = structural_line("0", Some("disabled"), "0").replace("cache_control_count=0 ", "");
    let why = line_is_baseline(&line)
        .expect_err("a summary missing the count was not emitted by the pinned shape");
    assert!(why.contains("absent"), "unexpected reason: {why}");
}

#[test]
fn a_non_numeric_count_token_is_not_read_as_a_zero() {
    let why = line_is_baseline(&structural_line("0", Some("disabled"), "many"))
        .expect_err("a count that is not a number classifies nothing");
    assert!(why.contains("not a count"), "unexpected reason: {why}");
}

#[test]
fn thinking_requires_an_active_shape_token() {
    assert_eq!(
        line_is_thinking(&structural_line("0", Some("enabled:31999"), "0")),
        Ok(())
    );
    assert!(line_is_thinking(&structural_line("0", Some("disabled"), "0")).is_err());
    assert!(line_is_thinking(&structural_line("0", None, "0")).is_err());
}

#[test]
fn cache_breakpoints_requires_at_least_one() {
    assert_eq!(
        line_is_cache_breakpoints(&structural_line("0", Some("disabled"), "1")),
        Ok(())
    );
    let why = line_is_cache_breakpoints(&structural_line("0", Some("disabled"), "0"))
        .expect_err("cache_control_count=0 is not a breakpoint");
    assert!(why.contains("at least 1"), "unexpected reason: {why}");
}

#[test]
fn token_lookup_is_exact_rather_than_substring() {
    let line = "structural summary output_thinking_shape=enabled:31999 tools_len=0";
    assert_eq!(token_value(line, "thinking_shape"), None);
}
