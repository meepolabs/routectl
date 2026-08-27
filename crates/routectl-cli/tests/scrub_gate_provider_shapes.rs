//! Drift detector for the fixture scrub gate's credential-shape coverage.
//!
//! `scripts/scrub-fixture.sh` declares which credential SHAPES it can detect
//! per provider kind, as a sentinel-delimited `PROVIDER_SHAPE_KINDS` table
//! plus a `PROVIDER_SHAPE_EXCLUDED` list carrying a written reason per entry.
//! This test parses that block as text and asserts a THREE-state
//! classification over `routectl_router::CONFIG_PROVIDER_KINDS`: every
//! config-nameable kind is in the table XOR on the exclusion list. An
//! unclassified kind is the failure state -- that is the whole contract, and
//! it mirrors `every_config_kind_is_classified_in_table_or_excluded` in
//! `crates/routectl-cli/src/commands/provider_env.rs`.
//!
//! Two states ("every kind has a shape") would be a rubber stamp: bedrock's
//! `AWS_SECRET_ACCESS_KEY` is 40 prefix-less base64 characters, structurally
//! invisible to anything that is not an entropy matcher, so it is classified
//! by EXCLUSION with the reason recorded beside it.
//!
//! The counts asserted below are guards on the PARSE, never the contract: a
//! count can never distinguish "classified" from "enough rows". The parse
//! reads the declared TABLE and never the combined provider-key regex's
//! alternation -- the shapes land as separate named regexes, so an
//! alternation parser would read the old list, pass, and know nothing about
//! the new ones.
//!
//! Placement: this lives in `routectl-cli` because `CONFIG_PROVIDER_KINDS`
//! lives in `routectl-router` (so `routectl-core`, a leaf crate, cannot see
//! it) and because a router-side test would run under
//! `--no-default-features`, where that vocabulary shrinks to two entries
//! while the reduced-feature CI legs run clippy/check only.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Sentinels bounding the declared block. Renaming either in the shell
/// without updating them here turns the parse into a loud failure rather
/// than a silently empty set.
const BEGIN_SENTINEL: &str = "# --- BEGIN PROVIDER_SHAPE_KINDS ---";
const END_SENTINEL: &str = "# --- END PROVIDER_SHAPE_KINDS ---";

const SHAPE_TABLE_PATH: &str = "scripts/scrub-fixture.sh";

/// The parsed classification block: kinds with a known credential shape
/// (paired with the rule ids covering them) and kinds classified by
/// exclusion.
struct ShapeClassification {
    table: Vec<(String, Vec<String>)>,
    excluded: Vec<String>,
}

impl ShapeClassification {
    fn in_table(&self, kind: &str) -> bool {
        self.table.iter().any(|(table_kind, _)| table_kind == kind)
    }

    fn is_excluded(&self, kind: &str) -> bool {
        self.excluded.iter().any(|excluded| excluded == kind)
    }

    fn rule_ids_for(&self, kind: &str) -> Option<&[String]> {
        self.table
            .iter()
            .find(|(table_kind, _)| table_kind == kind)
            .map(|(_, ids)| ids.as_slice())
    }

    /// Total kinds the block classified either way. The non-vacuity floor
    /// sits on this rather than on table rows alone, because `bedrock` is
    /// classified by exclusion.
    const fn classified_count(&self) -> usize {
        self.table.len() + self.excluded.len()
    }
}

fn shape_table_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/scrub-fixture.sh")
}

/// Extract the array literal opened by `PROVIDER_SHAPE_<name>=(` and closed
/// by a `)` on its own line. Both halves are required: an unclosed literal
/// means the block was reshaped and the parse below cannot be trusted.
fn array_body<'a>(block: &'a str, declaration: &str) -> &'a str {
    block
        .split_once(declaration)
        .unwrap_or_else(|| {
            panic!(
                "{SHAPE_TABLE_PATH} must declare `{declaration}` between the \
                 shape-coverage sentinels; the parse found no such array, so \
                 this test would otherwise pass over an empty set"
            )
        })
        .1
        .split_once("\n)")
        .unwrap_or_else(|| {
            panic!(
                "`{declaration}` in {SHAPE_TABLE_PATH} is not a closed array \
                 literal (no `)` on its own line)"
            )
        })
        .0
}

/// Quoted entries of a shell array body, comment and blank lines skipped.
/// A line that is neither a comment nor a quoted entry is a parse failure,
/// not something to skip past.
fn quoted_entries(body: &str, declaration: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let entry = line
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'));
        let entry = entry.unwrap_or_else(|| {
            panic!(
                "unparseable line {line:?} inside `{declaration}` in \
                 {SHAPE_TABLE_PATH}: entries must be one double-quoted item \
                 per line"
            )
        });
        entries.push(entry.to_owned());
    }
    entries
}

fn parse_shape_classification() -> ShapeClassification {
    let script = shape_table_path().canonicalize().unwrap_or_else(|err| {
        panic!(
            "{SHAPE_TABLE_PATH} must exist: it owns the captured-fixture \
             credential scrub gate ({err})"
        )
    });
    let source = std::fs::read_to_string(&script)
        .unwrap_or_else(|err| panic!("{SHAPE_TABLE_PATH} must be readable ({err})"));

    // Closed-block guard: both sentinels present, in order.
    let block = source
        .split_once(BEGIN_SENTINEL)
        .unwrap_or_else(|| {
            panic!(
                "{SHAPE_TABLE_PATH} must carry the `{BEGIN_SENTINEL}` line -- \
                 without it there is no block to parse and this test would be \
                 vacuous"
            )
        })
        .1
        .split_once(END_SENTINEL)
        .unwrap_or_else(|| {
            panic!(
                "the shape-coverage block in {SHAPE_TABLE_PATH} is not closed \
                 by `{END_SENTINEL}`"
            )
        })
        .0;

    let table_rows = quoted_entries(
        array_body(block, "PROVIDER_SHAPE_KINDS=("),
        "PROVIDER_SHAPE_KINDS",
    );
    let excluded = quoted_entries(
        array_body(block, "PROVIDER_SHAPE_EXCLUDED=("),
        "PROVIDER_SHAPE_EXCLUDED",
    );

    let table = table_rows
        .iter()
        .map(|row| {
            let (kind, ids) = row.split_once('=').unwrap_or_else(|| {
                panic!(
                    "row {row:?} in PROVIDER_SHAPE_KINDS ({SHAPE_TABLE_PATH}) \
                     is not `<kind>=<rule-id>[,<rule-id>...]`"
                )
            });
            let ids: Vec<String> = ids
                .split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect();
            assert!(
                !ids.is_empty(),
                "row {row:?} in PROVIDER_SHAPE_KINDS ({SHAPE_TABLE_PATH}) \
                 names no rule id, so it claims coverage it does not describe",
            );
            (kind.to_owned(), ids)
        })
        .collect();

    ShapeClassification { table, excluded }
}

#[test]
fn every_config_kind_is_classified_with_a_shape_or_an_exclusion() {
    // THE contract. A config-nameable provider kind that is neither in the
    // shape table nor explicitly excluded means the gate cannot detect that
    // kind's credential and nobody decided that was acceptable.
    let classification = parse_shape_classification();
    let config_kinds: BTreeSet<&str> = routectl_router::CONFIG_PROVIDER_KINDS
        .iter()
        .copied()
        .collect();

    for kind in config_kinds {
        let in_table = classification.in_table(kind);
        let excluded = classification.is_excluded(kind);
        assert!(
            in_table ^ excluded,
            "config provider kind {kind:?} must be classified in exactly one \
             of the PROVIDER_SHAPE_KINDS table or PROVIDER_SHAPE_EXCLUDED in \
             {SHAPE_TABLE_PATH} (in_table={in_table}, excluded={excluded}). \
             Add a row naming the rule ids that detect its credential, or add \
             it to PROVIDER_SHAPE_EXCLUDED with the reason no prefix shape \
             exists.",
        );
    }
}

#[test]
fn config_provider_vocabulary_is_the_full_provider_set_here() {
    // Non-vacuity guard on the VOCABULARY, not the contract. This crate
    // pulls routectl-router with default features and its Cargo.toml states
    // that a lean provider subset is not a supported CLI build target, so
    // CONFIG_PROVIDER_KINDS is always the full set in this test binary. A
    // future decision to support a lean CLI turns this RED instead of
    // silently narrowing the XOR loop above to a subset of the kinds.
    assert!(
        routectl_router::CONFIG_PROVIDER_KINDS.len() >= 5,
        "CONFIG_PROVIDER_KINDS holds only {} kinds in this build; the CLI is \
         supposed to compile every provider, so the classification check \
         above would silently cover a subset of the real vocabulary",
        routectl_router::CONFIG_PROVIDER_KINDS.len()
    );
}

#[test]
fn shape_classification_block_parsed_as_a_closed_populated_literal() {
    // Non-vacuity guard on the PARSE. A parse yielding nothing satisfies the
    // XOR loop only because it never classifies anything; these floors make
    // that state loud. They are deliberately NOT the detector: a count can
    // never tell "classified" from "enough rows".
    let classification = parse_shape_classification();

    assert!(
        classification.table.len() >= 4,
        "parsed only {} rows out of PROVIDER_SHAPE_KINDS in \
         {SHAPE_TABLE_PATH}; the parse broke",
        classification.table.len()
    );
    assert!(
        classification.classified_count() >= 5,
        "parsed only {} total classifications (table rows + exclusions) out \
         of the shape-coverage block in {SHAPE_TABLE_PATH}; the parse broke",
        classification.classified_count()
    );
}

#[test]
fn extraction_recovers_the_anthropic_shape_rule_ids() {
    // Positive control for the extraction: without it a parse that yielded
    // plausible-looking garbage would satisfy the classification loop for the
    // wrong reason.
    let classification = parse_shape_classification();
    let ids = classification
        .rule_ids_for("anthropic-api")
        .expect("PROVIDER_SHAPE_KINDS demonstrably carries an `anthropic-api` row");

    assert!(
        ids.iter().any(|id| id.contains("sk-ant-api03")),
        "extraction produced rule ids {ids:?} for `anthropic-api`, which does \
         not include the `sk-ant-api03` shape the gate demonstrably detects"
    );
}

#[test]
fn every_classified_kind_is_config_nameable() {
    // Neither list may name a kind no `[providers.X]` block can declare: such
    // a row is coverage of nothing, and it would mask the real kind it was
    // mistyped from.
    let classification = parse_shape_classification();

    for (kind, _) in &classification.table {
        assert!(
            routectl_router::is_config_provider_kind(kind),
            "PROVIDER_SHAPE_KINDS in {SHAPE_TABLE_PATH} names {kind:?}, which \
             is not a config-nameable provider kind",
        );
    }
    for kind in &classification.excluded {
        assert!(
            routectl_router::is_config_provider_kind(kind),
            "PROVIDER_SHAPE_EXCLUDED in {SHAPE_TABLE_PATH} names {kind:?}, \
             which is not a config-nameable provider kind",
        );
    }
}
