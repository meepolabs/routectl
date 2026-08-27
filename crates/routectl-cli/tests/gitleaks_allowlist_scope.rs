//! Drift detector for `.gitleaks.toml`'s path allowlist.
//!
//! The driver fixture corpus is committed, so `gitleaks protect --staged`
//! (the repo's pre-commit hook) scans it on every commit that touches it.
//! That scan is the commit-time backstop for credential shapes the
//! capture-time scrub gate cannot see, and it holds only while the corpus
//! path stays OUT of the allowlist. The first fixture that trips a false
//! positive makes adding the corpus root to `[allowlist] paths` the
//! obvious fix -- and that one line would exempt the only committed
//! fixture corpus from secret scanning, permanently and silently. The
//! config's own header says to fix the false positive instead.
//!
//! So the contract is an ABSENCE, and a bare absence check is worthless:
//! a typo'd path constant, a broken regex, or a parse that yields nothing
//! all "pass". Three independent guards close that:
//!
//! 1. The parse asserts the entry list is non-empty AND pins its count.
//! 2. The shared path prefix is welded to a directory that must exist on
//!    disk, so a typo in the corpus path is a failure rather than a
//!    silent miss.
//! 3. A POSITIVE CONTROL runs the same matcher against the live-capture
//!    root, which IS allowlisted, and requires it to fire.
//!
//! The pinned entry SET (not a `contains` check) makes the next allowlist
//! change a review moment rather than a silent widening.

use std::path::{Path, PathBuf};

use regex::Regex;

const GITLEAKS_CONFIG: &str = ".gitleaks.toml";

/// Parent of both fixture roots, mirroring `common::replay::harness`'s
/// `fixtures_dir`. Repo-relative, because gitleaks matches its path
/// patterns against repo-relative paths.
const FIXTURES_ROOT: &str = "crates/routectl-cli/tests/fixtures";

/// Every entry currently in `[allowlist] paths`, in file order. Pinned as
/// a SET rather than a count so that widening the allowlist cannot pass
/// review unnoticed.
const EXPECTED_ALLOWLIST_PATHS: &[&str] = &[
    r"Cargo\.lock",
    "^target/",
    "^crates/routectl-cli/tests/fixtures/captured/",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root must resolve from the crate manifest dir")
}

/// Repo-relative path of a fixture corpus root, in the trailing-slash
/// form gitleaks path patterns use.
fn fixture_root(name: &str) -> String {
    format!("{FIXTURES_ROOT}/{name}/")
}

/// `[allowlist] paths` as written, with the non-vacuity guard applied
/// before any caller can reason about the contents.
fn allowlist_paths() -> Vec<String> {
    let config = repo_root().join(GITLEAKS_CONFIG);
    let source = std::fs::read_to_string(&config).unwrap_or_else(|err| {
        panic!(
            "{GITLEAKS_CONFIG} must exist and be readable at {} ({err}): it \
             configures the pre-commit secret scan",
            config.display()
        )
    });
    let document: toml::Table = toml::from_str(&source)
        .unwrap_or_else(|err| panic!("{GITLEAKS_CONFIG} must be valid TOML ({err})"));

    let paths = document
        .get("allowlist")
        .and_then(|allowlist| allowlist.get("paths"))
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| {
            panic!(
                "{GITLEAKS_CONFIG} must declare an `[allowlist] paths` array; \
                 without it this parse yields nothing and every assertion \
                 below is vacuous"
            )
        });

    let paths: Vec<String> = paths
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| {
                    panic!(
                        "every `[allowlist] paths` entry in {GITLEAKS_CONFIG} \
                         must be a string; found {entry:?}"
                    )
                })
                .to_owned()
        })
        .collect();

    assert_eq!(
        paths.len(),
        EXPECTED_ALLOWLIST_PATHS.len(),
        "{GITLEAKS_CONFIG} declares {} `[allowlist] paths` entries but this \
         test pins {}. Every entry is a place a real secret can hide \
         undetected, so a change here is deliberate: if the new entry is \
         correct, update EXPECTED_ALLOWLIST_PATHS in this file to match and \
         say why in the commit body. A mismatch is also how a broken parse \
         surfaces instead of silently passing the absence check below.",
        paths.len(),
        EXPECTED_ALLOWLIST_PATHS.len()
    );

    paths
}

/// Allowlist entries whose pattern matches `candidate`, evaluated as
/// regexes the way gitleaks evaluates them -- an anchored `^.../captured/`
/// must not be credited for a `.../driver/` path just because the two
/// share a prefix.
fn entries_matching(paths: &[String], candidate: &str) -> Vec<String> {
    paths
        .iter()
        .filter(|pattern| {
            let compiled = Regex::new(pattern).unwrap_or_else(|err| {
                panic!(
                    "`[allowlist] paths` entry {pattern:?} in {GITLEAKS_CONFIG} \
                     must compile as a regex ({err}); an uncompilable pattern \
                     would make this matcher find nothing and read as a pass"
                )
            });
            compiled.is_match(candidate)
        })
        .cloned()
        .collect()
}

/// Guard on the path constants themselves: a typo in `FIXTURES_ROOT`
/// would make the absence assertion pass against a path that exists
/// nowhere.
#[test]
fn the_fixture_root_constant_names_a_real_directory() {
    let fixtures = repo_root().join(FIXTURES_ROOT);

    assert!(
        fixtures.is_dir(),
        "FIXTURES_ROOT ({FIXTURES_ROOT}) must resolve to a real directory at \
         {}; it is the prefix every assertion in this file is written \
         against, so a stale value silently defeats them",
        fixtures.display()
    );
}

/// THE contract.
#[test]
fn the_driver_fixture_corpus_is_not_exempt_from_secret_scanning() {
    let paths = allowlist_paths();
    let driver_root = fixture_root("driver");

    let matches = entries_matching(&paths, &driver_root);

    assert!(
        matches.is_empty(),
        "`[allowlist] paths` in {GITLEAKS_CONFIG} exempts the committed \
         driver fixture corpus ({driver_root}) from secret scanning via \
         {matches:?}. That corpus is committed and public, and \
         `gitleaks protect --staged` is the commit-time backstop for the \
         credential shapes the capture-time scrub gate cannot detect. \
         Remove the entry and fix the false positive at its source \
         (scrub or drop the offending fixture) instead."
    );
}

/// POSITIVE CONTROL for the assertion above. The live-capture root IS
/// allowlisted, so the same matcher must fire on it -- otherwise
/// "no entry matches driver/" is satisfied by a matcher that matches
/// nothing at all.
#[test]
fn the_allowlist_matcher_fires_on_the_allowlisted_capture_root() {
    let paths = allowlist_paths();
    let captured_root = fixture_root("captured");

    let matches = entries_matching(&paths, &captured_root);

    assert_eq!(
        matches,
        vec![format!("^{captured_root}")],
        "the live-capture root ({captured_root}) IS listed in \
         `[allowlist] paths` of {GITLEAKS_CONFIG}, so this matcher must \
         report exactly that entry. It did not, which means the matcher \
         itself is broken and the driver-corpus absence assertion proves \
         nothing."
    );
}

/// The exact set, so the next allowlist edit is a review moment.
#[test]
fn the_path_allowlist_is_exactly_the_reviewed_set() {
    let paths = allowlist_paths();

    assert_eq!(
        paths, EXPECTED_ALLOWLIST_PATHS,
        "`[allowlist] paths` in {GITLEAKS_CONFIG} drifted from the reviewed \
         set. Each entry disables secret scanning for everything under it; \
         confirm the change is intended, then update EXPECTED_ALLOWLIST_PATHS \
         here."
    );
}
