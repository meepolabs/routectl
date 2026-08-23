//! Weld between `MAX_THINKING_BUDGET_READER_KINDS` and the code that
//! actually reads `routectl_internal.max_thinking_budget`.
//!
//! The const is curated (the reading code is one shared function that
//! names no provider kind), so nothing in the type system stops it from
//! going stale when a lane starts or stops routing through
//! `build_thinking`. These tests close that by re-deriving the reader set
//! from the source files on disk: the single read site, and the set of
//! modules that reach it.
//!
//! The scanned modules are enumerated from the filesystem, never listed
//! by hand: a new module that reads the field must be either mapped to a
//! provider kind or rejected loudly, so it cannot slip past the weld by
//! being absent from a list.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{MAX_THINKING_BUDGET_READER_KINDS, egress_reads_max_thinking_budget};

/// The `kind = "..."` config token an operator writes for each
/// provider-egress module root. Not the scan's input -- the scan
/// enumerates roots from disk and looks them up here, so a root missing
/// from this map is a test failure rather than an invisible skip.
const KIND_BY_MODULE_ROOT: &[(&str, &str)] = &[
    ("anthropic_api", "anthropic-api"),
    ("bedrock", "bedrock"),
    ("openai_compat", "openai-compat"),
    ("openai_responses", "openai-responses"),
    ("gemini", "gemini"),
];

fn provider_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_string()
}

/// Test modules are excluded everywhere: a test calling `build_thinking`
/// directly proves nothing about which lane reads the field on a real
/// request. The convention is a `_tests.rs` suffix.
fn is_scanned_source(path: &Path) -> bool {
    let name = file_name_of(path);
    path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs") && !name.ends_with("_tests.rs")
}

/// Every module root under `src`: each subdirectory, plus each top-level
/// non-test `.rs` file. Derived from the filesystem so no module can be
/// omitted from the scan by forgetting to list it.
fn module_roots(src: &Path) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    for entry in std::fs::read_dir(src).expect("crate src directory must exist") {
        let path = entry.expect("directory entry must read").path();
        if path.is_dir() {
            roots.insert(file_name_of(&path));
        } else if is_scanned_source(&path) {
            roots.insert(
                path.file_stem()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or_default()
                    .to_string(),
            );
        }
    }
    roots
}

/// Every non-test `.rs` file belonging to `root`, as `(relative path,
/// source text)`. `root` is either a directory under `src` or a
/// single-file module.
fn sources_of_root(src: &Path, root: &str) -> Vec<(String, String)> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("module directory must read") {
            let path = entry.expect("directory entry must read").path();
            let name = file_name_of(&path);
            if path.is_dir() {
                walk(&path, &format!("{prefix}{name}/"), out);
                continue;
            }
            if !is_scanned_source(&path) {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source file must read as UTF-8");
            out.push((format!("{prefix}{name}"), text));
        }
    }

    let mut out = Vec::new();
    let as_dir = src.join(root);
    if as_dir.is_dir() {
        walk(&as_dir, &format!("{root}/"), &mut out);
        return out;
    }
    let as_file = src.join(format!("{root}.rs"));
    let text = std::fs::read_to_string(&as_file).expect("module source must read as UTF-8");
    out.push((format!("{root}.rs"), text));
    out
}

/// Strip line comments and doc comments so a prose mention of a symbol
/// never counts as a call site. Block comments are not stripped: this
/// crate's convention is `//` / `///` throughout, and a naive `/* */`
/// strip would corrupt string literals.
fn code_only(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `text` names `symbol` as a whole identifier. A substring test
/// would be wrong in both directions here: the Gemini egress has its own
/// unrelated `build_thinking_config`, which contains the reader's name as a
/// prefix and would false-positive that lane into the reader set.
fn names_symbol(text: &str, symbol: &str) -> bool {
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    text.match_indices(symbol).any(|(at, _)| {
        let before_ok = at == 0 || !is_ident_byte(text.as_bytes()[at - 1]);
        let after = at + symbol.len();
        let after_ok = after == text.len() || !is_ident_byte(text.as_bytes()[after]);
        before_ok && after_ok
    })
}

/// The one function that reads the field, and the sole entry point every
/// reading lane goes through.
const READER_FN: &str = "build_thinking";

/// `bedrock-invoke` never calls the reader directly -- it delegates whole
/// body construction to the anthropic-api normalizer, which does. Any
/// call into that normalizer therefore also reaches the read.
///
/// Matched as a plain substring, not a whole identifier: the Invoke lane
/// calls the `normalize_deferring_format_key_warn` variant, and every
/// `normalize*` entry point in that module assembles the body through
/// [`READER_FN`].
const DELEGATING_CALL: &str = "anthropic_api::request::normalize";

const FIELD_PATH: &str = "routectl_internal.max_thinking_budget";

/// `<root>/<file>:<line>` for every line that reads the field, over all
/// module roots found on disk.
fn read_sites(src: &Path) -> Vec<String> {
    let mut sites = Vec::new();
    for root in module_roots(src) {
        for (file, text) in sources_of_root(src, &root) {
            for (index, line) in code_only(&text).lines().enumerate() {
                if line.contains(FIELD_PATH) {
                    sites.push(format!("{file}:{}", index + 1));
                }
            }
        }
    }
    sites
}

/// The module roots whose non-test source reaches [`READER_FN`].
fn reader_roots(src: &Path) -> BTreeSet<String> {
    module_roots(src)
        .into_iter()
        .filter(|root| {
            sources_of_root(src, root).into_iter().any(|(_, text)| {
                let code = code_only(&text);
                names_symbol(&code, READER_FN) || code.contains(DELEGATING_CALL)
            })
        })
        .collect()
}

/// Map reader roots onto config kind tokens. `Err` carries the roots with
/// no mapping: a module that reaches the read but is not a known provider
/// egress, which the weld must refuse to interpret rather than drop.
fn kinds_of_reader_roots(roots: &BTreeSet<String>) -> Result<BTreeSet<&'static str>, Vec<String>> {
    let mapping: BTreeMap<&str, &str> = KIND_BY_MODULE_ROOT.iter().copied().collect();
    let mut kinds = BTreeSet::new();
    let mut unmapped = Vec::new();
    for root in roots {
        match mapping.get(root.as_str()) {
            Some(kind) => {
                kinds.insert(*kind);
            }
            None => unmapped.push(root.clone()),
        }
    }
    if unmapped.is_empty() {
        Ok(kinds)
    } else {
        Err(unmapped)
    }
}

#[test]
fn every_mapped_module_root_still_exists_on_disk() {
    let roots = module_roots(&provider_src_dir());
    for (root, kind) in KIND_BY_MODULE_ROOT {
        assert!(
            roots.contains(*root),
            "KIND_BY_MODULE_ROOT maps `{kind}` to src/{root}, which no longer \
             exists -- a renamed or removed egress must move this entry too"
        );
    }
}

#[test]
fn the_field_is_read_in_exactly_one_place() {
    // POSITIVE CONTROL for the derivation below: if the field acquired a
    // second read site, the `build_thinking`-reachability scan would stop
    // being a complete account of who reads it, and the derived reader set
    // would silently under-report.
    let sites = read_sites(&provider_src_dir());

    assert_eq!(
        sites.len(),
        1,
        "the operator budget cap must be read in exactly one place so \
         `build_thinking` reachability is a complete account of the reader \
         set; found: {sites:?}"
    );
    assert!(
        sites[0].starts_with("anthropic_api/extras.rs"),
        "the read must live beside MAX_THINKING_BUDGET_READER_KINDS in \
         anthropic_api/extras.rs, got {}",
        sites[0]
    );
}

#[test]
fn the_const_names_exactly_the_kinds_whose_egress_reaches_the_reader() {
    let roots = reader_roots(&provider_src_dir());
    let derived = kinds_of_reader_roots(&roots).unwrap_or_else(|unmapped| {
        panic!(
            "these modules reach the budget read but map to no provider kind: \
             {unmapped:?} -- add them to KIND_BY_MODULE_ROOT (or route them \
             away from the read) so the config diagnostic can account for them"
        )
    });

    let declared: BTreeSet<&str> = MAX_THINKING_BUDGET_READER_KINDS.iter().copied().collect();
    assert_eq!(
        derived, declared,
        "MAX_THINKING_BUDGET_READER_KINDS must name exactly the egress kinds \
         whose source reaches the budget read -- a lane that gained or lost \
         the read without this list moving makes the config diagnostic lie"
    );
}

#[test]
fn the_reader_scan_matches_whole_identifiers_only() {
    // The Gemini lane is excluded from the reader set by the identifier
    // boundary, not by absence: its own `build_thinking_config` carries the
    // reader's name as a prefix, so a substring scan would pull it in. Pin
    // both directions of the boundary, and pin that the lookalike is still
    // really there to be mismatched.
    assert!(
        sources_of_root(&provider_src_dir(), "gemini")
            .iter()
            .any(|(_, text)| text.contains("build_thinking_config")),
        "the gemini lookalike symbol must exist for this control to mean anything"
    );
    assert!(!names_symbol("fn build_thinking_config(", READER_FN));
    assert!(!names_symbol("let x = my_build_thinking;", READER_FN));
    assert!(names_symbol(
        "let thinking = build_thinking(req, false);",
        READER_FN
    ));
}

#[test]
fn a_non_reading_kind_and_an_unknown_token_answer_false() {
    for kind in ["gemini", "openai-compat", "openai-responses"] {
        assert!(
            !egress_reads_max_thinking_budget(kind),
            "{kind} has no path to the budget read"
        );
    }
    assert!(
        !egress_reads_max_thinking_budget("some-future-kind"),
        "a kind this crate ships no egress for cannot be reading the field"
    );
    // Positive control: the predicate is not vacuously false.
    for kind in MAX_THINKING_BUDGET_READER_KINDS {
        assert!(
            egress_reads_max_thinking_budget(kind),
            "{kind} is a declared reader"
        );
    }
}

/// Removes its directory on drop so a failing assertion cannot leave the
/// fixture behind.
struct FixtureDir(PathBuf);

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A synthetic `src`-shaped tree: a directory module that reaches the
/// reader and reads the field, a directory module that does neither, and
/// a single-file module that reaches the reader.
fn synthetic_src_tree(tag: &str) -> FixtureDir {
    let root =
        std::env::temp_dir().join(format!("routectl-budget-weld-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(src.join("future_egress")).expect("fixture dir must create");
    std::fs::create_dir_all(src.join("quiet_module")).expect("fixture dir must create");
    std::fs::write(
        src.join("future_egress/request.rs"),
        format!(
            "pub fn body(req: &Req) {{\n    let cap = req.{FIELD_PATH};\n    \
             let thinking = {READER_FN}(req, false);\n}}\n"
        ),
    )
    .expect("fixture source must write");
    std::fs::write(
        src.join("future_egress/mod.rs"),
        "pub mod request;\n#[cfg(test)]\n#[path = \"request_tests.rs\"]\nmod request_tests;\n",
    )
    .expect("fixture source must write");
    std::fs::write(
        src.join("future_egress/request_tests.rs"),
        format!("#[test]\nfn t() {{ let _ = {READER_FN}(&req, false); }}\n"),
    )
    .expect("fixture source must write");
    std::fs::write(
        src.join("quiet_module/mod.rs"),
        "pub fn passthrough() {}\n// mentions build_thinking in prose only\n",
    )
    .expect("fixture source must write");
    std::fs::write(
        src.join("flat_future.rs"),
        format!("pub fn go(req: &Req) {{ let _ = {READER_FN}(req, true); }}\n"),
    )
    .expect("fixture source must write");
    FixtureDir(root)
}

#[test]
fn an_unmapped_reader_module_fails_the_weld() {
    // NEGATIVE CONTROL for the enumeration itself: point the same scan at a
    // tree containing modules this crate has never heard of, and the weld
    // must report them rather than skip them. If enumeration ever regressed
    // to a curated list, these roots would vanish and the assertions below
    // would fail.
    let fixture = synthetic_src_tree("unmapped");
    let src = fixture.0.join("src");

    let roots = module_roots(&src);
    assert_eq!(
        roots,
        ["flat_future", "future_egress", "quiet_module"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>(),
        "enumeration must find directory modules and single-file modules alike"
    );

    let readers = reader_roots(&src);
    assert_eq!(
        readers,
        ["flat_future", "future_egress"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>(),
        "both new readers must be detected; the prose-only module must not be"
    );

    let unmapped = kinds_of_reader_roots(&readers)
        .expect_err("unknown reader modules must not resolve to kinds");
    assert_eq!(unmapped, vec!["flat_future", "future_egress"]);

    // The read-site count moves with the fixture too, so a new read site in
    // an unlisted module cannot hide from the single-read assertion.
    assert_eq!(
        read_sites(&src),
        vec!["future_egress/request.rs:2".to_string()],
        "the field read in the synthetic module must be located, and the \
         test-file read must be excluded"
    );
}
