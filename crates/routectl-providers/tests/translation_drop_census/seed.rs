//! The `SEED-VERDICT:` marker grammar: a retained seed site's record of WHY a
//! baked rule stays baked instead of being learned from upstream rejections.
//!
//! A child of the `TRANSLATION-DROP:` parser so it reuses that parser's
//! comment, anchor and forbidden-content rules rather than re-deriving them,
//! while leaving every `TRANSLATION-DROP:` parse untouched: nothing here is
//! called from the drop census or its welds.
//!
//! ```text
//! // SEED-VERDICT: <tag> -- <self-contained reason>
//! ```
//!
//! A long reason wraps onto the comment lines below; the reason ends at a
//! blank comment line or at the next `SEED-VERDICT:` / `TRANSLATION-DROP:`
//! token. The refusal of file paths, line numbers and planning ids reads the
//! WHOLE comment block below the marker, blank comment lines included, so the
//! marker and its wrapped reason close their comment block, directly above the
//! item they record.
//!
//! The population is not confined to the drop census's surfaces: the sweep
//! walks the providers and router `src` trees recursively.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{
    MARKER_TOKEN, anchors_code, comment_text, is_test_file, normalize, reject_forbidden_content,
};

/// The token that opens every seed marker.
pub const SEED_TOKEN: &str = "SEED-VERDICT:";

/// Router test modules whose names the `_tests` / `_test_support` naming
/// derivation does not recognize. Each is a `#[cfg(test)]` module declaration,
/// which the census derives and checks against this list.
pub const ROUTER_TEST_FILES: &[&str] = &[
    "alloc_probe.rs",
    "config/tests.rs",
    "probe_scheduler/test_support.rs",
    "router/tests.rs",
    "test_secret.rs",
];

pub const PROVIDERS: &str = "routectl-providers";
pub const ROUTER: &str = "routectl-router";

/// The closed tag vocabulary of a seed marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SeedTag {
    /// A baked prior about what an upstream accepts, kept because no
    /// rejection arrives (or none could be learned) to derive it from. A
    /// recorded seed: a candidate for retirement once the acceptance is
    /// observed instead.
    AcceptanceSeed,
    /// A fixed defense that stays static whatever an upstream would accept:
    /// relaxing it on evidence of acceptance would be the vulnerability.
    StaticSecurity,
    /// routectl structurally cannot handle the shape, so the rule stays
    /// static. Not the drop census's `structural`, which means an arm loses
    /// nothing.
    Structural,
}

impl SeedTag {
    pub const fn label(self) -> &'static str {
        match self {
            Self::AcceptanceSeed => "acceptance-seed",
            Self::StaticSecurity => "static-security",
            Self::Structural => "structural",
        }
    }

    fn parse(token: &str, where_: &str) -> Result<Self, String> {
        match token {
            "acceptance-seed" => Ok(Self::AcceptanceSeed),
            "static-security" => Ok(Self::StaticSecurity),
            "structural" => Ok(Self::Structural),
            "representability" => Err(format!(
                "{where_} carries `representability`; a wire-representability loss is recorded \
                 by its {MARKER_TOKEN} marker, never by a seed marker"
            )),
            other => Err(format!(
                "{where_} opens with {other:?}, which is no seed tag; the vocabulary is closed \
                 to acceptance-seed, static-security and structural"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedMarker {
    pub krate: String,
    /// Path relative to the crate's `src`.
    pub file: String,
    /// 1-based line, for error attribution only.
    pub line: usize,
    pub tag: SeedTag,
    pub reason: String,
}

fn is_marker_line(text: &str) -> bool {
    text.contains(SEED_TOKEN) || text.contains(MARKER_TOKEN)
}

fn seed_continuation(lines: &[&str], marker_line: usize) -> String {
    let mut parts = Vec::new();
    for line in lines.iter().skip(marker_line + 1) {
        let Some(text) = comment_text(line) else {
            break;
        };
        if text.is_empty() || is_marker_line(text) {
            break;
        }
        parts.push(text.to_string());
    }
    parts.join(" ")
}

fn seed_block_below(lines: &[&str], marker_line: usize) -> String {
    let mut parts = Vec::new();
    for line in lines.iter().skip(marker_line + 1) {
        let Some(text) = comment_text(line) else {
            break;
        };
        if is_marker_line(text) {
            break;
        }
        parts.push(text.to_string());
    }
    parts.join(" ")
}

fn parse_seed_marker(
    krate: &str,
    file: &str,
    lines: &[&str],
    index: usize,
) -> Result<SeedMarker, String> {
    let line_no = index + 1;
    let where_ = format!("the seed marker in {krate} {file} on line {line_no}");
    let Some(comment) = comment_text(lines[index]) else {
        return Err(format!(
            "{where_} is not a comment; a seed verdict sits in the comment of the item it records"
        ));
    };
    if !anchors_code(lines, index) {
        return Err(format!(
            "{where_} anchors no code; the item it recorded has moved out from under it"
        ));
    }
    let body = comment
        .split_once(SEED_TOKEN)
        .map(|(_, rest)| rest.trim())
        .unwrap_or_default();
    reject_forbidden_content(
        &format!("{body} {}", seed_block_below(lines, index)),
        &where_,
    )?;
    let Some((before_dashes, reason)) = body.split_once("--") else {
        return Err(format!(
            "{where_} has no `-- <reason>`; the reason is the seed's justification"
        ));
    };
    let mut tag_tokens = before_dashes.split_whitespace();
    let Some(tag_token) = tag_tokens.next() else {
        return Err(format!("{where_} declares no tag"));
    };
    let tag = SeedTag::parse(tag_token, &where_)?;
    if let Some(extra) = tag_tokens.next() {
        return Err(format!(
            "{where_} carries {extra:?} between its tag and `--`; a seed marker is exactly \
             `<tag> -- <reason>`"
        ));
    }
    let reason = normalize(&format!("{reason} {}", seed_continuation(lines, index)));
    if reason.is_empty() {
        return Err(format!("{where_} has an empty reason"));
    }
    Ok(SeedMarker {
        krate: krate.to_string(),
        file: file.to_string(),
        line: line_no,
        tag,
        reason,
    })
}

/// Every seed marker in one source text. An empty result is legal here.
pub fn parse_seed_file(krate: &str, file: &str, source: &str) -> Result<Vec<SeedMarker>, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut markers = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        if raw.contains(SEED_TOKEN) {
            markers.push(parse_seed_marker(krate, file, &lines, index)?);
        }
    }
    Ok(markers)
}

/// One swept source: `(crate, src-relative path, source)`.
pub type SweptSource = (String, String, String);

/// The two swept trees: the providers crate's own `src` and the router's,
/// reached through the workspace layout.
pub fn crate_roots() -> Vec<(&'static str, PathBuf)> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    vec![
        (PROVIDERS, manifest.join("src")),
        (ROUTER, manifest.join("../routectl-router/src")),
    ]
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|err| format!("{} must be a readable directory ({err})", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("cannot read an entry of {}: {err}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|err| format!("cannot type {}: {err}", path.display()))?;
        if kind.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            let relative = path
                .strip_prefix(root)
                .map_err(|err| format!("{} escapes its root ({err})", path.display()))?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let source = std::fs::read_to_string(&path)
                .map_err(|err| format!("{} must be readable ({err})", path.display()))?;
            out.push((relative, source));
        }
    }
    Ok(())
}

/// Every `.rs` file under each root, test files included, sorted. A root that
/// is missing or holds no Rust source is an error: half a sweep is not a
/// smaller census, it is a wrong one.
pub fn sweep_all(roots: &[(&str, PathBuf)]) -> Result<Vec<SweptSource>, String> {
    let mut all = Vec::new();
    for (krate, root) in roots {
        let mut found = Vec::new();
        walk(root, root, &mut found)?;
        if found.is_empty() {
            return Err(format!(
                "{krate} source root {} holds no .rs file; the sweep is looking in the wrong place",
                root.display()
            ));
        }
        all.extend(
            found
                .into_iter()
                .map(|(file, source)| ((*krate).to_string(), file, source)),
        );
    }
    all.sort();
    Ok(all)
}

pub fn is_excluded(krate: &str, file: &str) -> bool {
    is_test_file(file) || (krate == ROUTER && ROUTER_TEST_FILES.contains(&file))
}

/// The production half of [`sweep_all`].
pub fn production_sources(roots: &[(&str, PathBuf)]) -> Result<Vec<SweptSource>, String> {
    Ok(sweep_all(roots)?
        .into_iter()
        .filter(|(krate, file, _)| !is_excluded(krate, file))
        .collect())
}

/// Every seed marker over a supplied population. Zero markers is an error.
pub fn seed_census_over(population: &[SweptSource]) -> Result<Vec<SeedMarker>, String> {
    let mut markers = Vec::new();
    for (krate, file, source) in population {
        markers.extend(parse_seed_file(krate, file, source)?);
    }
    if markers.is_empty() {
        return Err(
            "the seed census recovered no marker; an empty parse is a failed parse".to_string(),
        );
    }
    Ok(markers)
}

pub fn seed_census() -> Result<Vec<SeedMarker>, String> {
    seed_census_over(&production_sources(&crate_roots())?)
}

type CountKey = (String, String, SeedTag);

fn tally<I: IntoIterator<Item = CountKey>>(keys: I) -> BTreeMap<CountKey, usize> {
    let mut counts = BTreeMap::new();
    for key in keys {
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

/// Compare the found markers against the pinned table as occurrence counts per
/// `(crate, file, tag)`, reporting both directions.
pub fn compare_to_table(
    found: &[SeedMarker],
    table: &[(&str, &str, SeedTag)],
) -> Result<(), String> {
    let found = tally(
        found
            .iter()
            .map(|m| (m.krate.clone(), m.file.clone(), m.tag)),
    );
    let pinned = tally(
        table
            .iter()
            .map(|(k, f, t)| ((*k).to_string(), (*f).to_string(), *t)),
    );
    if found == pinned {
        return Ok(());
    }
    let mut problems = Vec::new();
    for (key, count) in &found {
        let expected = pinned.get(key).copied().unwrap_or(0);
        if *count != expected {
            problems.push(format!(
                "{} {} {}: {count} marker(s) in source, {expected} in the table",
                key.0,
                key.1,
                key.2.label()
            ));
        }
    }
    for (key, expected) in &pinned {
        if !found.contains_key(key) {
            problems.push(format!(
                "{} {} {}: 0 marker(s) in source, {expected} in the table",
                key.0,
                key.1,
                key.2.label()
            ));
        }
    }
    Err(problems.join("; "))
}

/// The `#[cfg(test)]` outline module declarations in one source file, resolved
/// to src-relative paths: `mod name;` with an optional `#[path = "..."]`.
pub fn cfg_test_module_files(file: &str, source: &str) -> Vec<String> {
    let dir = file.rsplit_once('/').map_or("", |(d, _)| d);
    let name = file.rsplit('/').next().unwrap_or(file);
    let stem = name.trim_end_matches(".rs");
    let owns_dir = matches!(name, "mod.rs" | "lib.rs" | "main.rs");
    let join = |base: &str, tail: &str| {
        if base.is_empty() {
            tail.to_string()
        } else {
            format!("{base}/{tail}")
        }
    };
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if *line != "#[cfg(test)]" {
            continue;
        }
        let mut next = i + 1;
        let path_attr = lines
            .get(next)
            .and_then(|l| l.strip_prefix("#[path = \""))
            .and_then(|rest| rest.split('"').next());
        if path_attr.is_some() {
            next += 1;
        }
        let Some(decl) = lines.get(next) else {
            continue;
        };
        let decl = decl
            .strip_prefix("pub(crate) ")
            .or_else(|| decl.strip_prefix("pub "))
            .unwrap_or(decl);
        let Some(module) = decl.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')) else {
            continue;
        };
        let resolved = match (path_attr, owns_dir) {
            (Some(path), _) => join(dir, path),
            (None, true) => join(dir, &format!("{module}.rs")),
            (None, false) => join(&join(dir, stem), &format!("{module}.rs")),
        };
        out.push(collapse_parent_segments(&resolved));
    }
    out
}

/// Resolve `..` segments, which a `#[path]` reaching a sibling directory uses.
fn collapse_parent_segments(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        if segment == ".." {
            parts.pop();
        } else {
            parts.push(segment);
        }
    }
    parts.join("/")
}

pub fn expect_seed<T>(parsed: Result<T, String>) -> T {
    parsed.unwrap_or_else(|why| panic!("the seed census cannot be evaluated: {why}"))
}
