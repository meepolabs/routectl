//! Inserts zero-width spaces into configured sensitive words in the body.

use std::collections::HashSet;

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

// Bounds on the operator's `sensitive_words` list. The scan's worst case is
// linear in (same-initial word count) x (folded word length) x (request text
// length): at every text position whose first char matches, each word
// sharing that first char is walked until it diverges. Request text is
// client-controlled (tool output, fetched pages, pasted files) up to the
// ingress body limit, so the two word-side factors are the ones config can
// cap.
//
// Measured configurations: the documented peer example lists two words of
// 3 and 5 chars ("API", "proxy"); this repo's config tests use two words of
// 5 and 6 chars; no measured live config sets any. 32 entries x 32 folded
// chars is 16x the largest observed count and 5x the longest observed word,
// room for product names and short phrases. 64 x 64 was measured first and
// refused: 2.75-3.5s per 100 KiB of adversarial text, 5x the cost below.
//
// Scan of `a`-only text by `same_initial_words(count, len)` (distinct words
// of len-1 `a`s plus one distinct char, the costliest shape per bound),
// release profile, AMD Ryzen 9 9900X (24 threads, load average 4-5):
//   cargo test -p routectl-providers --release --lib \
//     cloak::obfuscate::tests::sensitive_word_scan_cost_at_the_bounds \
//     -- --ignored --nocapture
//   100 KiB:  2 x 5: 9.2ms   8 x 16: 109ms   16 x 16: 180ms
//             16 x 32: 352ms   32 x 16: 359ms   32 x 32 (bounds): 690ms
//   32 MiB:   "API", "proxy": 670ms   32 x 32 (bounds): 229s
//
// The bounds cap the word-side multiplier; they do not make an at-bound,
// pathological list cheap against a maximum-size body. That needs the
// operator to configure 32 long words sharing an initial and a common
// prefix, which no measured config approaches; making that case cheap as
// well takes a prefix index over the words, not a tighter bound.

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

/// A normalized, deduplicated, longest-first set of sensitive words for a
/// case-insensitive scan. Words shorter than `MIN_SENSITIVE_WORD_LEN` chars
/// or already containing a zero-width space are dropped at build time;
/// `None` is returned when no valid word remains (the obfuscation no-ops).
/// A list outside the bounds is refused rather than built.
struct SensitiveWordMatcher {
    /// Sorted longest-first by folded char count so an overlap prefers the
    /// longest match.
    words: Vec<FoldedWord>,
}

/// One configured sensitive word in the only form the scan uses: its folded
/// char stream, plus that stream's first char cached for the per-position
/// prefilter. The original-cased word is deliberately not retained -- no
/// code path needs it, and keeping it out means it cannot reach a log or a
/// debug rendering.
struct FoldedWord {
    folded: String,
    first: char,
}

impl SensitiveWordMatcher {
    fn build(words: &[String]) -> Result<Option<Self>, SensitiveWordsBoundError> {
        validate_sensitive_words(words)?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut valid: Vec<FoldedWord> = Vec::new();
        for w in words {
            let trimmed = w.trim();
            if trimmed.chars().count() < MIN_SENSITIVE_WORD_LEN
                || trimmed.contains(ZERO_WIDTH_SPACE)
            {
                continue;
            }
            let folded: String = trimmed.chars().flat_map(fold_char).collect();
            let Some(first) = folded.chars().next() else {
                continue;
            };
            if seen.insert(folded.clone()) {
                valid.push(FoldedWord { folded, first });
            }
        }
        if valid.is_empty() {
            return Ok(None);
        }
        // Sorted longest-first by FOLDED char count so the linear scan in
        // `match_at` returns the longest word anchored at a position: the
        // first hit wins, so a shorter word that is a prefix of a longer
        // one must never be tried first. The folded form is what the scan
        // compares, so it is what the ordering keys on -- original char
        // count can order two words the opposite way when their foldings
        // expand by different amounts.
        valid.sort_by_key(|w| std::cmp::Reverse(w.folded.chars().count()));
        Ok(Some(Self { words: valid }))
    }

    /// Return the obfuscated form of `text`, or `None` when no match was
    /// found (so callers can skip the write and keep bytes identical).
    /// Scans left-to-right over `text`'s own chars; at each char boundary
    /// the longest configured word that matches case-insensitively is
    /// obfuscated, then the scan resumes past the match.
    fn obfuscate(&self, text: &str) -> Option<String> {
        let mut out = String::with_capacity(text.len() + 8);
        let mut cursor = text.chars();
        let mut hit = false;
        while let Some(ch) = cursor.clone().next() {
            let rest = cursor.as_str();
            if let Some(len) = self.match_at(rest, ch) {
                let (matched, tail) = rest.split_at(len);
                push_obfuscated(&mut out, matched);
                cursor = tail.chars();
                hit = true;
            } else {
                out.push(ch);
                cursor.next();
            }
        }
        if hit { Some(out) } else { None }
    }

    /// Return the number of ORIGINAL bytes consumed by the longest
    /// configured word matching a prefix of `rest`, or `None`. `first` is
    /// `rest`'s first char, already known to the caller.
    fn match_at(&self, rest: &str, first: char) -> Option<usize> {
        // Folding the position's first char once and comparing it against
        // each word's cached first folded char rejects almost every
        // (position, word) pair without walking the word at all.
        let folded_first = fold_char(first).next()?;
        self.words
            .iter()
            .filter(|w| w.first == folded_first)
            .find_map(|w| folded_prefix_len(rest, &w.folded))
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

/// Length in ORIGINAL bytes of the prefix of `text` whose folding equals
/// `needle`, or `None` when there is no such prefix. `needle` must already
/// be folded through `fold_char`.
///
/// Every returned length is a sum of `char::len_utf8` values taken from
/// `text` itself, so it is always an original char boundary -- offsets are
/// never derived from a separately folded buffer. Two prefixes are reported
/// as no match: one ending part-way through a single original char's
/// folding (no spliceable boundary), and one covering fewer than
/// `MIN_MATCH_ORIGINAL_CHARS` original chars (no interior boundary to mark).
fn folded_prefix_len(text: &str, needle: &str) -> Option<usize> {
    let mut wanted = needle.chars();
    let mut consumed = 0usize;
    let mut original_chars = 0usize;
    for ch in text.chars() {
        for folded in fold_char(ch) {
            if wanted.next() != Some(folded) {
                return None;
            }
        }
        consumed += ch.len_utf8();
        original_chars += 1;
        if wanted.as_str().is_empty() {
            return (original_chars >= MIN_MATCH_ORIGINAL_CHARS).then_some(consumed);
        }
    }
    None
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
        let matcher = SensitiveWordMatcher::build(words)
            .expect("within bounds")
            .expect("valid words");
        let text = "a".repeat(bytes);
        let started = std::time::Instant::now();
        let hit = matcher.obfuscate(&text);
        let elapsed = started.elapsed();
        assert!(hit.is_none(), "the adversarial text holds no whole word");
        elapsed
    }

    /// Scan cost against `a`-only request text: a grid of same-initial list
    /// shapes at a 100 KiB sample, then the realistic list and the at-bound
    /// list at the ingress body limit. Ignored: the at-bound full-body scan
    /// runs for minutes by construction. Run it in release with the command
    /// recorded beside the bound constants.
    #[test]
    #[ignore = "multi-minute scan cost measurement; run explicitly with --ignored"]
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
