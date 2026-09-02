//! The counter side of the translation-drop census: every
//! `record_translation_drop` / `record_translation_policy_action` /
//! `record_translation_lane_seen` call in this crate's production source, with
//! its lane and class resolved to the literals the operator sees in telemetry.
//!
//! The marker side lives in `marker.rs`; the weld between the two, and the
//! reason each harvesting rule exists, is stated in
//! `translation_drop_counter_weld.rs`, this module's first consumer.
//!
//! # Why the harvest is offset-based rather than line-based
//!
//! `rustfmt` wraps a fully-qualified
//! `crate::translation_drop_metrics::record_translation_drop(` call so its
//! arguments land on FOLLOWING lines, and it splits the longer tuples of the
//! `for (fired, class) in [...]` tally table across lines too. A line-scoped
//! scan therefore sees a call with no arguments on three of the four surfaces
//! and reports every one of their classes as marked-but-not-counted. So the
//! arguments are read by matching delimiters over the whole source text, never
//! by reading the rest of the line.
//!
//! # Every unresolved expression is an ERROR, never a skipped call
//!
//! A lane or class this module cannot resolve to a literal is reported as a
//! failure. Skipping it would remove a counted drop from the weld's counter
//! side -- and fewer entries on one side means fewer things to match, which is
//! green by having less to check. That is the failure mode this census exists
//! to refuse, so an unresolvable expression fails loudly and gets a resolver
//! rather than an exemption.

// Each consumer uses the part of the harvest it needs; the rest is dead in that
// compilation unit, which is expected for a shared module.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::marker::{is_test_file, src_root};

/// The counter that owns the registry, its own unit tests, and the three
/// `pub fn` definitions. Excluded from the harvested population: its tests
/// exercise the API with reserved synthetic lanes bound to locals, which
/// resolve to no literal by design.
///
/// The exclusion is one file and it is justified by a CHECK rather than by
/// assertion -- see
/// `the_metrics_module_calls_the_counters_only_from_its_own_tests` in the weld.
pub const METRICS_MODULE: &str = "translation_drop_metrics.rs";

/// The three counters, spelled as they appear at a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Counter {
    Drop,
    PolicyAction,
    LaneSeen,
}

impl Counter {
    pub const fn token(self) -> &'static str {
        match self {
            Self::Drop => "record_translation_drop",
            Self::PolicyAction => "record_translation_policy_action",
            Self::LaneSeen => "record_translation_lane_seen",
        }
    }

    const fn all() -> [Self; 3] {
        [Self::Drop, Self::PolicyAction, Self::LaneSeen]
    }
}

/// One resolved counter call. A tally-table call contributes ONE entry per
/// class in its table, all sharing the call's file and line: the table is the
/// call's argument, so each of its classes is a literal that call can emit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CounterCall {
    /// Path relative to the crate's `src`.
    pub file: String,
    /// 1-based line of the call's opening token, for error attribution.
    pub line: usize,
    pub counter: Counter,
    /// The lane literal, with constants resolved through their definitions.
    pub lane: String,
    /// The class literal. `None` only for [`Counter::LaneSeen`], which takes
    /// no class.
    pub class: Option<String>,
}

// ---------------------------------------------------------------------------
// The population.
// ---------------------------------------------------------------------------

/// Every `.rs` file under the crate's `src`, recursively, sorted and relative
/// to `src`. Recursive rather than flat: a counter call is a call wherever it
/// sits, and a directory this walk did not enter would take its calls out of
/// the weld silently.
pub fn rs_files(root: &Path) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    collect_rs(root, root, &mut files)?;
    if files.is_empty() {
        return Err(format!(
            "{} holds no .rs file; the harvest is looking in the wrong place",
            root.display()
        ));
    }
    files.sort();
    Ok(files)
}

fn collect_rs(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
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
            collect_rs(root, &path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let relative = path
                .strip_prefix(root)
                .map_err(|err| format!("{} is not under the walk root: {err}", path.display()))?;
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// The production source the counter harvest runs over: every `.rs` file under
/// the crate's `src` that is not a test file by name, minus the counter's own
/// module.
///
/// WIDER than the four marked surfaces on purpose. A `record_translation_drop`
/// call added outside them would otherwise be invisible to the weld, which is
/// exactly the shape of gap the census exists to refuse; harvested from the
/// whole crate it surfaces as a counted class no marker claims.
pub fn production_files() -> Result<Vec<String>, String> {
    let files: Vec<String> = rs_files(&src_root())?
        .into_iter()
        .filter(|f| !is_test_file(f) && f != METRICS_MODULE)
        .collect();
    if files.is_empty() {
        return Err("the crate holds no production source to harvest".to_string());
    }
    Ok(files)
}

/// The production population as `(file, source)` pairs.
pub fn population() -> Result<Vec<(String, String)>, String> {
    let mut population = Vec::new();
    for file in production_files()? {
        let source = std::fs::read_to_string(src_root().join(&file))
            .map_err(|err| format!("{file} must be readable ({err})"))?;
        population.push((file, source));
    }
    Ok(population)
}

// ---------------------------------------------------------------------------
// Finding the calls.
// ---------------------------------------------------------------------------

/// Byte offset of every counter call opening in `source`, with the counter it
/// names.
///
/// A mention that is not a call -- inside a `//` comment, in a `use` item, or
/// the counter's own `fn` definition -- is not harvested. A mention that IS a
/// call shape but whose `(` does not follow on the same line is an error rather
/// than a skipped line: `rustfmt` never separates the two, so the shape means
/// the scan has stopped matching the code it was written against.
/// Whether `offset` sits inside a `use` item. Walks back to the nearest `;` or
/// `}` statement boundary and asks whether the statement opens with `use`, so a
/// rustfmt-wrapped import whose continuation line carries the counter token is
/// recognized as an import rather than read as a call with no arguments.
fn encloses_use_item(scan: &str, offset: usize) -> bool {
    // Only `;` and `}` end a statement. `{` must NOT be a boundary: an import's
    // own brace group (`use path::{a, b}`) would then be the nearest one and the
    // walk-back would start after the `use` it is looking for.
    let boundary = scan[..offset].rfind([';', '}']).map_or(0, |idx| idx + 1);
    scan[boundary..offset]
        .lines()
        .map(str::trim_start)
        .find(|line| !line.is_empty())
        .is_some_and(|first| first.starts_with("use "))
}

pub fn call_openings(file: &str, source: &str) -> Result<Vec<(Counter, usize)>, String> {
    let mut found = Vec::new();
    // Scan CODE only. A token inside a comment or a string is not a call, and a
    // string literal containing `//` must not make the real call beside it look
    // commented out -- the bug this replaced silently dropped such a call from
    // the counter side, which is green by having less to match.
    let scan = code_only(source);
    let scan = scan.as_str();
    for counter in Counter::all() {
        let token = counter.token();
        for (offset, _) in scan.match_indices(token) {
            // A longer token containing this one (`record_translation_drop`
            // inside nothing today, but the guard keeps the three tokens from
            // aliasing as they are renamed) is not this counter.
            let tail = &scan[offset + token.len()..];
            if tail.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let line_start = scan[..offset].rfind('\n').map_or(0, |nl| nl + 1);
            let line_end = scan[offset..]
                .find('\n')
                .map_or(scan.len(), |nl| offset + nl);
            let line = &source[line_start..line_end];
            // A `use` item importing the counter, and a `fn` DEFINING it, are
            // not calls. The `use` test walks back over the whole item rather
            // than testing one line: rustfmt wraps these imports at 100 cols and
            // three sit at 93 today, so one added name would put the token on a
            // continuation line and a single-line test would hard-error the
            // whole harvest on legal formatting.
            if encloses_use_item(scan, offset)
                || scan[line_start..offset].trim_end().ends_with("fn")
            {
                continue;
            }
            if !tail.trim_start_matches([' ', '\t']).starts_with('(') {
                return Err(format!(
                    "{file} names {token} in code with no argument list on the same line \
                     ({line:?}); the harvest reads the arguments from that `(` onward, so this \
                     shape means it would read none"
                ));
            }
            found.push((counter, offset + token.len()));
        }
    }
    found.sort_by_key(|(_, offset)| *offset);
    Ok(found)
}

/// A byte-offset classification of the whole source: `true` where the byte is
/// CODE, `false` where it sits inside a line comment, a block comment, a string
/// literal (raw strings included), or a char literal.
///
/// One shared pass, because every hand-rolled scan below needs the same answer
/// and four separate approximations of it produced four defects: a call skipped
/// because a URL literal on its line contained `//`; a commented-out call
/// harvested as live; a commented table row contributing a phantom class; and a
/// `const` inside a block comment overriding the real one. A scanner over Rust
/// source that does not know where code IS cannot be made correct by adding
/// special cases.
fn code_mask(source: &str) -> Vec<bool> {
    let bytes = source.as_bytes();
    let mut mask = vec![true; bytes.len()];
    let mut i = 0usize;
    let mut block_depth = 0usize;
    while i < bytes.len() {
        if block_depth > 0 {
            if bytes[i..].starts_with(b"*/") {
                block_depth -= 1;
                mask[i] = false;
                mask[i + 1] = false;
                i += 2;
                continue;
            }
            if bytes[i..].starts_with(b"/*") {
                block_depth += 1;
                mask[i] = false;
                mask[i + 1] = false;
                i += 2;
                continue;
            }
            mask[i] = false;
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            block_depth = 1;
            mask[i] = false;
            mask[i + 1] = false;
            i += 2;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                mask[i] = false;
                i += 1;
            }
            continue;
        }
        // A raw string: `r` followed by zero or more `#` then `"`. It has no
        // escapes, and it closes on `"` followed by the same hash count.
        if bytes[i] == b'r' {
            let mut hashes = 0usize;
            let mut j = i + 1;
            while bytes.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if bytes.get(j) == Some(&b'"') {
                let mut k = j + 1;
                loop {
                    if k >= bytes.len() {
                        break;
                    }
                    if bytes[k] == b'"'
                        && bytes[k + 1..]
                            .iter()
                            .take(hashes)
                            .filter(|b| **b == b'#')
                            .count()
                            == hashes
                    {
                        k += 1 + hashes;
                        break;
                    }
                    k += 1;
                }
                for slot in mask.iter_mut().take(k.min(bytes.len())).skip(i) {
                    *slot = false;
                }
                i = k;
                continue;
            }
        }
        if bytes[i] == b'"' {
            let mut k = i + 1;
            while k < bytes.len() {
                if bytes[k] == b'\\' {
                    k += 2;
                    continue;
                }
                if bytes[k] == b'"' {
                    k += 1;
                    break;
                }
                k += 1;
            }
            for slot in mask.iter_mut().take(k.min(bytes.len())).skip(i) {
                *slot = false;
            }
            i = k;
            continue;
        }
        // A char literal, distinguished from a lifetime by the closing quote
        // within three bytes (`'a'`, `'\n'`, `'\\'`). A lifetime (`'a` with no
        // close) is code and must stay code.
        if bytes[i] == b'\'' {
            let escaped = bytes.get(i + 1) == Some(&b'\\');
            let close = if escaped { i + 3 } else { i + 2 };
            if bytes.get(close) == Some(&b'\'') {
                for slot in mask.iter_mut().take(close + 1).skip(i) {
                    *slot = false;
                }
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
    mask
}

/// The source with COMMENTS blanked and string literals PRESERVED, offsets
/// intact. Distinct from [`code_only`], which blanks literals too: the class
/// harvest needs the literals -- they ARE the classes -- while still having to
/// refuse a commented-out table row, which is a phantom class nobody can find a
/// live call for.
pub fn without_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out: Vec<u8> = bytes.to_vec();
    let mut i = 0usize;
    let mut block_depth = 0usize;
    while i < bytes.len() {
        if block_depth > 0 {
            if bytes[i..].starts_with(b"*/") {
                block_depth -= 1;
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                continue;
            }
            if bytes[i..].starts_with(b"/*") {
                block_depth += 1;
            }
            if bytes[i] != b'\n' {
                out[i] = b' ';
            }
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            block_depth = 1;
            out[i] = b' ';
            out[i + 1] = b' ';
            i += 2;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
            continue;
        }
        // Skip over a string literal untouched, so its bytes survive and a `//`
        // inside it never opens a comment.
        if bytes[i] == b'"' {
            let mut k = i + 1;
            while k < bytes.len() {
                if bytes[k] == b'\\' {
                    k += 2;
                    continue;
                }
                if bytes[k] == b'"' {
                    k += 1;
                    break;
                }
                k += 1;
            }
            i = k;
            continue;
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

/// Lane-seen sites per lane. Extracted so the planted-duplicate control runs the
/// SAME tally the real one-site assertion runs -- the control previously asserted
/// only that its fixture produced two calls, so relaxing the real check from
/// `== 1` to `>= 1` left the whole binary green.
pub fn lane_seen_sites(calls: &[CounterCall]) -> BTreeMap<String, Vec<(String, usize)>> {
    let mut sites: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    for call in calls.iter().filter(|c| c.counter == Counter::LaneSeen) {
        sites
            .entry(call.lane.clone())
            .or_default()
            .push((call.file.clone(), call.line));
    }
    sites
}

/// The two vocabularies' same-side overlaps, as a checked verdict rather than
/// as two inline `intersection` calls. Extracted so the control that proves the
/// check can FIRE runs the same code the real assertion does -- the inline
/// version shipped with neither overlap reachable by any test, and neutering
/// both to `Vec::new()` left the whole binary green.
///
/// Returns the marked-side and counted-side overlaps, in that order. Both empty
/// is the healthy state.
pub fn vocabulary_overlaps(
    marked_drop: &std::collections::BTreeSet<String>,
    marked_policy: &std::collections::BTreeSet<String>,
    counted_drop: &std::collections::BTreeSet<String>,
    counted_policy: &std::collections::BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    (
        marked_drop.intersection(marked_policy).cloned().collect(),
        counted_drop.intersection(counted_policy).cloned().collect(),
    )
}

/// The source with every non-code byte blanked to a space, so a scan that has
/// no lexer of its own reads only code. Offsets are PRESERVED, so any offset
/// derived from this text is valid in the original.
pub fn code_only(source: &str) -> String {
    let mask = code_mask(source);
    // BYTE-for-byte, not char-for-char. Blanking a multibyte char to a single
    // space shortens the result and shifts every downstream offset, and three
    // sites slice the ORIGINAL with an offset derived from this text -- so one
    // non-ASCII char in a comment would either panic on a char boundary or
    // silently truncate a class literal. `byte_length_is_preserved` pins this.
    let mut out: Vec<u8> = source.as_bytes().to_vec();
    for (idx, byte) in out.iter_mut().enumerate() {
        if !mask.get(idx).copied().unwrap_or(false) && *byte != b'\n' {
            *byte = b' ';
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

/// The text between a delimiter and its match, starting at the opening
/// delimiter's offset.
fn delimited(source: &str, open_at: usize, open: char, close: char) -> Result<&str, String> {
    let bytes = source.as_bytes();
    if bytes.get(open_at) != Some(&(open as u8)) {
        return Err(format!("expected {open:?} at offset {open_at}"));
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut cursor = open_at;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if in_string {
            if byte == b'\\' {
                cursor += 2;
                continue;
            }
            if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == open as u8 {
            depth += 1;
        } else if byte == close as u8 {
            depth -= 1;
            if depth == 0 {
                return Ok(&source[open_at + 1..cursor]);
            }
        }
        cursor += 1;
    }
    Err(format!(
        "the {open:?} at offset {open_at} is never closed by {close:?}"
    ))
}

/// Split an argument list on its top-level commas.
fn arguments(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut start = 0usize;
    let bytes = args.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if in_string {
            if byte == b'\\' {
                cursor += 2;
                continue;
            }
            if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b',' if depth == 0 => {
                    out.push(args[start..cursor].trim());
                    start = cursor + 1;
                }
                _ => {}
            }
        }
        cursor += 1;
    }
    let last = args[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out.retain(|arg| !arg.is_empty());
    out
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset].matches('\n').count() + 1
}

// ---------------------------------------------------------------------------
// Resolving a class through the tally table.
// ---------------------------------------------------------------------------

/// A `for (<fired>, <class>) in [ ... ] { ... }` tally table: the shape the
/// gemini egress flushes its per-request drop flags through, where the class
/// reaching the counter is a loop BINDING and its literals are in the table.
/// About a third of the counted classes resolve only this way.
struct ClassTable {
    binding: String,
    body: std::ops::Range<usize>,
    classes: Vec<String>,
}

fn class_tables(source: &str) -> Result<Vec<ClassTable>, String> {
    let mut tables = Vec::new();
    // Find the table STRUCTURE over code only, so a commented-out `for (..)`
    // contributes nothing. `code_only` preserves offsets, so every offset below
    // is valid in `source` -- and the class literals are read from `source`,
    // since blanking them would erase the very strings being harvested.
    let scan_owned = code_only(source);
    let scan = scan_owned.as_str();
    for (offset, _) in scan.match_indices("for (") {
        let paren_at = offset + "for ".len();
        let Ok(binding_list) = delimited(scan, paren_at, '(', ')') else {
            continue;
        };
        let bindings = arguments(binding_list);
        let [_fired, class] = bindings.as_slice() else {
            continue;
        };
        // The `in [` follows the binding list IMMEDIATELY, so the search is
        // anchored to what comes right after the `)` rather than scanning
        // forward for the next occurrence anywhere in the file -- an unanchored
        // search would pair a `for (a, b) in some_iter` with a bracket
        // belonging to an unrelated expression far below and read its literals
        // as this loop's class vocabulary.
        let after_bindings = paren_at + binding_list.len() + 2;
        let Some(bracket_at) = scan[after_bindings..]
            .find('[')
            .map(|at| after_bindings + at)
            .filter(|at| scan[after_bindings..*at].trim() == "in")
        else {
            continue;
        };
        let table = delimited(scan, bracket_at, '[', ']')?;
        // The literals must come from the ORIGINAL text: `scan` has them blanked,
        // and they are the very strings being harvested. Offsets are preserved by
        // `code_only`, so this span is valid in `source`. Reading it from `scan`
        // is what made every table class resolve to an empty string.
        let literal_source = without_comments(source);
        let table_span = &literal_source[bracket_at + 1..bracket_at + 1 + table.len()];
        let body_at = bracket_at + table.len() + 2;
        let Some(brace_at) = scan[body_at..]
            .find('{')
            .map(|at| body_at + at)
            .filter(|at| scan[body_at..*at].trim().is_empty())
        else {
            continue;
        };
        let body = delimited(scan, brace_at, '{', '}')?;
        tables.push(ClassTable {
            binding: (*class).to_string(),
            body: brace_at + 1..brace_at + 1 + body.len(),
            classes: string_literals(table_span),
        });
    }
    Ok(tables)
}

/// Every double-quoted literal in `text`. The tally tables carry class
/// literals and nothing else, so no escape handling beyond skipping `\"` is
/// needed.
fn string_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let start = cursor + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'"' {
            end += if bytes[end] == b'\\' { 2 } else { 1 };
        }
        out.push(text[start..end.min(bytes.len())].to_string());
        cursor = end + 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Resolving a lane through its constant's definition.
// ---------------------------------------------------------------------------

/// `&str` constants of the population, keyed by `(file, name)`, with their
/// right-hand side unresolved.
fn str_constants(
    population: &[(String, String)],
) -> Result<BTreeMap<(String, String), String>, String> {
    let mut table: BTreeMap<(String, String), String> = BTreeMap::new();
    for (file, source) in population {
        // Blank comments and literals first: a `const` inside a block comment is
        // not a definition, and silently letting it win over the real one
        // resolves a lane to a WRONG value -- worse than failing, because a
        // wrong pair looks resolved.
        // Pair each blanked line with its ORIGINAL: the blanked one decides
        // whether this is really a definition (a `const` inside a block comment
        // is not), while the value must be read from the original, since
        // blanking erases the very literal being resolved.
        let scanned = code_only(source);
        for (line, original) in scanned.lines().zip(source.lines()) {
            let trimmed = line.trim();
            let Some(rest) = trimmed
                .strip_prefix("const ")
                .or_else(|| trimmed.strip_prefix("pub const "))
                .or_else(|| trimmed.strip_prefix("pub(crate) const "))
            else {
                continue;
            };
            let Some((name, tail)) = rest.split_once(':') else {
                continue;
            };
            let Some((declared, value)) = tail.split_once('=') else {
                continue;
            };
            // `&str` EXACTLY, not "contains str": `&[&str]` is a slice constant
            // and resolves nothing here, and treating it as a candidate produces
            // spurious duplicate-name collisions on ordinary code.
            if declared.trim() != "&'static str" && declared.trim() != "&str" {
                continue;
            }
            // Re-read the value from the original line at the same `=`.
            let value = original
                .split_once('=')
                .map_or(value, |(_, v)| v)
                .trim()
                .trim_end_matches(';')
                .trim();
            let key = (file.clone(), name.trim().to_string());
            // A DUPLICATE name in one file is unresolvable by a line scanner --
            // per-fn-scoped constants sharing a name are ordinary style in this
            // crate. Overwriting silently picks the last one and resolves a call
            // to a possibly wrong literal, so this fails loud instead.
            if let Some(existing) = table.get(&key) {
                if existing != value {
                    return Err(format!(
                        "{file} defines the `&str` constant {:?} more than once, with different \
                         values ({existing:?} and {value:?}); a line scan cannot tell which one a \
                         call resolves through, so this must be disambiguated rather than guessed",
                        key.1
                    ));
                }
                continue;
            }
            table.insert(key, value.to_string());
        }
    }
    Ok(table)
}

/// The file holding `super::` of `file`: its parent module's `mod.rs`.
fn parent_module(file: &str) -> Option<String> {
    let dir = file.rsplit_once('/')?.0;
    Some(format!("{dir}/mod.rs"))
}

/// Resolve a lane expression to its literal by reading the constant's own
/// definition, transitively. Only two of the four denominator sites pass a
/// literal -- the others pass `LANE` and `super::PROVIDER_KIND` -- so a
/// literal-only harvest reads two lanes as having no denominator site at all.
fn resolve_str(
    expr: &str,
    file: &str,
    constants: &BTreeMap<(String, String), String>,
    where_: &str,
) -> Result<String, String> {
    let mut expr = expr.trim().to_string();
    let mut file = file.to_string();
    for _ in 0..8 {
        if let Some(literal) = expr.strip_prefix('"').and_then(|r| r.split('"').next()) {
            return Ok(literal.to_string());
        }
        let (lookup_file, name) = match expr.strip_prefix("super::") {
            Some(name) => (
                parent_module(&file).ok_or_else(|| {
                    format!("{where_} names {expr} but {file} has no parent module")
                })?,
                name.to_string(),
            ),
            None => (file.clone(), expr.clone()),
        };
        if name.contains("::") {
            return Err(format!(
                "{where_} names {expr}, a path shape this resolver does not read; resolve it by \
                 reading the constant's definition rather than assuming its value"
            ));
        }
        let Some(next) = constants.get(&(lookup_file.clone(), name.clone())) else {
            return Err(format!(
                "{where_} names {expr}, and no `&str` constant {name} is defined in \
                 {lookup_file}; an unresolved lane would take this call out of the census"
            ));
        };
        expr = next.clone();
        file = lookup_file;
    }
    Err(format!(
        "{where_} names a constant chain too deep to resolve, ending at {expr}"
    ))
}

// ---------------------------------------------------------------------------
// The harvest.
// ---------------------------------------------------------------------------

/// Harvest every counter call out of a supplied `(file, source)` population, so
/// the weld's controls run the real harvester over a planted tree rather than a
/// second implementation of it.
///
/// An empty result is legal here: a population with no counter call is what a
/// control plants. The census-wide emptiness guard is [`harvest_crate`].
pub fn harvest(population: &[(String, String)]) -> Result<Vec<CounterCall>, String> {
    let constants = str_constants(population)?;
    let mut calls = Vec::new();
    for (file, source) in population {
        let tables = class_tables(source)?;
        for (counter, after_token) in call_openings(file, source)? {
            let open_at = after_token
                + source[after_token..].find('(').ok_or_else(|| {
                    format!("{file} names {} with no argument list", counter.token())
                })?;
            let line = line_of(source, open_at);
            let where_ = format!("the {} call in {file} on line {line}", counter.token());
            let args = arguments(delimited(source, open_at, '(', ')')?);
            let expected = if counter == Counter::LaneSeen { 1 } else { 2 };
            if args.len() != expected {
                return Err(format!(
                    "{where_} passes {} arguments where {expected} are expected; the harvest \
                     cannot tell which is the lane",
                    args.len()
                ));
            }
            let lane = resolve_str(args[0], file, &constants, &where_)?;
            let classes = if counter == Counter::LaneSeen {
                vec![None]
            } else {
                resolve_classes(args[1], &tables, open_at, &where_)?
                    .into_iter()
                    .map(Some)
                    .collect()
            };
            for class in classes {
                calls.push(CounterCall {
                    file: file.clone(),
                    line,
                    counter,
                    lane: lane.clone(),
                    class,
                });
            }
        }
    }
    calls.sort();
    Ok(calls)
}

fn resolve_classes(
    expr: &str,
    tables: &[ClassTable],
    call_at: usize,
    where_: &str,
) -> Result<Vec<String>, String> {
    if let Some(literal) = expr.strip_prefix('"').and_then(|r| r.split('"').next()) {
        return Ok(vec![literal.to_string()]);
    }
    let enclosing: Vec<&ClassTable> = tables
        .iter()
        .filter(|t| t.binding == expr && t.body.contains(&call_at))
        .collect();
    let [table] = enclosing.as_slice() else {
        return Err(format!(
            "{where_} passes the class as {expr}, which is neither a literal nor the binding of \
             an enclosing `for (.., {expr}) in [...]` tally table; an unresolved class would take \
             this call out of the census"
        ));
    };
    if table.classes.is_empty() {
        return Err(format!(
            "{where_} reads its class from a tally table holding no literal"
        ));
    }
    Ok(table.classes.clone())
}

/// The harvest over the real production tree. An empty result, or a counter
/// with no call at all, is a FAILED harvest rather than a tree with nothing to
/// count: every register the weld pins is satisfied by a population of zero.
pub fn harvest_crate() -> Result<Vec<CounterCall>, String> {
    let calls = harvest(&population()?)?;
    for counter in Counter::all() {
        if !calls.iter().any(|c| c.counter == counter) {
            return Err(format!(
                "the harvest recovered no {} call; an empty result is a failed harvest, not a \
                 tree with nothing to count",
                counter.token()
            ));
        }
    }
    Ok(calls)
}

/// The absolute path of the crate's `src`, for the callers that read files the
/// harvested population deliberately excludes.
pub fn src_path(relative: &str) -> PathBuf {
    src_root().join(relative)
}
