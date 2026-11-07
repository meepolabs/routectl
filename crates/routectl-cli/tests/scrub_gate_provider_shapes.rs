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
//! and the full set this weld needs is no longer compiled in.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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

// ---------------------------------------------------------------------------
// Repo gitleaks config scope
// ---------------------------------------------------------------------------
//
// Same subject as the block above, from the commit-time side. The local
// pre-commit hook loads the root `.gitleaks.toml` (default rules plus one
// extra rule, with one exact public value exempted) and honors a root
// `.gitleaksignore`. Anything else in either file could disable a default
// rule, skip paths, or exempt more than that one value locally, so the
// whole shape is pinned here.

const ANTIGRAVITY_SOURCE_PATH: &str = "crates/routectl-auth/src/oauth/providers/antigravity.rs";
const GITLEAKS_ROOT_KEYS: [&str; 3] = ["allowlist", "extend", "rules"];
const GLOBAL_ALLOWLIST_KEYS: [&str; 3] = ["description", "regexTarget", "regexes"];

fn repo_root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(
        root.join(".pre-commit-config.yaml").is_file(),
        "control: {} must be the repo root that holds the commit gate config",
        root.display()
    );
    root
}

fn load_repo_gitleaks_config() -> toml::Table {
    let path = repo_root().join(".gitleaks.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("control: read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn antigravity_client_secret() -> String {
    let path = repo_root().join(ANTIGRAVITY_SOURCE_PATH);
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let values: Vec<&str> = source
        .lines()
        .filter_map(|line| line.strip_prefix("const CLIENT_SECRET: &str = \""))
        .filter_map(|rest| rest.strip_suffix("\";"))
        .collect();
    assert_eq!(
        values.len(),
        1,
        "{ANTIGRAVITY_SOURCE_PATH} must declare exactly one CLIENT_SECRET literal: {values:?}"
    );
    assert!(
        values[0].starts_with("GOCSPX-"),
        "control: the Antigravity client secret has the rule's prefix: {:?}",
        values[0]
    );
    values[0].to_owned()
}

fn table_keys(table: &toml::Table) -> BTreeSet<&str> {
    table.keys().map(String::as_str).collect()
}

#[test]
fn the_repo_gitleaks_config_extends_the_defaults_with_only_the_gocspx_rule() {
    let config = load_repo_gitleaks_config();
    assert_eq!(
        table_keys(&config),
        BTreeSet::from(GITLEAKS_ROOT_KEYS),
        "the root .gitleaks.toml may only carry [extend], [allowlist], and [[rules]]",
    );

    let extend: toml::Table = toml::from_str("useDefault = true").expect("literal");
    assert_eq!(
        config.get("extend").and_then(toml::Value::as_table),
        Some(&extend),
        "[extend] must inherit every default rule and nothing else",
    );

    let gocspx_rule: toml::Table = toml::from_str(
        r#"
        id = "google-oauth-client-secret"
        description = "Google OAuth client secret (GOCSPX- prefix)"
        regex = 'GOCSPX-[A-Za-z0-9_-]{20,}'
        keywords = ["GOCSPX-"]
        "#,
    )
    .expect("literal");
    assert_eq!(
        config.get("rules"),
        Some(&toml::Value::Array(vec![toml::Value::Table(gocspx_rule)])),
        "[[rules]] must be exactly the one GOCSPX- rule, with no allowlist or \
         override of a default rule id",
    );
}

#[test]
fn the_repo_gitleaks_config_exempts_only_the_antigravity_client_secret() {
    let config = load_repo_gitleaks_config();
    let allowlist = config
        .get("allowlist")
        .and_then(toml::Value::as_table)
        .expect("the global [allowlist] holds the one exempted value");
    assert_eq!(
        table_keys(allowlist),
        BTreeSet::from(GLOBAL_ALLOWLIST_KEYS),
        "any other [allowlist] key would exempt paths, commits, or words from the scan",
    );
    assert_eq!(
        allowlist.get("regexTarget").and_then(toml::Value::as_str),
        Some("secret"),
        "the allowlist must match the extracted secret, not the line or match",
    );

    let exact_value = format!("^{}$", antigravity_client_secret());
    assert_eq!(
        allowlist.get("regexes"),
        Some(&toml::Value::Array(vec![toml::Value::String(exact_value)])),
        "the allowlist must exempt only the exact Antigravity client secret",
    );
}

/// True when anything sits at `path`, a dangling symlink included:
/// `Path::exists` follows the link and reports false for one, yet gitleaks
/// still opens the name and a later target would take effect.
fn path_is_present(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

#[test]
fn the_repo_root_carries_no_gitleaksignore() {
    let ignore = repo_root().join(".gitleaksignore");
    assert!(
        !path_is_present(&ignore),
        "{} must not exist: the local hook would skip every fingerprint it lists",
        ignore.display()
    );
}

#[cfg(unix)]
#[test]
fn path_presence_counts_a_dangling_symlink_and_rejects_an_absent_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dangling = dir.path().join("dangling");
    std::os::unix::fs::symlink(dir.path().join("no-such-target"), &dangling)
        .expect("create dangling symlink");
    let absent = dir.path().join("absent");

    assert!(
        !dangling.exists(),
        "control: Path::exists must report false for the dangling symlink"
    );
    assert!(path_is_present(&dangling), "a dangling symlink is present");
    assert!(!path_is_present(&absent), "an absent path is not present");
}

// ---------------------------------------------------------------------------
// The gitleaks pre-commit hook runs with its upstream defaults
// ---------------------------------------------------------------------------
//
// An `args` or `entry` override on the upstream hook can narrow what it scans
// (a different subcommand, a path, `--config`, `--no-git`) without touching
// `.gitleaks.toml`. No YAML parser is in the dependency tree, so the reader
// below accepts exactly one block layout and rejects everything else: an
// unrecognized layout is a failure, never a pass.

const GITLEAKS_REPO_LINE: &str = "  - repo: https://github.com/gitleaks/gitleaks";
const GITLEAKS_REPO_URL: &str = "github.com/gitleaks/gitleaks";

fn is_yaml_comment_or_blank(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#')
}

const COMMIT_SHA_LEN: usize = 40;

fn is_commit_sha(token: &str) -> bool {
    token.len() == COMMIT_SHA_LEN
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A rev must be a full lowercase commit sha, which upstream cannot move the
/// way it can a tag, optionally followed by a `# ...` comment naming the
/// release.
fn is_valid_rev_line(line: &str) -> bool {
    let Some(value) = line.strip_prefix("    rev: ") else {
        return false;
    };
    let (sha, rest) = value.split_at(value.find(char::is_whitespace).unwrap_or(value.len()));
    is_commit_sha(sha) && (rest.is_empty() || rest.trim_start().starts_with('#'))
}

/// Checks that the config holds exactly one gitleaks repo block, laid out as
/// `rev:`, `hooks:`, and a single `- id: gitleaks` with no other key.
fn check_gitleaks_hook_block(config: &str) -> Result<(), String> {
    let lines: Vec<&str> = config.lines().collect();
    let mentions: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !is_yaml_comment_or_blank(line) && line.contains(GITLEAKS_REPO_URL))
        .map(|(index, _)| index)
        .collect();
    let start = match mentions.as_slice() {
        [] => return Err("no gitleaks repo block".to_owned()),
        [index] if lines[*index] == GITLEAKS_REPO_LINE => *index,
        [index] => {
            return Err(format!(
                "unrecognized gitleaks repo line: {:?}",
                lines[*index]
            ));
        }
        _ => return Err(format!("{} gitleaks repo mentions", mentions.len())),
    };

    let body: Vec<&str> = lines[start + 1..]
        .iter()
        .copied()
        .take_while(|line| {
            is_yaml_comment_or_blank(line) || line.len() - line.trim_start().len() >= 4
        })
        .filter(|line| !is_yaml_comment_or_blank(line))
        .collect();
    match body.as_slice() {
        [rev, "    hooks:", "      - id: gitleaks"] if is_valid_rev_line(rev) => Ok(()),
        _ => Err(format!("unrecognized gitleaks repo block layout: {body:?}")),
    }
}

fn load_pre_commit_config() -> String {
    let path = repo_root().join(".pre-commit-config.yaml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("control: read {}: {e}", path.display()))
}

#[test]
fn the_gitleaks_hook_runs_with_upstream_defaults() {
    let config = load_pre_commit_config();
    if let Err(reason) = check_gitleaks_hook_block(&config) {
        panic!(
            "the gitleaks hook must be the upstream hook with no args or entry \
             override: {reason}"
        );
    }
}

#[test]
fn the_gitleaks_hook_reader_rejects_overrides_and_unknown_layouts() {
    let original = load_pre_commit_config();
    let hook_line = "      - id: gitleaks\n";
    let hooks_block = format!("    hooks:\n{hook_line}");
    let rev_line = original
        .lines()
        .find(|line| line.starts_with("    rev: "))
        .expect("control: the gitleaks block carries a rev line");
    let block = format!("{GITLEAKS_REPO_LINE}\n{rev_line}\n{hooks_block}");
    let rev = rev_line
        .trim_start_matches("    rev: ")
        .split_whitespace()
        .next()
        .expect("control: the rev line carries a value");
    assert!(
        is_commit_sha(rev),
        "control: the real gitleaks rev is a full commit sha: {rev:?}"
    );
    let flow_block = format!(
        "  - {{repo: https://{GITLEAKS_REPO_URL}, rev: {rev}, hooks: [{{id: gitleaks}}]}}\n"
    );
    assert_eq!(
        original.matches(&block).count(),
        1,
        "control: the real config holds the gitleaks block in block style"
    );

    let rev_with = |value: &str| original.replacen(rev_line, &format!("    rev: {value}"), 1);
    let cases: [(&str, String); 9] = [
        (
            "args added",
            original.replacen(
                hook_line,
                &format!("{hook_line}        args: [--redact]\n"),
                1,
            ),
        ),
        (
            "entry added",
            original.replacen(
                hook_line,
                &format!("{hook_line}        entry: gitleaks git --staged\n"),
                1,
            ),
        ),
        (
            "repo block in flow style",
            original.replacen(&block, &flow_block, 1),
        ),
        (
            "hooks list in flow style",
            original.replacen(&hooks_block, "    hooks: [{id: gitleaks}]\n", 1),
        ),
        (
            "second hook",
            original.replacen(
                hook_line,
                &format!("{hook_line}      - id: gitleaks-system\n"),
                1,
            ),
        ),
        ("block removed", original.replacen(&block, "", 1)),
        ("tag rev", rev_with("v8.30.1")),
        ("short sha rev", rev_with(&rev[..12])),
        ("uppercase sha rev", rev_with(&rev.to_ascii_uppercase())),
    ];
    for (name, mutated) in cases {
        assert_ne!(mutated, original, "control: the {name} edit must apply");
        assert!(
            check_gitleaks_hook_block(&mutated).is_err(),
            "{name}: the reader must reject this layout"
        );
    }
}

#[test]
fn the_rev_line_check_accepts_only_a_full_lowercase_commit_sha() {
    let sha = "83d9cd684c87d95d656c1458ef04895a7f1cbd8e";
    let accepted = [
        ("bare sha", format!("    rev: {sha}")),
        (
            "sha with release comment",
            format!("    rev: {sha}  # v8.30.1"),
        ),
        (
            "sha with tab before comment",
            format!("    rev: {sha}\t# v8.30.1"),
        ),
    ];
    let rejected = [
        ("tag", "    rev: v8.30.1".to_owned()),
        ("tag with comment", "    rev: v8.30.1  # v8.30.1".to_owned()),
        ("short sha", format!("    rev: {}", &sha[..12])),
        (
            "uppercase sha",
            format!("    rev: {}", sha.to_ascii_uppercase()),
        ),
        ("41 hex digits", format!("    rev: {sha}0")),
        ("non-hex character", format!("    rev: {}g", &sha[..39])),
        (
            "comment without whitespace",
            format!("    rev: {sha}#v8.30.1"),
        ),
        (
            "trailing token that is not a comment",
            format!("    rev: {sha}  v8.30.1"),
        ),
        ("quoted sha", format!("    rev: '{sha}'")),
        ("empty value", "    rev: ".to_owned()),
        ("wrong indent", format!("  rev: {sha}")),
    ];
    for (name, line) in accepted {
        assert!(
            is_valid_rev_line(&line),
            "{name}: must be accepted: {line:?}"
        );
    }
    for (name, line) in rejected {
        assert!(
            !is_valid_rev_line(&line),
            "{name}: must be rejected: {line:?}"
        );
    }
}
