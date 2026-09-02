//! The `TRANSLATION-DROP:` marker grammar and the parser that recovers every
//! marker out of the four request-translation surfaces, as a module the census
//! and the welds built on top of it share.
//!
//! The grammar itself, the reason each rule is a parse ERROR rather than a
//! skipped line, the FILE-list exclusion of test code, and the census ceiling
//! are all stated in the module doc of `translation_drop_census.rs`, which is
//! this parser's first consumer and owns the pinned population.
//!
//! Every parse fails LOUDLY. An absent source, an unparseable marker, or a
//! census-wide empty result is an error, never an empty set that satisfies a
//! downstream assertion by classifying nothing.

// Each consumer of this module uses the part of the parse it needs; the rest
// is dead in that compilation unit, which is expected for a shared module.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// The four swept surfaces, relative to the crate's `src`.
pub const SURFACES: &[&str] = &[
    "openai_compat/wire_lift",
    "bedrock/converse",
    "gemini",
    "openai_responses",
];

/// The token that opens every marker. Renaming it in source without updating
/// it here empties the census, which the non-vacuity guards below turn into a
/// loud failure rather than a green run.
pub const MARKER_TOKEN: &str = "TRANSLATION-DROP:";

/// The four lane spellings, each matching its surface's own `PROVIDER_KIND`.
pub const LANES: &[&str] = &[
    "openai-compat",
    "bedrock-converse",
    "gemini",
    "openai-responses",
];

/// The closed tag vocabulary a counted marker may carry beyond its verdict.
const COUNTED_TAGS: &[&str] = &["class", "test", "silent"];

// ---------------------------------------------------------------------------
// The parsed marker.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A counted wire-representability loss on one lane.
    Lane(String),
    /// A counted loss routectl itself chose while the upstream would have
    /// accepted the value. Welds to the policy vocabulary, not the drop one.
    PolicyAction,
    /// No content is lost; the reason says why.
    Structural,
    /// A same-dialect-reachable candidate: a defect to file, not a drop.
    FidelityRisk,
    /// Genuinely unclassified.
    Unresolved,
}

impl Verdict {
    pub const fn is_counted(&self) -> bool {
        matches!(self, Self::Lane(_) | Self::PolicyAction)
    }

    pub const fn label(&self) -> &'static str {
        match self {
            Self::Lane(_) => "lane",
            Self::PolicyAction => "policy-action",
            Self::Structural => "structural",
            Self::FidelityRisk => "fidelity-risk",
            Self::Unresolved => "unresolved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// Path relative to the crate's `src`. No marker carries its own path, so
    /// this is derived from where the marker was found.
    pub file: String,
    /// 1-based line, for error attribution only. Never pinned: it moves.
    pub line: usize,
    pub verdict: Verdict,
    pub class: Option<String>,
    pub test: Option<String>,
    /// Whitespace-normalized reason, for the prose verdicts.
    pub reason: String,
    /// The arm drops with no log and no counter.
    pub silent: bool,
}

// ---------------------------------------------------------------------------
// Reading source.
// ---------------------------------------------------------------------------

pub fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Read a source file, or report why it cannot be read. Absence is an error:
/// these files ARE the census population, so a checkout missing one has
/// nothing to count.
pub fn read_source(relative: &str) -> Result<String, String> {
    std::fs::read_to_string(src_root().join(relative))
        .map_err(|err| format!("{relative} must be readable ({err})"))
}

/// A test file by NAME. These surfaces keep their test modules in sibling
/// files (`#[path = "..."] mod tests;`) to stay under the project's file-length
/// ceiling, and every one of them carries `_tests` or `_test_support` in its
/// name -- including the split forms (`request_tests_parity.rs`), which a bare
/// suffix match would read as production source. The naming shape is the
/// derivation; the pinned list above is what keeps the derivation honest.
pub fn is_test_file(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    name.contains("_tests") || name.contains("_test_support")
}

/// Every `.rs` file in the four surfaces, sorted, relative to `src`.
pub fn surface_files() -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    for surface in SURFACES {
        let dir = src_root().join(surface);
        let entries = std::fs::read_dir(&dir)
            .map_err(|err| format!("{surface} must be a readable directory ({err})"))?;
        let mut found = 0usize;
        for entry in entries {
            let entry = entry.map_err(|err| format!("cannot read an entry of {surface}: {err}"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            // REFUSE a nested directory rather than skipping it. The sweep is
            // flat, so markers inside a subdirectory would be invisible -- and
            // fewer markers on this side means fewer things for the welds to
            // match, which is green by having less to check.
            let kind = entry
                .file_type()
                .map_err(|err| format!("cannot type {surface}/{name}: {err}"))?;
            if kind.is_dir() {
                return Err(format!(
                    "{surface} gained the subdirectory {name}; this sweep is flat and would not \
                     see markers inside it. Widen the traversal, then pin the new files."
                ));
            }
            if Path::new(&name).extension().is_some_and(|ext| ext == "rs") {
                found += 1;
                files.push(format!("{surface}/{name}"));
            }
        }
        if found == 0 {
            return Err(format!(
                "{surface} holds no .rs file; the census is looking in the wrong place"
            ));
        }
    }
    files.sort();
    Ok(files)
}

/// The production files of the four surfaces: everything not on the test-file
/// exclusion list.
pub fn production_files() -> Result<Vec<String>, String> {
    let files: Vec<String> = surface_files()?
        .into_iter()
        .filter(|f| !is_test_file(f))
        .collect();
    if files.is_empty() {
        return Err("the four surfaces hold no production source".to_string());
    }
    Ok(files)
}

// ---------------------------------------------------------------------------
// The parse.
// ---------------------------------------------------------------------------

/// The comment text of a line, or `None` when the line is not a comment. Both
/// `//` and `///` carry markers in the tree.
fn comment_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("//")?;
    Some(rest.trim_start_matches('/').trim())
}

/// The reason continues onto the comment lines directly below the marker: a
/// one-line marker is the grammar, but a long reason wraps, and reading only
/// the first line would call a wrapped reason truncated or empty.
fn continuation(lines: &[&str], marker_line: usize) -> String {
    let mut parts = Vec::new();
    for line in lines.iter().skip(marker_line + 1) {
        let Some(text) = comment_text(line) else {
            break;
        };
        if text.is_empty() || text.contains(MARKER_TOKEN) {
            break;
        }
        parts.push(text.to_string());
    }
    parts.join(" ")
}

/// The WHOLE contiguous comment block below the marker, blank `//` lines
/// included. [`continuation`] stops at the first blank comment line because
/// that is where the reason ends -- but a planning id placed below such a line
/// would then evade every content refusal, so the threat-surface scan reads
/// this instead of the reason.
fn comment_block_below(lines: &[&str], marker_line: usize) -> String {
    let mut parts = Vec::new();
    for line in lines.iter().skip(marker_line + 1) {
        let Some(text) = comment_text(line) else {
            break;
        };
        if text.contains(MARKER_TOKEN) {
            break;
        }
        parts.push(text.to_string());
    }
    parts.join(" ")
}

/// Whether the marker has code IMMEDIATELY below it to describe. A marker with
/// nothing but comments beneath it until a blank line anchors no arm, so
/// whatever it described has moved out from under it -- the analogue of an
/// unclosed block in a sentinel-delimited parse, and the same failure: a
/// declaration whose subject is gone.
///
/// The window stops at the first blank line rather than scanning to EOF. An
/// EOF-wide scan is satisfied by any code anywhere below, so it can only ever
/// fire for a marker in a file's final comment block -- meaning deleting the
/// arm under a mid-file marker would leave it green, which is the whole
/// condition this guard exists to catch.
fn anchors_code(lines: &[&str], marker_line: usize) -> bool {
    // The window allows at most ONE blank line. Two bounds are both wrong:
    // scanning to EOF is satisfied by any code anywhere below, so a deleted arm
    // under a mid-file marker stays green; stopping at the FIRST blank line
    // red-fails a `//` comment block legitimately separated from its arm by one
    // blank line, which is correct code and the kind of false red that gets a
    // refusal loosened.
    let mut blanks = 0usize;
    lines
        .iter()
        .skip(marker_line + 1)
        .take_while(|line| {
            if line.trim().is_empty() {
                blanks += 1;
                return blanks <= 1;
            }
            true
        })
        .any(|line| !line.trim().is_empty() && comment_text(line).is_none())
}

fn is_snake_case(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A `<...>f<digits>.<digits>` run, the board task id shape. Detected by scan
/// rather than by spelling an id out: a literal example of the shape would
/// itself be the leak this refuses.
pub fn holds_task_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b'f' {
            continue;
        }
        if idx > 0 && (bytes[idx - 1].is_ascii_alphanumeric() || bytes[idx - 1] == b'_') {
            continue;
        }
        let mut cursor = idx + 1;
        let digits_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == digits_start || cursor >= bytes.len() || bytes[cursor] != b'.' {
            continue;
        }
        // A digit-run that spells a Rust FLOAT TYPE is prose, not an id:
        // `f32.0` / `f64.5` read as a task id to a naive scan, and these are
        // numeric wire-translation surfaces where that prose is likely. Excluded
        // by the spelling rather than by requiring two digits after the dot,
        // because a real id can carry only one there.
        let width = &text[digits_start..cursor];
        if matches!(width, "16" | "32" | "64" | "128") {
            continue;
        }
        if bytes.get(cursor + 1).is_some_and(u8::is_ascii_digit) {
            return true;
        }
    }
    false
}

/// A line reference in either shape a marker could carry it: the
/// `<file>.rs:<digits>` suffix, or the prose `line <digits>` / `lines <digits>`
/// form. Both rot on the next edit above the arm, so both are refused; prose
/// that says "two lines below" names no line and rides through.
///
/// Note on the colon leg's reachability through [`reject_forbidden_content`]:
/// the bare `.rs` check there fires first, so any `foo.rs:118` is already
/// refused as a file path before reaching here. The leg is kept because it is
/// the honest statement of the rule, and because a caller scanning for a line
/// reference alone (the paired controls do) needs it -- not because it is the
/// gate that catches the shape in production.
pub fn holds_line_number(text: &str) -> bool {
    // The colon form must look like a FILE reference: the token before the colon
    // ends in a source extension. Without that bound the scan refuses ordinary
    // numeric prose -- a JSON fragment (`{"budget_tokens":1024}`), a ratio
    // (`3:1`), a clock time (`12:30`) -- all plausible on surfaces that
    // translate numeric wire fields, and each a red build on a correct marker.
    // A bare `<name>:<digits>` shape cannot be the discriminator, because that
    // is exactly what a ratio is.
    let colon_form = text.split_whitespace().any(|token| {
        let Some((head, tail)) = token.rsplit_once(':') else {
            return false;
        };
        if !std::path::Path::new(head)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        {
            return false;
        }
        let digits = tail.trim_end_matches([',', ';', ')', '.']);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    });
    let words: Vec<&str> = text.split_whitespace().collect();
    let prose_form = words.windows(2).any(|pair| {
        let word = pair[0].trim_matches(|c: char| !c.is_ascii_alphabetic());
        (word.eq_ignore_ascii_case("line") || word.eq_ignore_ascii_case("lines"))
            && pair[1].starts_with(|c: char| c.is_ascii_digit())
    });
    colon_form || prose_form
}

/// The content a marker may never carry, whichever line of it holds the text.
fn reject_forbidden_content(text: &str, where_: &str) -> Result<(), String> {
    if text.contains(".rs") {
        return Err(format!(
            "{where_} carries a file path; the census knows the file it found the marker in, \
             and a path field rots on the next move"
        ));
    }
    if holds_line_number(text) {
        return Err(format!(
            "{where_} carries a line number; a line reference rots on the next edit above it"
        ));
    }
    if holds_task_id(text) {
        return Err(format!(
            "{where_} carries a planning id; state the reason a reader of this repo can check \
             instead of pointing at a board"
        ));
    }
    Ok(())
}

/// The `key=value` tags of a counted marker, and the bare `silent` tag.
fn parse_counted_tags(
    fields: &[&str],
    where_: &str,
) -> Result<(Option<String>, Option<String>, bool), String> {
    let mut class = None;
    let mut test = None;
    let mut silent = false;
    for field in fields {
        if *field == "silent" {
            if silent {
                return Err(format!("{where_} carries `silent` twice"));
            }
            silent = true;
            continue;
        }
        let Some((key, value)) = field.split_once('=') else {
            return Err(format!(
                "{where_} carries the unrecognized tag {field:?}; the tag vocabulary is \
                 closed to {COUNTED_TAGS:?}"
            ));
        };
        if !COUNTED_TAGS.contains(&key) {
            return Err(format!(
                "{where_} carries the unrecognized tag {key:?}; the tag vocabulary is closed \
                 to {COUNTED_TAGS:?}"
            ));
        }
        if !is_snake_case(value) {
            return Err(format!(
                "{where_} declares {key}={value:?}, which is not a snake_case identifier"
            ));
        }
        let slot = match key {
            "class" => &mut class,
            "test" => &mut test,
            // `silent` is a BARE tag. Reaching here means it was spelled as
            // `silent=<value>`, which would otherwise land in `test` -- both
            // fabricating a test name and dropping the flag, so the arm would
            // vanish from the one register no derivation can rebuild.
            _ => {
                return Err(format!(
                    "{where_} spells {key} as a key=value tag; it is a bare tag"
                ));
            }
        };
        if slot.is_some() {
            return Err(format!("{where_} declares {key} twice"));
        }
        *slot = Some(value.to_string());
    }
    Ok((class, test, silent))
}

/// One marker, from its own line plus the reason continuation below it.
fn parse_marker(
    file: &str,
    lines: &[&str],
    index: usize,
    raw_line: &str,
) -> Result<Marker, String> {
    let line_no = index + 1;
    let where_ = format!("the marker in {file} on line {line_no}");

    let Some(comment) = comment_text(raw_line) else {
        return Err(format!(
            "{where_} is not a comment; a verdict has to sit in the doc comment of the arm \
             it describes"
        ));
    };
    if !anchors_code(lines, index) {
        return Err(format!(
            "{where_} anchors no code; the arm it described has moved out from under it"
        ));
    }

    let body = comment
        .split_once(MARKER_TOKEN)
        .map(|(_, rest)| rest.trim())
        .unwrap_or_default();
    if body.is_empty() {
        return Err(format!("{where_} declares no verdict"));
    }
    let continued = continuation(lines, index);

    let fields: Vec<&str> = body.split_whitespace().collect();
    let verdict = parse_verdict(fields[0], &where_)?;
    let fields = &fields[1..];

    if verdict.is_counted() {
        // The ROT checks (`.rs` paths, line numbers) are genuinely line-scoped:
        // a counted verdict discards `continued`, so prose below it is not part
        // of the marker and an ordinary cross-reference there must not red-fail.
        reject_forbidden_content(body, &where_)?;
        // The PLANNING-ID check is NOT line-scoped, because it is a
        // threat-surface refusal rather than a rot refusal. Narrowing it with
        // the rot checks would have silently narrowed a threat-surface rule --
        // and the repo's own scanner misses a one-digit id, so for that shape
        // this census is the only gate. Scan the whole adjacent block.
        if holds_task_id(&format!("{body} {}", comment_block_below(lines, index))) {
            return Err(format!(
                "{where_} carries a planning id; it is meaningless to a reader of this repo \
                 and pins internal planning structure into the code"
            ));
        }
        return parse_counted(file, line_no, verdict, fields, &where_);
    }
    // A prose verdict's continuation BECOMES the pinned reason, so it is part
    // of the marker and is scanned with it.
    reject_forbidden_content(&format!("{body} {continued}"), &where_)?;
    // The reason stops at a blank `//` line; the threat-surface scan does not.
    if holds_task_id(&comment_block_below(lines, index)) {
        return Err(format!(
            "{where_} carries a planning id below its reason; it is meaningless to a reader of \
             this repo and pins internal planning structure into the code"
        ));
    }
    parse_prose(file, line_no, verdict, body, &continued, &where_)
}

/// The verdict token. A `policy-action` marker spelled with a lane is refused
/// in `parse_counted`, where the lane's absence is part of that verdict's
/// grammar, rather than here.
fn parse_verdict(token: &str, where_: &str) -> Result<Verdict, String> {
    if let Some(lane) = token.strip_prefix("lane=") {
        if !LANES.contains(&lane) {
            return Err(format!(
                "{where_} names the lane {lane:?}, which is not one of the four fixed \
                 spellings {LANES:?}"
            ));
        }
        return Ok(Verdict::Lane(lane.to_string()));
    }
    match token {
        "policy-action" => Ok(Verdict::PolicyAction),
        "structural" => Ok(Verdict::Structural),
        "fidelity-risk" => Ok(Verdict::FidelityRisk),
        "unresolved" => Ok(Verdict::Unresolved),
        other => Err(format!(
            "{where_} opens with {other:?}, which is no verdict this grammar knows"
        )),
    }
}

fn parse_counted(
    file: &str,
    line: usize,
    verdict: Verdict,
    fields: &[&str],
    where_: &str,
) -> Result<Marker, String> {
    if verdict == Verdict::PolicyAction && fields.iter().any(|f| f.starts_with("lane=")) {
        return Err(format!(
            "{where_} carries a lane; the policy vocabulary welds on class alone, and a lane \
             on the marker would cover three call sites while implying every hand-typed lane \
             literal was covered"
        ));
    }
    let (class, test, silent) = parse_counted_tags(fields, where_)?;
    let Some(class) = class else {
        return Err(format!(
            "{where_} is a counted verdict with no class; there is no counter literal to weld it \
             against"
        ));
    };
    let Some(test) = test else {
        return Err(format!(
            "{where_} is a counted verdict with no test; nothing pins the drop it declares"
        ));
    };
    Ok(Marker {
        file: file.to_string(),
        line,
        verdict,
        class: Some(class),
        test: Some(test),
        reason: String::new(),
        silent,
    })
}

fn parse_prose(
    file: &str,
    line: usize,
    verdict: Verdict,
    body: &str,
    continued: &str,
    where_: &str,
) -> Result<Marker, String> {
    if body.contains("class=") {
        return Err(format!(
            "{where_} is a `{}` verdict carrying a class; a verdict claiming nothing is lost \
             cannot also name the loss it counts",
            verdict.label()
        ));
    }
    for tag in ["test=", "lane="] {
        if body.contains(tag) {
            return Err(format!(
                "{where_} is a `{}` verdict carrying `{tag}`; the counted tags belong only to a \
                 counted verdict",
                verdict.label()
            ));
        }
    }
    // `silent` is a BARE tag, so it needs a whitespace boundary rather than a
    // substring test: "silently" is the natural word for describing a non-loss
    // and appears throughout these surfaces' prose, so `contains` would refuse
    // a legitimate reason and blame the wrong thing.
    if body.split_whitespace().any(|field| field == "silent") {
        return Err(format!(
            "{where_} is a `{}` verdict carrying `silent`; the counted tags belong only to a \
             counted verdict",
            verdict.label()
        ));
    }
    let Some((before_dashes, reason)) = body.split_once("--") else {
        return Err(format!(
            "{where_} is a `{}` verdict with no `-- <reason>`; the reason IS the verdict's \
             evidence",
            verdict.label()
        ));
    };
    // A prose verdict's shape is exactly `<verdict> -- <reason>`. Anything
    // between the two would otherwise be discarded UNREAD, which is how the
    // retired `pattern:` spelling could survive on the 51 prose markers -- half
    // the population -- while the closed vocabulary caught it only on counted
    // ones. The closed-vocabulary refusal has to cover both halves or its
    // guarantee is only ever half true.
    if let Some(token) = before_dashes.split_whitespace().nth(1) {
        return Err(format!(
            "{where_} carries {token:?} between its verdict and `--`; a prose verdict is exactly \
             `<verdict> -- <reason>`, so a tag there would be discarded unread"
        ));
    }
    let reason = normalize(&format!("{reason} {continued}"));
    if reason.is_empty() {
        return Err(format!(
            "{where_} is a `{}` verdict whose reason is empty",
            verdict.label()
        ));
    }
    Ok(Marker {
        file: file.to_string(),
        line,
        verdict,
        class: None,
        test: None,
        reason,
        silent: false,
    })
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Every marker in one source text. An empty result is legal here: most
/// production files carry no drop arm.
pub fn parse_file(file: &str, source: &str) -> Result<Vec<Marker>, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut markers = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        if raw.contains(MARKER_TOKEN) {
            markers.push(parse_marker(file, &lines, index, raw)?);
        }
    }
    Ok(markers)
}

/// The census over a supplied `(file, source)` population, so the non-vacuity
/// guard is testable without deleting source from the tree.
pub fn census_over(population: &[(String, String)]) -> Result<Vec<Marker>, String> {
    let mut markers = Vec::new();
    for (file, source) in population {
        markers.extend(parse_file(file, source)?);
    }
    if markers.is_empty() {
        return Err(
            "the census recovered no marker; an empty parse is a failed parse, not a tree with \
             nothing to declare"
                .to_string(),
        );
    }
    Ok(markers)
}

/// The census over the real tree.
pub fn census() -> Result<Vec<Marker>, String> {
    let mut population = Vec::new();
    for file in production_files()? {
        let source = read_source(&file)?;
        population.push((file, source));
    }
    census_over(&population)
}

pub fn expect<T>(parsed: Result<T, String>) -> T {
    parsed.unwrap_or_else(|why| panic!("the translation-drop census cannot be evaluated: {why}"))
}

/// A stub source with a marker and something below it to anchor, for the
/// grammar controls.
fn stub(marker_body: &str) -> String {
    format!("// {MARKER_TOKEN} {marker_body}\nfn arm() {{}}\n")
}

pub fn parse_stub(marker_body: &str) -> Result<Vec<Marker>, String> {
    parse_file("surface/arm.rs", &stub(marker_body))
}
