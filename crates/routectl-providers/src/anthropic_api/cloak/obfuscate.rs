//! Inserts zero-width spaces into configured sensitive words in the body.

use std::collections::HashSet;

use serde_json::Value;

/// Zero-width space (U+200B) inserted after the first character of each
/// `sensitive_words` match. Represented as a Rust escape (never a literal
/// non-ASCII byte in source) per the repo's ASCII-only rule. Invisible to
/// the model, so no reverse mapping is needed on the response.
pub(super) const ZERO_WIDTH_SPACE: char = '\u{200B}';

/// Minimum length (in chars) a configured sensitive word must have to be
/// obfuscated. Mirrors CLIProxyAPI's matcher: words shorter than this are
/// dropped to avoid pathological single-letter rewrites.
const MIN_SENSITIVE_WORD_LEN: usize = 2;

/// Obfuscate each configured sensitive word in the outgoing body by
/// inserting a zero-width space (U+200B) after the first character of each
/// match. Mirrors CLIProxyAPI's `ObfuscateSensitiveWords`: matches are
/// case-insensitive and longest-match-first; obfuscation is applied to
/// `system` (string and array-of-text-blocks forms) and `messages[]`
/// content text (string and array-of-text-blocks forms). The inserted
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
    /// (original-cased word, lowercased word), sorted longest-first by
    /// char count so an overlap prefers the longest match.
    words: Vec<(String, String)>,
}

impl SensitiveWordMatcher {
    fn build(words: &[String]) -> Option<Self> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut valid: Vec<(String, String)> = Vec::new();
        for w in words {
            let trimmed = w.trim();
            if trimmed.chars().count() < MIN_SENSITIVE_WORD_LEN
                || trimmed.contains(ZERO_WIDTH_SPACE)
            {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if seen.insert(lower.clone()) {
                valid.push((trimmed.to_string(), lower));
            }
        }
        if valid.is_empty() {
            return None;
        }
        // Longest-first (by char count) so the scan prefers the longest
        // overlapping match, matching CLIProxyAPI's sort-by-length.
        valid.sort_by_key(|w| std::cmp::Reverse(w.0.chars().count()));
        Some(Self { words: valid })
    }

    /// Return the obfuscated form of `text`, or `None` when no match was
    /// found (so callers can skip the write and keep bytes identical).
    /// Scans left-to-right; at each position the longest configured word
    /// that matches case-insensitively (anchored at that byte offset) is
    /// obfuscated, then the scan resumes past the match.
    fn obfuscate(&self, text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        // `to_lowercase` can change byte length for some scripts; guard by
        // only using the lowercased copy for ASCII-safe matching. The
        // configured words and the haystack are compared on their
        // lowercased forms but we splice from the ORIGINAL `text` by byte
        // offset, so a length divergence between `text` and `lower` would
        // corrupt offsets. Fall back to no-op when the lengths diverge.
        if lower.len() != text.len() {
            return self.obfuscate_charwise(text);
        }
        let bytes = text.as_bytes();
        let lower_bytes = lower.as_bytes();
        let mut out = String::with_capacity(text.len() + 8);
        let mut i = 0usize;
        let mut hit = false;
        while i < bytes.len() {
            if let Some(w) = self.match_at(lower_bytes, i) {
                let matched = &text[i..i + w];
                push_obfuscated(&mut out, matched);
                i += w;
                hit = true;
            } else {
                // Advance one full char so we never split a UTF-8 boundary.
                let ch_len = utf8_char_len(bytes[i]);
                out.push_str(&text[i..i + ch_len]);
                i += ch_len;
            }
        }
        if hit { Some(out) } else { None }
    }

    /// Slow path for haystacks whose lowercased byte length diverges from
    /// the original (rare; non-ASCII case folding). Matches on a fully
    /// lowercased char view and rebuilds from the original chars.
    ///
    /// Graceful degradation (intentional, documented): when per-char
    /// lowercasing also changes the CHAR count (a configured sensitive word
    /// whose lowercase form is a different length, e.g. certain non-ASCII
    /// scripts), this returns `None` -- that word is silently NOT obfuscated
    /// rather than risk corrupting the body by splicing at a mismatched
    /// offset. Sensitive-word obfuscation is best-effort hardening, not a
    /// correctness-critical transform, so skipping such a word is preferred
    /// over a malformed payload.
    fn obfuscate_charwise(&self, text: &str) -> Option<String> {
        let orig: Vec<char> = text.chars().collect();
        let lower: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
        // When per-char lowering changed the char count, give up rather
        // than risk corrupting the body. Sensitive-word obfuscation is a
        // best-effort hardening, not a correctness-critical transform.
        if lower.len() != orig.len() {
            return None;
        }
        let mut out = String::with_capacity(text.len() + 8);
        let mut i = 0usize;
        let mut hit = false;
        while i < orig.len() {
            if let Some(n) = self.match_at_chars(&lower, i) {
                let matched: String = orig[i..i + n].iter().collect();
                push_obfuscated(&mut out, &matched);
                i += n;
                hit = true;
            } else {
                out.push(orig[i]);
                i += 1;
            }
        }
        if hit { Some(out) } else { None }
    }

    /// Return the byte length of the longest configured word that matches
    /// `lower_bytes` anchored at byte offset `i`, or `None`.
    fn match_at(&self, lower_bytes: &[u8], i: usize) -> Option<usize> {
        for (_, lw) in &self.words {
            let lwb = lw.as_bytes();
            if i + lwb.len() <= lower_bytes.len() && &lower_bytes[i..i + lwb.len()] == lwb {
                return Some(lwb.len());
            }
        }
        None
    }

    /// Char-view variant of `match_at`: return the char count of the
    /// longest configured word matching `lower` anchored at char index `i`.
    fn match_at_chars(&self, lower: &[char], i: usize) -> Option<usize> {
        for (_, lw) in &self.words {
            let lwc: Vec<char> = lw.chars().collect();
            if i + lwc.len() <= lower.len() && lower[i..i + lwc.len()] == lwc[..] {
                return Some(lwc.len());
            }
        }
        None
    }
}

/// Append `matched` to `out` with a zero-width space inserted after its
/// first character. A single-char match is left unchanged (no interior
/// position to mark), matching CLIProxyAPI's `size >= len` guard.
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

/// Length in bytes of the UTF-8 char beginning with `b`.
const fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
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
