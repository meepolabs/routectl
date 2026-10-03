//! Inserts zero-width spaces into configured sensitive words in the body.

use std::collections::HashSet;

use aho_corasick::{AhoCorasick, Input, MatchKind};
use serde_json::Value;

/// Zero-width space (U+200B) inserted after the first character of each
/// `sensitive_words` match. Represented as a Rust escape (never a literal
/// non-ASCII byte in source) per the repo's ASCII-only rule. Invisible to
/// the model, so no reverse mapping is needed on the response.
pub(super) const ZERO_WIDTH_SPACE: char = '\u{200B}';

/// Minimum length (in chars) a configured sensitive word must have to be
/// obfuscated. A single-character word would match nearly every position
/// in the body, so words shorter than this are dropped at build time
/// rather than rewriting the payload into noise.
const MIN_SENSITIVE_WORD_LEN: usize = 2;

/// Minimum number of ORIGINAL chars a match must consume to be reported.
/// The marker goes after the match's first character, so a match covering
/// one original char has no interior original boundary: inserting nothing
/// while reporting a hit would claim the term was broken and return
/// byte-identical text. Such a match is reported as no match instead.
const MIN_MATCH_ORIGINAL_CHARS: usize = 2;

/// Greek small final sigma. Unicode gives U+03A3 a single simple-lowercase
/// mapping (U+03C3), so a term written with the final form would otherwise
/// never match uppercase text however it is cased. Both the needle and the
/// haystack are folded through `fold_char`, which collapses this to
/// `NORMAL_SIGMA` so the two streams cannot disagree.
const FINAL_SIGMA: char = '\u{3C2}';

/// Greek small sigma: the normalized form both sigma variants fold to.
const NORMAL_SIGMA: char = '\u{3C3}';

// Bounds on the operator's `sensitive_words` list. The scan is a
// leftmost-longest automaton search over the folded request text (cost model
// on `SensitiveWordMatcher`): linear in the text length, with a re-read of at
// most one word length at each start a fold expansion makes it refuse. The
// word-side factors set the automaton's size and that re-read, so they are
// the ones config caps; request text is client-controlled (tool output,
// fetched pages, pasted files) up to the ingress body limit.
//
// Measured configurations: the documented peer example lists two words of
// 3 and 5 chars ("API", "proxy"); this repo's config tests use two words of
// 5 and 6 chars; no measured live config sets any. 32 entries x 32 folded
// chars is 16x the largest observed count and 5x the longest observed word,
// room for product names and short phrases.
//
// Release profile, AMD Ryzen 9 9900X (24 threads, load average 1-2):
//   cargo test -p routectl-providers --release --lib -- --ignored --exact \
//     anthropic_api::cloak::obfuscate::tests::sensitive_word_scan_cost_at_the_bounds \
//     --nocapture
//   100 KiB, `a`-only text, `same_initial_words`: every shape 1.1-1.2ms
//   32 MiB:  "API", "proxy" on `a`s: 422ms
//            32 x 32 same-initial words on `a`s (bounds): 378ms
//            prefix chain (`a` x 2..=32) on `a`s, all matches: 562ms
//            nested prefix/suffix words on mixed `a`/`b`: 517ms
//            15 words refused inside every U+0130 on U+0130s: 2.6s
// The last is the re-read bound at its worst: every start's longest match
// ends inside a two-char folding, so each start re-reads up to 31 folded
// chars. It needs an operator list of words ending part-way through U+0130's
// folding AND request text dense in U+0130.

/// Most entries `sensitive_words` may hold. Derivation above.
pub const MAX_SENSITIVE_WORDS: usize = 32;

/// Most chars one `sensitive_words` entry may fold to, after trimming.
/// Counted on the folded stream the scan walks, not on original chars or
/// bytes: a char whose lowercase expands (U+0130 folds to two) costs the
/// scan two steps. Derivation above.
pub const MAX_SENSITIVE_WORD_FOLDED_CHARS: usize = 32;

/// A `sensitive_words` list outside the bounds. Carries only counts and an
/// entry index -- never the configured word, which is operator content -- and
/// its `Display` names only the bound that was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveWordsBoundError {
    /// More than [`MAX_SENSITIVE_WORDS`] entries.
    TooManyEntries {
        /// How many entries the list holds.
        count: usize,
    },
    /// An entry folds to more than [`MAX_SENSITIVE_WORD_FOLDED_CHARS`] chars.
    EntryTooLong {
        /// The zero-based position of the entry in the list.
        index: usize,
        /// How many chars the trimmed entry folds to.
        folded_chars: usize,
    },
}

impl std::fmt::Display for SensitiveWordsBoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyEntries { .. } => {
                write!(f, "sensitive_words: at most {MAX_SENSITIVE_WORDS} entries")
            }
            Self::EntryTooLong { .. } => write!(
                f,
                "each sensitive word must fold to at most {MAX_SENSITIVE_WORD_FOLDED_CHARS} \
                 characters"
            ),
        }
    }
}

impl std::error::Error for SensitiveWordsBoundError {}

/// Check a `sensitive_words` list against [`MAX_SENSITIVE_WORDS`] and
/// [`MAX_SENSITIVE_WORD_FOLDED_CHARS`]. Every entry is measured, including
/// ones the matcher later drops as too short or already marked, so the rule
/// an operator reads is the rule applied.
///
/// # Errors
///
/// Returns [`SensitiveWordsBoundError`] for the first bound exceeded.
pub fn validate_sensitive_words(words: &[String]) -> Result<(), SensitiveWordsBoundError> {
    if words.len() > MAX_SENSITIVE_WORDS {
        return Err(SensitiveWordsBoundError::TooManyEntries { count: words.len() });
    }
    for (index, word) in words.iter().enumerate() {
        let folded_chars = word.trim().chars().flat_map(fold_char).count();
        if folded_chars > MAX_SENSITIVE_WORD_FOLDED_CHARS {
            return Err(SensitiveWordsBoundError::EntryTooLong {
                index,
                folded_chars,
            });
        }
    }
    Ok(())
}

/// Obfuscate each configured sensitive word in the outgoing body by
/// inserting a zero-width space (U+200B) after the first character of each
/// match. Matching is case-insensitive and longest-match-first, so a
/// configured word never shadows a longer configured word that starts at
/// the same position. Obfuscation is applied to `system` (string and
/// array-of-blocks forms) and `messages[]` content (string and
/// array-of-blocks forms): text blocks, and the text a document block
/// carries inline (see `obfuscate_document_block`). The inserted
/// zero-width space is invisible to the model, so no reverse mapping is
/// needed on the response. An empty word list is a byte-identical no-op.
///
/// A list outside the bounds is refused before the body is touched; the
/// caller must then not send the body, since the terms would travel
/// unmarked.
pub(super) fn obfuscate_sensitive_words(
    body: &mut Value,
    words: &[String],
) -> Result<(), SensitiveWordsBoundError> {
    let Some(matcher) = SensitiveWordMatcher::build(words)? else {
        return Ok(());
    };
    obfuscate_system(body, &matcher);
    obfuscate_messages(body, &matcher);
    Ok(())
}

/// A normalized, deduplicated set of sensitive words compiled into one
/// multi-pattern automaton over their folded forms. Words shorter than
/// `MIN_SENSITIVE_WORD_LEN` chars or already containing a zero-width space
/// are dropped at build time; `None` is returned when no valid word remains
/// (the obfuscation no-ops). A list outside the bounds is refused rather
/// than built. The original-cased words are deliberately not retained -- no
/// code path needs them, and keeping them out means they cannot reach a log
/// or a debug rendering.
///
/// Cost. The automaton reports non-overlapping leftmost-longest matches, so
/// one search step either consumes the match it reports or is a refusal.
/// A reported match is refused only when it starts or ends inside one
/// original char's multi-char folding, or covers fewer than
/// `MIN_MATCH_ORIGINAL_CHARS` original chars -- all of which need such a
/// folding (U+0130 is the only char whose simple lowercase is two chars),
/// so refusals happen only at text positions next to one. A refusal costs
/// one pass over `words` (at most `MAX_SENSITIVE_WORDS`, each compared only
/// when its length lands on an original char boundary) and a re-search from
/// the next original char, which re-reads at most one word length of
/// folded text. Worst case: text length x a constant set by the bounds,
/// paid only at expanding chars; elsewhere each text byte is read a
/// constant number of times however the words nest or share prefixes.
struct SensitiveWordMatcher {
    /// Every valid word's folded form, searched with leftmost-longest
    /// semantics.
    automaton: AhoCorasick,
    /// The same folded words, longest first, for the refusal fallback: the
    /// longest word at a start may be refused while a shorter one there
    /// qualifies.
    words: Vec<String>,
    /// Upper bound on the ORIGINAL bytes any single match can span: each
    /// original char folds to at least one char and is at most four bytes,
    /// so a word of `n` folded chars covers at most `4 * n` original bytes.
    /// A window extended this far past its committed core holds every match
    /// that starts inside the core.
    overlap_bytes: usize,
    /// Original bytes committed per window. Bounds the folded copy and its
    /// offset map to `chunk_bytes + overlap_bytes` regardless of body size.
    chunk_bytes: usize,
}

/// Original bytes of request text folded and searched per window.
const SCAN_CHUNK_BYTES: usize = 64 * 1024;

/// Most UTF-8 bytes one char occupies.
const MAX_UTF8_CHAR_BYTES: usize = 4;

impl SensitiveWordMatcher {
    fn build(words: &[String]) -> Result<Option<Self>, SensitiveWordsBoundError> {
        Self::build_with_chunk_bytes(words, SCAN_CHUNK_BYTES)
    }

    fn build_with_chunk_bytes(
        words: &[String],
        chunk_bytes: usize,
    ) -> Result<Option<Self>, SensitiveWordsBoundError> {
        let Some(mut folded) = folded_word_set(words)? else {
            return Ok(None);
        };
        folded.sort_by_key(|w| std::cmp::Reverse(w.len()));
        let longest = folded.iter().map(|w| w.chars().count()).max().unwrap_or(0);
        // Infallible at these sizes: the bounds cap the automaton at 32
        // patterns of at most 32 folded chars.
        let automaton = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&folded)
            .expect("a bounded word list always compiles");
        Ok(Some(Self {
            automaton,
            words: folded,
            overlap_bytes: longest * MAX_UTF8_CHAR_BYTES,
            chunk_bytes: chunk_bytes.max(1),
        }))
    }

    /// Return the obfuscated form of `text`, or `None` when no match was
    /// found (so callers can skip the write and keep bytes identical).
    /// Scans left-to-right over `text`'s own char boundaries; at each one the
    /// longest configured word that matches case-insensitively is
    /// obfuscated, then the scan resumes past the match.
    fn obfuscate(&self, text: &str) -> Option<String> {
        let mut out = String::with_capacity(text.len() + 8);
        let mut hit = false;
        let mut cursor = 0usize;
        while cursor < text.len() {
            let span = window_span(text, cursor, self.chunk_bytes, self.overlap_bytes);
            let window = &text[cursor..span.end];
            cursor += self.scan_window(&mut out, window, span.core_end - cursor, &mut hit);
        }
        if hit { Some(out) } else { None }
    }

    /// Append `window`'s committed region (its first `core_len` bytes) to
    /// `out` with every chosen match marked, and return how many original
    /// bytes were consumed. Only matches STARTING in the committed region are
    /// taken; one is emitted whole even when it runs into the lookahead.
    fn scan_window(
        &self,
        out: &mut String,
        window: &str,
        core_len: usize,
        hit: &mut bool,
    ) -> usize {
        let folded = FoldedWindow::new(window);
        let mut emitted = 0usize;
        let mut search_from = 0usize;
        while let Some(m) = self
            .automaton
            .find(Input::new(&folded.text).span(search_from..folded.text.len()))
        {
            // Leftmost: no match of any kind starts between `search_from`
            // and `m.start()`, so nothing valid is skipped by jumping there.
            let Some(start) = folded.original_at(m.start()) else {
                search_from = folded.next_boundary_after(m.start());
                continue;
            };
            if start >= core_len {
                break;
            }
            match self.longest_valid_end(&folded, window, m.start(), m.end()) {
                Some((folded_end, end)) => {
                    out.push_str(&window[emitted..start]);
                    push_obfuscated(out, &window[start..end]);
                    *hit = true;
                    emitted = end;
                    search_from = folded_end;
                }
                None => search_from = folded.next_boundary_after(m.start()),
            }
        }
        if emitted < core_len {
            out.push_str(&window[emitted..core_len]);
            emitted = core_len;
        }
        emitted
    }

    /// The longest qualifying match starting at folded offset `start`, as
    /// (folded end, original end), given the automaton's longest match there
    /// ends at `longest_end`. A match qualifies when it ends on an original
    /// char boundary (one ending part-way through a char's folding has
    /// nowhere to splice) and covers at least `MIN_MATCH_ORIGINAL_CHARS`
    /// original chars (a shorter one has no interior boundary to mark). When
    /// the longest is refused, every shorter word is tried at the same start,
    /// longest first; each costs a table lookup unless its end lands on an
    /// original boundary.
    fn longest_valid_end(
        &self,
        folded: &FoldedWindow,
        window: &str,
        start: usize,
        longest_end: usize,
    ) -> Option<(usize, usize)> {
        let original_start = folded.original_at(start)?;
        let qualifies = |folded_end: usize| {
            folded
                .original_at(folded_end)
                .filter(|&end| spans_min_original_chars(window, original_start, end))
        };
        if let Some(end) = qualifies(longest_end) {
            return Some((longest_end, end));
        }
        let rest = &folded.text.as_bytes()[start..];
        self.words
            .iter()
            .filter(|w| w.len() < longest_end - start)
            .find_map(|w| {
                let folded_end = start + w.len();
                let end = qualifies(folded_end)?;
                rest.starts_with(w.as_bytes()).then_some((folded_end, end))
            })
    }
}

/// Whether `window[start..end]` (both original char boundaries) holds at
/// least `MIN_MATCH_ORIGINAL_CHARS` chars.
fn spans_min_original_chars(window: &str, start: usize, end: usize) -> bool {
    window[start..end]
        .chars()
        .nth(MIN_MATCH_ORIGINAL_CHARS - 1)
        .is_some()
}

/// Validate `words`, then trim, filter, fold, and deduplicate them into the
/// patterns the automaton is built from. `None` when no word survives.
fn folded_word_set(words: &[String]) -> Result<Option<Vec<String>>, SensitiveWordsBoundError> {
    validate_sensitive_words(words)?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut folded_words: Vec<String> = Vec::new();
    for w in words {
        let trimmed = w.trim();
        if trimmed.chars().count() < MIN_SENSITIVE_WORD_LEN || trimmed.contains(ZERO_WIDTH_SPACE) {
            continue;
        }
        let folded: String = trimmed.chars().flat_map(fold_char).collect();
        if !folded.is_empty() && seen.insert(folded.clone()) {
            folded_words.push(folded);
        }
    }
    Ok((!folded_words.is_empty()).then_some(folded_words))
}

/// One scan window: `[cursor, core_end)` is committed (matches may only
/// START there) and `[core_end, end)` is lookahead so a match starting near
/// the core's end is seen whole. Both ends are original char boundaries.
struct WindowSpan {
    core_end: usize,
    end: usize,
}

/// The window starting at `cursor` (an original char boundary). Its length
/// is at most `chunk_bytes + overlap_bytes` plus two partial chars of
/// rounding up to a boundary, independent of `text.len()`.
const fn window_span(
    text: &str,
    cursor: usize,
    chunk_bytes: usize,
    overlap_bytes: usize,
) -> WindowSpan {
    let core_end = text.ceil_char_boundary(cursor.saturating_add(chunk_bytes));
    let end = text.ceil_char_boundary(core_end.saturating_add(overlap_bytes));
    WindowSpan { core_end, end }
}

/// Marks a folded offset that is not an original char boundary. Never a real
/// offset: windows are bounded far below `u32::MAX` bytes.
const NOT_A_BOUNDARY: u32 = u32::MAX;

/// A window's text folded through `fold_char`, with the map from folded
/// byte offsets back to the original text. Offsets handed back are always
/// original char boundaries: a folded offset that falls inside one original
/// char's expansion maps to nothing.
struct FoldedWindow {
    text: String,
    /// Per folded byte offset (inclusive of the end): the original byte
    /// offset when that folded offset starts an original char's folding (or
    /// is the end of the text), else `NOT_A_BOUNDARY`.
    original: Vec<u32>,
}

impl FoldedWindow {
    fn new(window: &str) -> Self {
        let mut text = String::with_capacity(window.len() + window.len() / 2);
        let mut original: Vec<u32> = Vec::with_capacity(text.capacity() + 1);
        for (offset, ch) in window.char_indices() {
            let folded_from = text.len();
            text.extend(fold_char(ch));
            original.push(offset as u32);
            original.resize(
                original.len() + (text.len() - folded_from - 1),
                NOT_A_BOUNDARY,
            );
        }
        original.push(window.len() as u32);
        Self { text, original }
    }

    /// The first folded offset after `folded_at` that starts an original
    /// char (or is the end of the text).
    fn next_boundary_after(&self, folded_at: usize) -> usize {
        (folded_at + 1..self.original.len())
            .find(|&at| self.original[at] != NOT_A_BOUNDARY)
            .unwrap_or(self.text.len())
    }

    fn original_at(&self, folded_at: usize) -> Option<usize> {
        self.original
            .get(folded_at)
            .filter(|&&offset| offset != NOT_A_BOUNDARY)
            .map(|&offset| offset as usize)
    }
}

/// Simple per-char lowercase folding, with both Greek sigma variants
/// collapsed to `NORMAL_SIGMA`. The SINGLE folding used for configured
/// words and for haystack text alike, so the two streams cannot drift.
/// `str::to_lowercase`'s context-sensitive final-sigma rule is deliberately
/// not used: it depends on a char's position in a word, which a streaming
/// per-position match cannot reproduce.
fn fold_char(ch: char) -> impl Iterator<Item = char> {
    ch.to_lowercase()
        .map(|c| if c == FINAL_SIGMA { NORMAL_SIGMA } else { c })
}

/// Append `matched` to `out` with a zero-width space inserted after its
/// first character. `MIN_MATCH_ORIGINAL_CHARS` prevents a single-char match
/// from reaching this helper; the single-char branch keeps it total on any
/// direct input.
fn push_obfuscated(out: &mut String, matched: &str) {
    let mut chars = matched.chars();
    if let Some(first) = chars.next() {
        let rest = chars.as_str();
        if rest.is_empty() {
            out.push(first);
        } else {
            out.push(first);
            out.push(ZERO_WIDTH_SPACE);
            out.push_str(rest);
        }
    }
}

/// Obfuscate sensitive words in `body["system"]` (string form, or an array
/// of blocks).
fn obfuscate_system(body: &mut Value, matcher: &SensitiveWordMatcher) {
    match body.get_mut("system") {
        Some(Value::String(s)) => {
            if let Some(ob) = matcher.obfuscate(s) {
                *s = ob;
            }
        }
        Some(Value::Array(blocks)) => {
            for block in blocks.iter_mut() {
                obfuscate_content_block(block, matcher);
            }
        }
        _ => {}
    }
}

/// Obfuscate sensitive words in `body["messages"][].content` (string form,
/// or an array of content blocks).
fn obfuscate_messages(body: &mut Value, matcher: &SensitiveWordMatcher) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages.iter_mut() {
        match msg.get_mut("content") {
            Some(Value::String(s)) => {
                if let Some(ob) = matcher.obfuscate(s) {
                    *s = ob;
                }
            }
            Some(Value::Array(blocks)) => {
                for block in blocks.iter_mut() {
                    obfuscate_content_block(block, matcher);
                }
            }
            _ => {}
        }
    }
}

/// Obfuscate one system or message content block in place: a text block's
/// `text`, or a document block's inline text. Blocks of any other type are
/// left untouched.
fn obfuscate_content_block(block: &mut Value, matcher: &SensitiveWordMatcher) {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => obfuscate_text_block(block, matcher),
        Some("document") => obfuscate_document_block(block, matcher),
        _ => {}
    }
}

/// Obfuscate the `text` field of a `{type:"text"}` content block in place.
/// Blocks of any other type are left untouched.
fn obfuscate_text_block(block: &mut Value, matcher: &SensitiveWordMatcher) {
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return;
    }
    obfuscate_string_field(block, "text", matcher);
}

/// Obfuscate the text a document block carries inline in its `source`: the
/// `data` of a `{type:"text"}` source, and a `{type:"content"}` source's
/// `content`, whether a string or an array (only its text blocks).
///
/// A base64, URL, or file source is opaque here -- its bytes are encoded or
/// fetched upstream -- so a sensitive word inside one travels unmarked. The
/// block's `title` and `context` are not scanned.
fn obfuscate_document_block(block: &mut Value, matcher: &SensitiveWordMatcher) {
    let Some(source) = block.get_mut("source") else {
        return;
    };
    match source.get("type").and_then(Value::as_str) {
        Some("text") => obfuscate_string_field(source, "data", matcher),
        Some("content") => match source.get_mut("content") {
            Some(Value::String(s)) => {
                if let Some(ob) = matcher.obfuscate(s) {
                    *s = ob;
                }
            }
            Some(Value::Array(blocks)) => {
                for inner in blocks.iter_mut() {
                    obfuscate_text_block(inner, matcher);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

/// Rewrite the string at `obj[key]` when it holds a match; any other shape,
/// or no match, leaves the object byte-identical.
fn obfuscate_string_field(obj: &mut Value, key: &str, matcher: &SensitiveWordMatcher) {
    let Some(text) = obj.get(key).and_then(Value::as_str) else {
        return;
    };
    if let Some(ob) = matcher.obfuscate(text)
        && let Some(map) = obj.as_object_mut()
    {
        map.insert(key.into(), Value::String(ob));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(words: &[&str]) -> SensitiveWordMatcher {
        let owned: Vec<String> = words.iter().map(|w| (*w).to_string()).collect();
        SensitiveWordMatcher::build(&owned)
            .expect("words are within bounds")
            .expect("words are valid")
    }

    /// A word no test text contains, so an error that echoes a configured
    /// word is caught by searching for it.
    const SENTINEL: &str = "zqxsentinelword";

    fn owned(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    /// `count` distinct words of `len` folded chars sharing the initial
    /// `a`: `len - 1` repeated `a`s, then one CJK char per word (which folds
    /// to itself) so no two words dedupe and each walk diverges only on its
    /// last char against `a`-only text.
    fn same_initial_words(count: usize, len: usize) -> Vec<String> {
        (0..count)
            .map(|i| {
                let last = char::from_u32(0x4E00 + u32::try_from(i).unwrap()).unwrap();
                format!("{}{last}", "a".repeat(len - 1))
            })
            .collect()
    }

    #[test]
    fn a_list_at_both_bounds_is_accepted() {
        // Arrange
        let words = same_initial_words(MAX_SENSITIVE_WORDS, MAX_SENSITIVE_WORD_FOLDED_CHARS);

        // Act
        let result = validate_sensitive_words(&words);
        let built = SensitiveWordMatcher::build(&words);

        // Assert
        assert_eq!(result, Ok(()));
        assert_eq!(
            built.expect("within bounds").map(|m| m.words.len()),
            Some(MAX_SENSITIVE_WORDS)
        );
    }

    #[test]
    fn one_entry_over_the_count_bound_is_refused() {
        // Arrange
        let words = same_initial_words(MAX_SENSITIVE_WORDS + 1, 2);

        // Act
        let result = validate_sensitive_words(&words);

        // Assert
        assert_eq!(
            result,
            Err(SensitiveWordsBoundError::TooManyEntries {
                count: MAX_SENSITIVE_WORDS + 1
            })
        );
    }

    #[test]
    fn one_folded_char_over_the_length_bound_is_refused_with_its_index() {
        // Arrange
        let mut words = owned(&["api", "proxy"]);
        words.push("a".repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS + 1));

        // Act
        let result = validate_sensitive_words(&words);

        // Assert
        assert_eq!(
            result,
            Err(SensitiveWordsBoundError::EntryTooLong {
                index: 2,
                folded_chars: MAX_SENSITIVE_WORD_FOLDED_CHARS + 1
            })
        );
    }

    #[test]
    fn surrounding_whitespace_does_not_count_toward_the_length_bound() {
        // Arrange
        let words = vec![format!(
            "  {}  ",
            "a".repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS)
        )];

        // Act
        let result = validate_sensitive_words(&words);

        // Assert
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn the_length_bound_counts_folded_chars_not_bytes() {
        // Arrange: U+03A3 is two UTF-8 bytes and folds to one char, so this
        // entry is twice the bound in bytes and exactly at it folded.
        let word = "\u{3A3}".repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS);
        assert!(word.len() > MAX_SENSITIVE_WORD_FOLDED_CHARS);

        // Act
        let result = validate_sensitive_words(&[word]);

        // Assert
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn the_length_bound_counts_folded_chars_not_original_chars() {
        // Arrange: U+0130 folds to two chars, so half the bound in original
        // chars plus one is over the bound on the stream the scan walks.
        let original_chars = MAX_SENSITIVE_WORD_FOLDED_CHARS / 2 + 1;
        let word = "\u{130}".repeat(original_chars);

        // Act
        let result = validate_sensitive_words(&[word]);

        // Assert
        assert_eq!(
            result,
            Err(SensitiveWordsBoundError::EntryTooLong {
                index: 0,
                folded_chars: original_chars * 2
            })
        );
    }

    #[test]
    fn refusals_name_the_bound_and_never_the_configured_word() {
        // Arrange
        let too_long = vec![SENTINEL.repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS)];
        let too_many: Vec<String> = (0..=MAX_SENSITIVE_WORDS)
            .map(|i| format!("{SENTINEL}{i}"))
            .collect();

        // Act
        let rendered: Vec<String> = [too_long, too_many]
            .iter()
            .map(|words| validate_sensitive_words(words).unwrap_err().to_string())
            .collect();

        // Assert: the exact messages, so a count, index, or length creeping
        // back in is red.
        assert_eq!(
            rendered,
            [
                format!(
                    "each sensitive word must fold to at most {MAX_SENSITIVE_WORD_FOLDED_CHARS} \
                     characters"
                ),
                format!("sensitive_words: at most {MAX_SENSITIVE_WORDS} entries"),
            ]
        );
        for message in &rendered {
            assert!(!message.contains(SENTINEL), "echoes a word: {message}");
        }
    }

    #[test]
    fn the_matcher_refuses_an_over_bound_list() {
        // Arrange: the list a library caller bypassing config could pass.
        let too_many = same_initial_words(MAX_SENSITIVE_WORDS + 1, 2);
        let too_long = vec!["a".repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS + 1)];

        // Act
        let refused =
            [too_many, too_long].map(|words| SensitiveWordMatcher::build(&words).is_err());

        // Assert
        assert_eq!(refused, [true, true]);
    }

    #[test]
    fn an_over_bound_list_leaves_the_body_untouched() {
        // Arrange
        let mut words = owned(&["secret"]);
        words.push("a".repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS + 1));
        let mut body = serde_json::json!({"system": "the secret"});
        let before = body.clone();

        // Act
        let result = obfuscate_sensitive_words(&mut body, &words);

        // Assert
        assert!(result.is_err());
        assert_eq!(body, before);
    }

    /// Time one scan of `bytes` of `a` and assert it found nothing.
    fn time_scan(words: &[String], bytes: usize) -> std::time::Duration {
        let (elapsed, hit) = time_scan_of(words, &"a".repeat(bytes));
        assert!(!hit, "the adversarial text holds no whole word");
        elapsed
    }

    /// Time one scan of `text`, returning whether it marked anything.
    fn time_scan_of(words: &[String], text: &str) -> (std::time::Duration, bool) {
        let matcher = SensitiveWordMatcher::build(words)
            .expect("within bounds")
            .expect("valid words");
        let started = std::time::Instant::now();
        let hit = matcher.obfuscate(text).is_some();
        (started.elapsed(), hit)
    }

    /// `a` repeated 2..=32 times plus one CJK-terminated word: every
    /// position starts 31 nested words, so an all-`a` text is all matches.
    fn prefix_chain_words() -> Vec<String> {
        let mut words: Vec<String> = (2..=MAX_SENSITIVE_WORD_FOLDED_CHARS)
            .map(|len| "a".repeat(len))
            .collect();
        words.push("a\u{4E00}".to_string());
        words
    }

    /// Words nested in each other both ways: `a`-runs ending in `b` and
    /// `b` followed by `a`-runs, so every word is a suffix or prefix of
    /// another.
    fn nested_words() -> Vec<String> {
        (1..=16)
            .flat_map(|run| {
                let a_run = "a".repeat(run);
                [format!("{a_run}b"), format!("b{a_run}")]
            })
            .collect()
    }

    /// Words of 3, 5, ... 31 folded chars that end on the first half of
    /// U+0130's two-char folding, so against U+0130-only text every start
    /// has 15 candidates and every one is refused.
    fn expansion_refusal_words() -> Vec<String> {
        (1..=15)
            .map(|pairs| format!("{}i", "i\u{307}".repeat(pairs)))
            .collect()
    }

    fn repeat_to(unit: &str, bytes: usize) -> String {
        unit.repeat(bytes / unit.len())
    }

    /// Scan cost against adversarial request text: a grid of same-initial
    /// list shapes on `a`-only text at a 100 KiB sample, then the realistic
    /// list, the at-bound list, and three match-dense shapes (nested
    /// prefixes, nested prefixes and suffixes, refused expansion matches) at
    /// the ingress body limit. Ignored: it is a timing measurement,
    /// meaningful only in release. Run it with the command recorded beside
    /// the bound constants.
    #[test]
    #[ignore = "scan cost measurement, release only; see docs/DEVELOPMENT.md \"Explicit runs\""]
    fn sensitive_word_scan_cost_at_the_bounds() {
        const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
        const SAMPLE_BYTES: usize = 100 * 1024;
        for (count, len) in [(2, 5), (8, 16), (16, 16), (16, 32), (32, 16), (32, 32)] {
            let elapsed = time_scan(&same_initial_words(count, len), SAMPLE_BYTES);
            println!("{count} words x {len} chars: {SAMPLE_BYTES} bytes in {elapsed:?}");
        }
        let realistic = owned(&["API", "proxy"]);
        let at_bounds = same_initial_words(MAX_SENSITIVE_WORDS, MAX_SENSITIVE_WORD_FOLDED_CHARS);
        for (label, words) in [
            ("realistic API, proxy", realistic),
            ("at bounds", at_bounds),
        ] {
            let elapsed = time_scan(&words, MAX_BODY_BYTES);
            println!("{label}: {MAX_BODY_BYTES} bytes in {elapsed:?}");
        }
        let dense = [
            ("prefix chain", prefix_chain_words(), "a", true),
            (
                "nested prefixes and suffixes",
                nested_words(),
                "aaaaaaaaaaaaaaaabaaaaaaab",
                true,
            ),
            (
                "refused expansion matches",
                expansion_refusal_words(),
                "\u{130}",
                false,
            ),
        ];
        for (label, words, unit, marks) in dense {
            let text = repeat_to(unit, MAX_BODY_BYTES);
            let (elapsed, hit) = time_scan_of(&words, &text);
            assert_eq!(hit, marks, "{label}");
            println!("{label}: {} bytes in {elapsed:?}", text.len());
        }
    }

    #[test]
    fn inline_document_text_is_obfuscated_and_opaque_sources_are_not() {
        // Arrange: the sentinel in every document source shape, plus an
        // uppercase non-ASCII variant so the match is not ASCII-only.
        let text_doc = |data: &str| {
            serde_json::json!({"type": "document",
                "source": {"type": "text", "media_type": "text/plain", "data": data}})
        };
        let mut body = serde_json::json!({
            "system": [text_doc("see zqxsentinel here")],
            "messages": [{"role": "user", "content": [
                text_doc("\u{c9}ZQXSENTINEL"),
                {"type": "document", "source": {"type": "content", "content": "zqxsentinel"}},
                {"type": "document", "source": {"type": "content", "content": [
                    {"type": "text", "text": "zqxsentinel"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "zqxsentinel"}},
                ]}},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "zqxsentinel"}},
                {"type": "document", "source": {"type": "url", "url": "https://example.test/zqxsentinel"}},
                {"type": "document", "title": "zqxsentinel", "source": {"type": "file", "file_id": "zqxsentinel"}},
            ]}]
        });
        let words = owned(&["zqxsentinel", "\u{e9}zqxsentinel"]);

        // Act
        obfuscate_sensitive_words(&mut body, &words).expect("within bounds");

        // Assert
        let marked = "z\u{200B}qxsentinel";
        assert_eq!(
            body["system"][0]["source"]["data"],
            format!("see {marked} here")
        );
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["source"]["data"], "\u{c9}\u{200B}ZQXSENTINEL");
        assert_eq!(content[1]["source"]["content"], marked);
        assert_eq!(content[2]["source"]["content"][0]["text"], marked);
        assert_eq!(
            content[2]["source"]["content"][1]["source"]["data"],
            "zqxsentinel"
        );
        assert_eq!(content[3]["source"]["data"], "zqxsentinel");
        assert_eq!(
            content[4]["source"]["url"],
            "https://example.test/zqxsentinel"
        );
        assert_eq!(content[5]["title"], "zqxsentinel");
        assert_eq!(content[5]["source"]["file_id"], "zqxsentinel");
    }

    #[test]
    fn a_document_without_a_match_keeps_its_bytes() {
        // Arrange
        let mut body = serde_json::json!({"system": [{"type": "document",
            "source": {"type": "text", "media_type": "text/plain", "data": ""}}]});
        let before = body.clone();

        // Act
        obfuscate_sensitive_words(&mut body, &owned(&["zqxsentinel"])).expect("within bounds");

        // Assert
        assert_eq!(body, before);
    }

    #[test]
    fn a_one_original_char_expansion_reports_no_hit() {
        // The needle's two folded chars both come from the single original
        // char U+0130. Reporting `Some` here would hand the caller
        // byte-identical text while claiming a rewrite -- the caller would
        // write it back and the term would travel upstream unmarked. Only
        // the `Option` distinguishes the honest answer from the dishonest
        // one, so the assertion has to be made here rather than on output.
        let m = matcher(&["i\u{307}"]);

        let result = m.obfuscate("a \u{130} b");

        assert!(
            result.is_none(),
            "a match with no interior original boundary must not report a hit"
        );
    }

    #[test]
    fn a_two_original_char_match_reports_a_hit() {
        // Positive control: the same needle spread over two original chars
        // has an interior boundary, so the hit is real.
        let m = matcher(&["i\u{307}"]);

        let result = m.obfuscate("a i\u{307} b");

        assert_eq!(
            result.as_deref(),
            Some("a i\u{200B}\u{307} b"),
            "a match spanning two original chars must report a hit"
        );
    }
}

#[cfg(test)]
#[path = "../cloak_obfuscate_differential_tests.rs"]
mod differential_tests;
