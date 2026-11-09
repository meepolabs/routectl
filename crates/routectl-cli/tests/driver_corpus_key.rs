//! Weld between the committed driver corpus's directory names and the
//! landing key the capture rig composes from the same facts it writes into
//! `meta.json`.
//!
//! The rig lands a driver capture at
//! `<lane>/<case_id>__<driver>__<config_variant>` and records each token in
//! `meta.json` (`lane`, `case_id`, `client.driver`, `config_variant`). A
//! committed directory whose name disagrees with its own `meta.json` is one a
//! recapture would NOT replace: the rerun lands beside it as a sibling, and
//! the corpus then holds two fixtures for one scenario. So every committed
//! case directory must equal the key composed from its own metadata, and no
//! two may compose the same key.
//!
//! The key carries no model token. The committed corpus holds one model per
//! key (the model is in `meta.json`); a recapture under another model
//! replaces the fixture rather than landing beside it.
//!
//! Every clause is decided by [`key_violations`] over plain entries, so the
//! same predicate that runs on the committed corpus runs on synthetic entries
//! with exactly one clause broken.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::replay::{FixtureMeta, discover_driver_fixtures, driver_root, load_fixture};

/// Separator between the three leaf tokens, as the rig writes it.
const LEAF_SEP: &str = "__";

/// Case directories the committed corpus holds today. A floor, not an exact
/// count: a walk that found fewer has lost fixtures (or walked the wrong
/// root), and a weld over an empty corpus would pass on anything.
const MIN_COMMITTED_CASES: usize = 7;

/// One case directory as the weld sees it: where it sits and what its
/// `meta.json` claims.
struct Entry {
    lane_dir: String,
    leaf: String,
    meta: FixtureMeta,
}

/// Whether `token` matches the rig's leaf-token grammar,
/// `^[a-z0-9]+([.-][a-z0-9]+)*$`: runs of ASCII lowercase letters and
/// digits joined by single `.` or `-`.
fn is_leaf_token(token: &str) -> bool {
    token.split(['.', '-']).all(|run| {
        !run.is_empty()
            && run
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// The leaf the rig composes from a fixture's metadata.
fn composed_leaf(meta: &FixtureMeta) -> String {
    format!(
        "{}{LEAF_SEP}{}{LEAF_SEP}{}",
        meta.case_id, meta.client.driver, meta.config_variant
    )
}

/// Every way `entries` fail to be a corpus keyed by its own metadata. Empty
/// means each directory is exactly its composed key and no key repeats.
fn key_violations(entries: &[Entry]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();

    for entry in entries {
        let here = format!("{}/{}", entry.lane_dir, entry.leaf);
        let meta = &entry.meta;
        for (field, value) in [
            ("lane", &meta.lane),
            ("case_id", &meta.case_id),
            ("client.driver", &meta.client.driver),
            ("config_variant", &meta.config_variant),
        ] {
            if !is_leaf_token(value) {
                out.push(format!(
                    "token: `{here}` records {field} `{value}`, which is not a landing-key token"
                ));
            }
        }
        if entry.lane_dir != meta.lane {
            out.push(format!(
                "lane: `{here}` sits under lane `{}` but records lane `{}`",
                entry.lane_dir, meta.lane
            ));
        }
        let composed = composed_leaf(meta);
        if entry.leaf != composed {
            out.push(format!(
                "leaf: `{here}` is not the key its meta.json composes (`{composed}`)"
            ));
        }
        if let Some(first) = seen.insert((meta.lane.clone(), composed.clone()), here.clone()) {
            out.push(format!(
                "duplicate: `{first}` and `{here}` both compose `{}/{composed}`",
                meta.lane
            ));
        }
    }
    out
}

/// Immediate non-dot subdirectories of `parent`, sorted: the same entries
/// the loader's driver walk offers.
fn case_dirs_under(parent: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(parent)
        .unwrap_or_else(|e| panic!("`{}` must list: {e}", parent.display()))
        .map(|entry| entry.expect("a directory entry reads").path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        })
        .collect();
    dirs.sort();
    dirs
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Every committed case directory, loaded. A directory that does not load
/// fails the run by name: it cannot be keyed, and skipping it would leave a
/// mis-keyed fixture unchecked.
fn committed_entries(root: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    for lane in case_dirs_under(root) {
        for case in case_dirs_under(&lane) {
            let fixture = load_fixture(&case).unwrap_or_else(|e| {
                panic!(
                    "committed driver fixture `{}` must load: {e}",
                    case.display()
                )
            });
            entries.push(Entry {
                lane_dir: file_name(&lane),
                leaf: file_name(&case),
                meta: fixture.meta,
            });
        }
    }
    entries
}

#[test]
fn every_committed_driver_case_directory_is_the_key_its_meta_composes() {
    let root = driver_root();
    let entries = committed_entries(&root);
    let loader_walk = discover_driver_fixtures(&root)
        .unwrap_or_else(|e| panic!("the driver root {} must walk: {e}", root.display()));

    assert!(
        entries.len() >= MIN_COMMITTED_CASES,
        "the weld walked {} case directories under {}, fewer than the {MIN_COMMITTED_CASES} \
         committed",
        entries.len(),
        root.display(),
    );
    assert_eq!(
        entries.len(),
        loader_walk.entries_walked(),
        "the weld must walk exactly the directories the loader's driver walk offers",
    );
    let violations = key_violations(&entries);
    assert!(
        violations.is_empty(),
        "committed driver corpus is not keyed by its own metadata:\n{}",
        violations.join("\n"),
    );
}

// ---- controls: each clause, broken alone, over synthetic entries ----

fn well_keyed(case_id: &str, driver: &str, variant: &str) -> Entry {
    let mut meta: FixtureMeta =
        serde_json::from_value(common::replay::current_meta()).expect("the planted meta parses");
    meta.case_id = case_id.to_string();
    meta.client.driver = driver.to_string();
    meta.config_variant = variant.to_string();
    Entry {
        lane_dir: meta.lane.clone(),
        leaf: composed_leaf(&meta),
        meta,
    }
}

/// Asserts the violations are non-empty and every one carries `tag`, so a
/// control cannot be satisfied by an unrelated clause.
fn assert_only(violations: &[String], tag: &str) {
    assert!(
        !violations.is_empty(),
        "the broken `{tag}` clause produced no violation"
    );
    for violation in violations {
        assert!(
            violation.starts_with(tag),
            "expected only `{tag}` violations, got: {violation}"
        );
    }
}

#[test]
fn a_well_keyed_corpus_has_no_violation() {
    let entries = [
        well_keyed("plain-turn-01", "claude-code", "anthropic-api"),
        well_keyed("plain-turn-01", "claude-code", "anthropic-api.front-proxy"),
        well_keyed("plain-turn-01", "claude-code-print", "anthropic-api"),
    ];

    assert_eq!(key_violations(&entries), Vec::<String>::new());
}

#[test]
fn a_directory_still_named_by_case_alone_is_a_leaf_violation() {
    let mut entry = well_keyed("plain-turn-01", "claude-code", "anthropic-api");
    entry.leaf = "plain-turn-01".to_string();

    assert_only(&key_violations(&[entry]), "leaf:");
}

#[test]
fn a_directory_naming_another_driver_than_its_meta_is_a_leaf_violation() {
    let mut entry = well_keyed("thinking-01", "claude-code-print", "anthropic-api");
    entry.leaf = "thinking-01__claude-code__anthropic-api".to_string();

    assert_only(&key_violations(&[entry]), "leaf:");
}

#[test]
fn a_case_under_a_lane_its_meta_does_not_record_is_a_lane_violation() {
    let mut entry = well_keyed("plain-turn-01", "claude-code", "anthropic-api");
    entry.lane_dir = "bedrock".to_string();

    assert_only(&key_violations(&[entry]), "lane:");
}

#[test]
fn an_empty_driver_token_is_a_token_violation_even_when_the_name_agrees() {
    let entry = well_keyed("plain-turn-01", "", "anthropic-api");

    assert_only(&key_violations(&[entry]), "token:");
}

#[test]
fn a_second_directory_composing_an_existing_key_is_a_duplicate_violation() {
    // Two directories cannot share a path, so a repeated key always arrives
    // through a directory named otherwise: here a capture under another
    // model left at a hand-picked name.
    let first = well_keyed("plain-turn-01", "claude-code", "anthropic-api");
    let mut second = well_keyed("plain-turn-01", "claude-code", "anthropic-api");
    second.meta.model = Some("another-model".to_string());
    second.leaf = "plain-turn-01-other-model".to_string();

    let violations = key_violations(&[first, second]);

    let duplicates: Vec<&String> = violations
        .iter()
        .filter(|v| v.starts_with("duplicate:"))
        .collect();
    assert_eq!(
        duplicates.len(),
        1,
        "expected one duplicate violation, got: {violations:?}"
    );
    assert!(
        duplicates[0].contains("anthropic-api/plain-turn-01__claude-code__anthropic-api")
            && duplicates[0].contains("anthropic-api/plain-turn-01-other-model"),
        "the duplicate must name both directories: {}",
        duplicates[0],
    );
}

#[test]
fn the_leaf_token_grammar_accepts_dotted_and_dashed_tokens_only() {
    for accepted in [
        "anthropic-api",
        "anthropic-api.front-proxy",
        "claude-code-print",
        "a1",
    ] {
        assert!(is_leaf_token(accepted), "`{accepted}` must be accepted");
    }
    for refused in ["", "Upper", "a__b", "a..b", "-a", "a-", ".a", "a/b", "a b"] {
        assert!(!is_leaf_token(refused), "`{refused}` must be refused");
    }
}
