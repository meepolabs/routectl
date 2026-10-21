//! The `SEED-VERDICT:` marker grammar: a retained seed site's record of WHY a
//! baked rule stays baked instead of being learned from upstream rejections.
//!
//! A child of the `TRANSLATION-DROP:` parser so it reuses that parser's
//! comment, anchor and forbidden-content rules rather than re-deriving them.
//! Nothing here is called from the drop census or its welds. One shared change
//! reaches the drop parse: the comment reader takes the `!` of a `//!` line as
//! part of the prefix in both grammars, so a drop reason wrapped across inner
//! doc lines reads without it.
//!
//! ```text
//! // SEED-VERDICT: <tag> -- <self-contained reason>
//! ```
//!
//! A comment BLOCK is a run of non-blank comment lines: a blank `//`, `///`
//! or `//!` line ends one, as does any line that is not a comment. A long
//! reason wraps onto the lines below the marker and ends where its block
//! ends, or at the next `SEED-VERDICT:` / `TRANSLATION-DROP:` line. Past that
//! point, up to the item the marker anchors, only blank comment lines and
//! other markers' paragraphs may follow: the marker is the last prose
//! paragraph of its comment, so nothing a reader takes for its reason sits
//! outside the reason the census pins.
//!
//! A seed line directly below a prose `TRANSLATION-DROP:` marker in the same
//! block would be read as part of that marker's reason, so it is refused:
//! the seed line goes above the drop marker, or a blank comment line
//! separates them.
//!
//! The refusal of file paths, line numbers and planning ids reads the whole
//! contiguous comment below the marker, blank comment lines included, up to
//! the next marker.
//!
//! The population is not confined to the drop census's surfaces: the sweep
//! walks the providers and router `src` trees recursively.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use super::{
    MARKER_TOKEN, anchors_code, comment_text, is_test_file, normalize, parse_file, parse_verdict,
    reject_forbidden_content,
};

/// The token that opens every seed marker.
pub const SEED_TOKEN: &str = "SEED-VERDICT:";

/// Test modules whose names the `_tests` / `_test_support` naming derivation
/// does not recognize, as `(crate, src-relative file)`. Each is an outline
/// module declared under a `#[cfg(...)]` whose predicate holds a bare `test`
/// atom, which the census derives and checks against this list.
pub const UNNAMED_TEST_FILES: &[(&str, &str)] = &[
    (PROVIDERS, "alloc_probe.rs"),
    (ROUTER, "alloc_probe.rs"),
    (ROUTER, "config/tests.rs"),
    (ROUTER, "probe_scheduler/test_support.rs"),
    (ROUTER, "router/tests.rs"),
    (ROUTER, "test_secret.rs"),
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

/// The maximal run of lines around `index` that satisfy `in_run`, or `None`
/// when `index` itself does not.
fn bounds_where(
    lines: &[&str],
    index: usize,
    in_run: impl Fn(&str) -> bool,
) -> Option<Range<usize>> {
    if index >= lines.len() || !in_run(lines[index]) {
        return None;
    }
    let start = (0..index)
        .rev()
        .take_while(|&i| in_run(lines[i]))
        .last()
        .unwrap_or(index);
    let end = (index + 1..lines.len())
        .find(|&i| !in_run(lines[i]))
        .unwrap_or(lines.len());
    Some(start..end)
}

/// The comment block holding `index`: the maximal run of non-blank comment
/// lines around it. A blank comment line and a non-comment line both end a
/// block. `None` when `index` is itself blank or not a comment.
pub fn comment_block_bounds(lines: &[&str], index: usize) -> Option<Range<usize>> {
    bounds_where(lines, index, |line| {
        comment_text(line).is_some_and(|text| !text.is_empty())
    })
}

/// The whole comment holding `index`: every contiguous comment line around
/// it, blank comment lines included, so a doc comment's paragraphs are one
/// comment. Only a non-comment line ends it.
pub fn comment_run_bounds(lines: &[&str], index: usize) -> Option<Range<usize>> {
    bounds_where(lines, index, |line| comment_text(line).is_some())
}

/// The line just past the marker's reason: its block's end, or an earlier
/// marker line inside the block.
fn reason_end(lines: &[&str], block: &Range<usize>, marker_line: usize) -> usize {
    (marker_line + 1..block.end)
        .find(|&i| comment_text(lines[i]).is_some_and(is_marker_line))
        .unwrap_or(block.end)
}

fn seed_continuation(lines: &[&str], marker_line: usize, end: usize) -> String {
    lines[marker_line + 1..end]
        .iter()
        .filter_map(|line| comment_text(line))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The contiguous comment below the marker, blank comment lines included, up
/// to the next marker: what the forbidden-content scan reads.
fn seed_block_below(lines: &[&str], marker_line: usize) -> String {
    let run_end = comment_run_bounds(lines, marker_line).map_or(marker_line + 1, |run| run.end);
    lines[marker_line + 1..run_end]
        .iter()
        .filter_map(|line| comment_text(line))
        .take_while(|text| !is_marker_line(text))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Refuse a seed line sitting below a prose drop marker in the same block,
/// where that marker's reason would absorb it. A counted drop marker keeps no
/// reason, and a seed line above stacks independently.
fn refuse_absorption_into_a_drop_reason(
    lines: &[&str],
    block: &Range<usize>,
    index: usize,
    where_: &str,
) -> Result<(), String> {
    let Some(above) = (block.start..index)
        .rev()
        .find(|&i| comment_text(lines[i]).is_some_and(is_marker_line))
    else {
        return Ok(());
    };
    let Some(drop) = comment_text(lines[above])
        .and_then(|text| text.split_once(MARKER_TOKEN))
        .map(|(_, rest)| rest)
    else {
        return Ok(());
    };
    let counted = drop
        .split_whitespace()
        .next()
        .and_then(|token| parse_verdict(token, where_).ok())
        .is_some_and(|verdict| verdict.is_counted());
    if counted {
        return Ok(());
    }
    Err(format!(
        "{where_} sits directly below a prose {MARKER_TOKEN} marker, whose reason would absorb \
         it; place the SEED line above the TRANSLATION-DROP marker or separate them with a blank \
         comment line"
    ))
}

/// Refuse prose after the marker's reason before the anchored item: past the
/// reason only blank comment lines and other markers' paragraphs may follow.
fn refuse_trailing_prose(lines: &[&str], from: usize, where_: &str) -> Result<(), String> {
    let mut in_marker_paragraph = false;
    for line in &lines[from..] {
        match comment_text(line) {
            None => return Ok(()),
            Some("") => in_marker_paragraph = false,
            Some(text) if is_marker_line(text) => in_marker_paragraph = true,
            Some(_) if in_marker_paragraph => {}
            Some(_) => {
                return Err(format!(
                    "{where_} is followed by prose in the same comment; the marker must be the \
                     last paragraph of its comment, directly above the item it records"
                ));
            }
        }
    }
    Ok(())
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
    let block = comment_block_bounds(lines, index).unwrap_or(index..index + 1);
    refuse_absorption_into_a_drop_reason(lines, &block, index, &where_)?;
    let end = reason_end(lines, &block, index);
    refuse_trailing_prose(lines, end, &where_)?;
    let reason = normalize(&format!(
        "{reason} {}",
        seed_continuation(lines, index, end)
    ));
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

/// The prose phrase every baked rule's justification opens with.
pub const BAKED_PHRASE: &str = "Baked seed verdict";

/// A comment holding the baked-rule phrase, and whether a parsed marker in the
/// same comment records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakedBlock {
    pub krate: String,
    pub file: String,
    /// 1-based first line of the comment, for error attribution only.
    pub line: usize,
    pub covered: bool,
}

/// Every comment in one source holding [`BAKED_PHRASE`], matched ASCII
/// case-insensitively over the comment's text with prefixes stripped and
/// whitespace collapsed, so a phrase wrapped across lines or opening mid
/// sentence still counts. Coverage reads parsed markers, seed and
/// drop alike, never a token grep: a token that does not parse covers nothing.
pub fn baked_blocks_in(krate: &str, file: &str, source: &str) -> Result<Vec<BakedBlock>, String> {
    let lines: Vec<&str> = source.lines().collect();
    let marker_lines: Vec<usize> = parse_seed_file(krate, file, source)?
        .iter()
        .map(|m| m.line - 1)
        .chain(parse_file(file, source)?.iter().map(|m| m.line - 1))
        .collect();
    let phrase = BAKED_PHRASE.to_ascii_lowercase();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some(run) = comment_run_bounds(&lines, index) else {
            index += 1;
            continue;
        };
        let text = normalize(
            &lines[run.clone()]
                .iter()
                .filter_map(|line| comment_text(line))
                .collect::<Vec<_>>()
                .join(" "),
        );
        if text.to_ascii_lowercase().contains(&phrase) {
            blocks.push(BakedBlock {
                krate: krate.to_string(),
                file: file.to_string(),
                line: run.start + 1,
                covered: marker_lines.iter().any(|line| run.contains(line)),
            });
        }
        index = run.end;
    }
    Ok(blocks)
}

/// Every baked-phrase comment over a supplied population. Zero is an error.
pub fn baked_blocks_over(population: &[SweptSource]) -> Result<Vec<BakedBlock>, String> {
    let mut blocks = Vec::new();
    for (krate, file, source) in population {
        blocks.extend(baked_blocks_in(krate, file, source)?);
    }
    if blocks.is_empty() {
        return Err(format!(
            "the sweep found no `{BAKED_PHRASE}` comment; an empty sweep is a failed sweep"
        ));
    }
    Ok(blocks)
}

/// The comments holding the phrase with no marker beside them, as messages.
pub fn uncovered_baked_blocks(blocks: &[BakedBlock]) -> Vec<String> {
    blocks
        .iter()
        .filter(|block| !block.covered)
        .map(|block| {
            format!(
                "{} {}: the comment opening on line {} states a baked seed verdict but holds no \
                 {SEED_TOKEN} or {MARKER_TOKEN} marker",
                block.krate, block.file, block.line
            )
        })
        .collect()
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
    is_test_file(file) || UNNAMED_TEST_FILES.contains(&(krate, file))
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

/// Whether an attribute line is a `#[cfg(...)]` whose predicate holds a bare
/// `test` atom and no `not`. String literals are blanked first, so
/// `feature = "test-utils"` names no atom.
fn is_test_cfg(line: &str) -> bool {
    let Some(predicate) = line
        .strip_prefix("#[cfg(")
        .and_then(|rest| rest.strip_suffix(")]"))
    else {
        return false;
    };
    let unquoted: String = predicate
        .split('"')
        .step_by(2)
        .collect::<Vec<_>>()
        .join(" ");
    let atoms: Vec<&str> = unquoted
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|atom| !atom.is_empty())
        .collect();
    atoms.contains(&"test") && !atoms.contains(&"not")
}

/// The attribute opening on `lines[start]`, joined with the lines it spans up
/// to the one balancing its brackets, and the index just past it.
fn joined_attribute(lines: &[&str], start: usize) -> (String, usize) {
    let is_open = |text: &str| text.matches('[').count() > text.matches(']').count();
    let mut joined = lines[start].to_string();
    let mut next = start + 1;
    while is_open(&joined) && next < lines.len() {
        joined.push_str(lines[next]);
        next += 1;
    }
    (joined, next)
}

/// `decl` with any leading `pub` or `pub(...)` visibility removed.
fn strip_visibility(decl: &str) -> &str {
    let Some(rest) = decl.strip_prefix("pub") else {
        return decl;
    };
    let rest = match rest.strip_prefix('(') {
        Some(inner) => inner.split_once(')').map_or(rest, |(_, after)| after),
        None => rest,
    };
    if rest.starts_with(char::is_whitespace) {
        rest.trim_start()
    } else {
        decl
    }
}

/// The test-gated outline module declarations in one source file, resolved to
/// src-relative paths: a [`is_test_cfg`] attribute (possibly spanning lines),
/// any further attributes with an optional `#[path = "..."]` among them, then
/// `mod name;` under any visibility.
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
        if !line.starts_with("#[cfg(") {
            continue;
        }
        let (cfg, mut next) = joined_attribute(&lines, i);
        if !is_test_cfg(&cfg) {
            continue;
        }
        let mut path_attr = None;
        while next < lines.len() && lines[next].starts_with("#[") {
            if let Some(rest) = lines[next].strip_prefix("#[path = \"") {
                path_attr = rest.split('"').next();
            }
            next = joined_attribute(&lines, next).1;
        }
        let Some(decl) = lines.get(next) else {
            continue;
        };
        let decl = strip_visibility(decl);
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
