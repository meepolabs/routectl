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

/// Obfuscate each configured sensitive word in the outgoing body by
/// inserting a zero-width space (U+200B) after the first character of each
/// match. Matching is case-insensitive and longest-match-first, so a
/// configured word never shadows a longer configured word that starts at
/// the same position. Obfuscation is applied to `system` (string and
/// array-of-text-blocks forms) and `messages[]` content text (string and
/// array-of-text-blocks forms). The inserted
/// zero-width space is invisible to the model, so no reverse mapping is
/// needed on the response. An empty word list is a byte-identical no-op.
pub(super) fn obfuscate_sensitive_words(body: &mut Value, words: &[String]) {
    let matcher = match SensitiveWordMatcher::build(words) {
        Some(m) => m,
        None => return,
    };
    obfuscate_system(body, &matcher);
    obfuscate_messages(body, &matcher);
}

/// A normalized, deduplicated, longest-first set of sensitive words for a
/// case-insensitive scan. Words shorter than `MIN_SENSITIVE_WORD_LEN` chars
/// or already containing a zero-width space are dropped at build time;
/// `None` is returned when no valid word remains (the obfuscation no-ops).
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
    fn build(words: &[String]) -> Option<Self> {
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
            return None;
        }
        // Sorted longest-first by FOLDED char count so the linear scan in
        // `match_at` returns the longest word anchored at a position: the
        // first hit wins, so a shorter word that is a prefix of a longer
        // one must never be tried first. The folded form is what the scan
        // compares, so it is what the ordering keys on -- original char
        // count can order two words the opposite way when their foldings
        // expand by different amounts.
        valid.sort_by_key(|w| std::cmp::Reverse(w.folded.chars().count()));
        Some(Self { words: valid })
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
/// of `{type:"text", text:...}` blocks).
fn obfuscate_system(body: &mut Value, matcher: &SensitiveWordMatcher) {
    match body.get_mut("system") {
        Some(Value::String(s)) => {
            if let Some(ob) = matcher.obfuscate(s) {
                *s = ob;
            }
        }
        Some(Value::Array(blocks)) => {
            for block in blocks.iter_mut() {
                obfuscate_text_block(block, matcher);
            }
        }
        _ => {}
    }
}

/// Obfuscate sensitive words in `body["messages"][].content` (string form,
/// or an array of content blocks; only `{type:"text"}` blocks are touched).
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
                    obfuscate_text_block(block, matcher);
                }
            }
            _ => {}
        }
    }
}

/// Obfuscate the `text` field of a `{type:"text"}` content block in place.
/// Blocks of any other type are left untouched.
fn obfuscate_text_block(block: &mut Value, matcher: &SensitiveWordMatcher) {
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return;
    }
    let Some(text) = block.get("text").and_then(Value::as_str) else {
        return;
    };
    if let Some(ob) = matcher.obfuscate(text)
        && let Some(obj) = block.as_object_mut()
    {
        obj.insert("text".into(), Value::String(ob));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(words: &[&str]) -> SensitiveWordMatcher {
        let owned: Vec<String> = words.iter().map(|w| (*w).to_string()).collect();
        SensitiveWordMatcher::build(&owned).expect("words are valid")
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
