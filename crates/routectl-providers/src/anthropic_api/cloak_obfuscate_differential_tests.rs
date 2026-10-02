//! Differential contract of the sensitive-word matcher: the windowed
//! automaton scan must produce byte-identical output to a direct per-position
//! scan, which is kept here as the oracle. Every non-ASCII fixture char is
//! written as a `\u{...}` escape so the source stays ASCII-only.

use super::*;

/// The per-position reference scan: at each original char boundary, walk
/// every word (longest folded first) against the text's own folding, take
/// the first that ends on an original char boundary and covers at least
/// `MIN_MATCH_ORIGINAL_CHARS` original chars, and resume past it. Quadratic
/// in the worst case, which is why it is a test oracle and not the matcher.
struct OracleMatcher {
    words: Vec<OracleWord>,
}

struct OracleWord {
    folded: String,
    first: char,
}

impl OracleMatcher {
    fn build(words: &[String]) -> Option<Self> {
        validate_sensitive_words(words).expect("generated lists are within bounds");
        let mut seen: HashSet<String> = HashSet::new();
        let mut valid: Vec<OracleWord> = Vec::new();
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
                valid.push(OracleWord { folded, first });
            }
        }
        if valid.is_empty() {
            return None;
        }
        valid.sort_by_key(|w| std::cmp::Reverse(w.folded.chars().count()));
        Some(Self { words: valid })
    }

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

    /// The original text of every match the scan marks, in order.
    fn matched_spans<'t>(&self, text: &'t str) -> Vec<&'t str> {
        let mut spans = Vec::new();
        let mut cursor = text.chars();
        while let Some(ch) = cursor.clone().next() {
            let rest = cursor.as_str();
            if let Some(len) = self.match_at(rest, ch) {
                spans.push(&rest[..len]);
                cursor = rest[len..].chars();
            } else {
                cursor.next();
            }
        }
        spans
    }

    /// Whether some kept word's folding is a strict prefix of another's.
    fn has_prefix_related_words(&self) -> bool {
        self.words.iter().any(|short| {
            self.words.iter().any(|long| {
                long.folded.len() > short.folded.len() && long.folded.starts_with(&short.folded)
            })
        })
    }

    fn match_at(&self, rest: &str, first: char) -> Option<usize> {
        let folded_first = fold_char(first).next()?;
        self.words
            .iter()
            .filter(|w| w.first == folded_first)
            .find_map(|w| folded_prefix_len(rest, &w.folded))
    }

    /// Whether some position in `text` has a word whose folding is a prefix
    /// of the text's folding there but is refused (it ends inside one char's
    /// expansion, or covers too few original chars), while a shorter word
    /// at the same position is accepted. Counts the corpus's fallback cases.
    fn exercises_fallback(&self, text: &str) -> bool {
        text.char_indices().any(|(at, _)| {
            let rest = &text[at..];
            let folded_rest: String = rest.chars().flat_map(fold_char).collect();
            let mut refused_longer = false;
            for w in &self.words {
                if !folded_rest.starts_with(&w.folded) {
                    continue;
                }
                if folded_prefix_len(rest, &w.folded).is_some() {
                    return refused_longer;
                }
                refused_longer = true;
            }
            false
        })
    }
}

/// Length in ORIGINAL bytes of the prefix of `text` whose folding equals
/// `needle`, or `None` when the prefix ends inside one char's folding or
/// covers fewer than `MIN_MATCH_ORIGINAL_CHARS` original chars.
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

/// Deterministic xorshift64 generator, so a red case reproduces from its
/// index alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Word material. U+0130 folds to two chars (`i` plus U+0307), so a word
/// ending in `i` can end inside its expansion; the sigma forms fold to one
/// char; U+1E9E folds to the German sharp s; U+212A (Kelvin) folds to `k`;
/// U+023A folds to a wider char; U+4E00 folds to itself.
const WORD_ATOMS: &[&str] = &[
    "a", "b", "i", "k", "s", "\u{130}", "\u{307}", "\u{3A3}", "\u{3C3}", "\u{3C2}", "\u{DF}",
    "\u{1E9E}", "\u{4E00}",
];

/// Text-only material on top of the word atoms: case variants that fold to
/// a word atom, a wider-folding char, and separators.
const TEXT_ATOMS: &[&str] = &["A", "B", "I", "K", "S", "\u{212A}", "\u{23A}", " ", "x"];

fn generate_words(rng: &mut Rng) -> Vec<String> {
    let count = 1 + rng.below(5);
    let mut words: Vec<String> = Vec::with_capacity(count);
    for _ in 0..count {
        let word = match words.last() {
            // A re-cased copy of the previous word: it folds to the same
            // pattern and must be deduplicated, not compiled twice.
            Some(previous) if rng.below(8) == 0 => previous.to_uppercase(),
            // A word at or near the folded-length bound.
            _ if rng.below(10) == 0 => long_word(rng),
            // A word sharing a prefix with the previous one, so overlapping
            // candidates at one start are common.
            // The previous word plus a final `i`: its folding is a prefix of
            // the previous word followed by U+0130, ending inside that
            // char's expansion, so the previous word must win there.
            Some(previous) if rng.below(4) == 0 => format!("{previous}i"),
            Some(previous) if rng.below(2) == 0 => {
                let chars: Vec<char> = previous.chars().collect();
                let keep = 1 + rng.below(chars.len());
                let mut derived: String = chars[..keep].iter().collect();
                for _ in 0..rng.below(3) {
                    derived.push_str(rng.pick(WORD_ATOMS));
                }
                derived
            }
            _ => (0..=rng.below(5)).map(|_| rng.pick(WORD_ATOMS)).collect(),
        };
        words.push(clamp_to_bound(word));
    }
    words
}

/// Drop trailing chars until the word folds within the length bound.
fn clamp_to_bound(mut word: String) -> String {
    while folded_chars(&word) > MAX_SENSITIVE_WORD_FOLDED_CHARS {
        word.pop();
    }
    word
}

/// A word of 28 to 32 folded chars from the word atoms.
fn long_word(rng: &mut Rng) -> String {
    let target = MAX_SENSITIVE_WORD_FOLDED_CHARS - rng.below(5);
    let mut word = String::new();
    loop {
        let atom = rng.pick(WORD_ATOMS);
        let grown = format!("{word}{atom}");
        if folded_chars(&grown) > target {
            return word;
        }
        word = grown;
    }
}

fn folded_chars(word: &str) -> usize {
    word.chars().flat_map(fold_char).count()
}

/// The cased form a word char takes in text: its uppercase, or for `k` and
/// the sharp s the char that folds to it while shrinking in bytes (U+212A
/// and U+1E9E), so contracting folds sit inside matches.
fn recase(rng: &mut Rng, ch: char, text: &mut String) {
    match (ch, rng.below(2)) {
        ('k', 0) => text.push('\u{212A}'),
        ('\u{DF}', 0) => text.push('\u{1E9E}'),
        _ => text.extend(ch.to_uppercase()),
    }
}

/// Text built from word occurrences (some re-cased, some cut short, some
/// ending in U+0130, some back to back) interleaved with single atoms.
fn generate_text(rng: &mut Rng, words: &[String]) -> String {
    let mut text = String::new();
    for _ in 0..rng.below(16) {
        match rng.below(5) {
            0 | 1 => {
                let word = &words[rng.below(words.len())];
                let mut chars: Vec<char> = word.chars().collect();
                if rng.below(4) == 0 {
                    chars.truncate(rng.below(chars.len() + 1));
                }
                for ch in chars {
                    if rng.below(3) == 0 {
                        recase(rng, ch, &mut text);
                    } else {
                        text.push(ch);
                    }
                }
            }
            2 => {
                // A word with its final `i` (or nothing) replaced by U+0130.
                let word = &words[rng.below(words.len())];
                text.push_str(word.strip_suffix('i').unwrap_or(word));
                text.push('\u{130}');
            }
            3 => text.push_str(rng.pick(TEXT_ATOMS)),
            _ => text.push_str(rng.pick(WORD_ATOMS)),
        }
    }
    text
}

/// Chunk sizes for the windowed matcher: one byte (every boundary is a
/// window edge), sizes smaller than one multibyte char, sizes splitting
/// typical words, and the production size (a single window).
const CHUNK_SIZES: &[usize] = &[1, 2, 3, 5, 8, SCAN_CHUNK_BYTES];

const CORPUS_CASES: usize = 4000;

#[derive(Debug, Default, PartialEq, Eq)]
struct CorpusTally {
    comparisons: usize,
    hit_cases: usize,
    fallback_cases: usize,
    expanding_cases: usize,
    contracting_match_cases: usize,
    duplicate_word_cases: usize,
    prefix_related_cases: usize,
    near_bound_word_cases: usize,
}

/// Chars whose folding is narrower in bytes than the char itself.
fn is_contracting(ch: char) -> bool {
    fold_char(ch).map(char::len_utf8).sum::<usize>() < ch.len_utf8()
}

/// Words that pass the length and marker filters, before deduplication.
fn valid_word_count(words: &[String]) -> usize {
    words
        .iter()
        .map(|w| w.trim())
        .filter(|w| w.chars().count() >= MIN_SENSITIVE_WORD_LEN && !w.contains(ZERO_WIDTH_SPACE))
        .count()
}

fn run_corpus(seed: u64) -> CorpusTally {
    let mut rng = Rng(seed);
    let mut tally = CorpusTally::default();
    for case in 0..CORPUS_CASES {
        let words = generate_words(&mut rng);
        let text = generate_text(&mut rng, &words);
        let Some(oracle) = OracleMatcher::build(&words) else {
            for &chunk in CHUNK_SIZES {
                let built = SensitiveWordMatcher::build_with_chunk_bytes(&words, chunk);
                assert!(
                    built.expect("within bounds").is_none(),
                    "case {case}: the oracle found no valid word in {words:?}"
                );
                tally.comparisons += 1;
            }
            continue;
        };
        let expected = oracle.obfuscate(&text);
        tally.hit_cases += usize::from(expected.is_some());
        tally.fallback_cases += usize::from(oracle.exercises_fallback(&text));
        tally.expanding_cases += usize::from(text.contains('\u{130}'));
        tally.contracting_match_cases += usize::from(
            oracle
                .matched_spans(&text)
                .iter()
                .any(|span| span.chars().any(is_contracting)),
        );
        tally.duplicate_word_cases += usize::from(oracle.words.len() < valid_word_count(&words));
        tally.prefix_related_cases += usize::from(oracle.has_prefix_related_words());
        tally.near_bound_word_cases += usize::from(
            oracle
                .words
                .iter()
                .any(|w| w.folded.chars().count() >= MAX_SENSITIVE_WORD_FOLDED_CHARS - 4),
        );
        for &chunk in CHUNK_SIZES {
            let matcher = SensitiveWordMatcher::build_with_chunk_bytes(&words, chunk)
                .expect("within bounds")
                .expect("the oracle found a valid word");
            assert_eq!(
                matcher.obfuscate(&text),
                expected,
                "case {case}, chunk {chunk}: words {words:?}, text {text:?}"
            );
            tally.comparisons += 1;
        }
    }
    tally
}

#[test]
fn the_windowed_scan_matches_the_per_position_oracle_on_a_generated_corpus() {
    // Arrange
    let seed = 0x5EED_CAFE_F00D_0001;

    // Act
    let tally = run_corpus(seed);

    // Assert: every case compared at every chunk size, and the corpus holds
    // enough hits, fallbacks, and expanding folds to have exercised them.
    assert_eq!(tally.comparisons, CORPUS_CASES * CHUNK_SIZES.len());
    assert!(tally.hit_cases >= CORPUS_CASES / 2, "{tally:?}");
    assert!(tally.fallback_cases >= CORPUS_CASES / 10, "{tally:?}");
    assert!(tally.expanding_cases >= CORPUS_CASES / 2, "{tally:?}");
    assert!(
        tally.contracting_match_cases >= CORPUS_CASES / 20,
        "{tally:?}"
    );
    assert!(tally.duplicate_word_cases >= CORPUS_CASES / 20, "{tally:?}");
    assert!(tally.prefix_related_cases >= CORPUS_CASES / 5, "{tally:?}");
    assert!(
        tally.near_bound_word_cases >= CORPUS_CASES / 10,
        "{tally:?}"
    );
}

/// Assert the windowed matcher agrees with the oracle at every chunk size.
fn assert_matches_oracle(words: &[String], text: &str) -> bool {
    let expected = OracleMatcher::build(words).expect("valid").obfuscate(text);
    for &chunk in CHUNK_SIZES {
        let matcher = SensitiveWordMatcher::build_with_chunk_bytes(words, chunk)
            .expect("within bounds")
            .expect("valid words");
        assert_eq!(
            matcher.obfuscate(text),
            expected,
            "chunk {chunk}: words {words:?}, text {text:?}"
        );
    }
    expected.is_some()
}

#[test]
fn a_chain_of_nested_prefix_words_matches_the_oracle() {
    // Arrange: `a` repeated 2..=9 times, alone and with a U+0130-ending
    // variant, against `a`-runs of every length up to 40 separated by `b`
    // or by U+0130 (which can split a run's final match).
    let chain: Vec<String> = (2..=9).map(|n| "a".repeat(n)).collect();
    let mut with_expansion = chain.clone();
    with_expansion.push("aaai".to_string());
    let mut hits = 0usize;
    let mut compared = 0usize;

    // Act
    for words in [&chain, &with_expansion] {
        for run in 0..=40 {
            for separator in ["b", "\u{130}"] {
                let text = format!("{0}{separator}{0}", "a".repeat(run));
                hits += usize::from(assert_matches_oracle(words, &text));
                compared += 1;
            }
        }
    }

    // Assert
    assert_eq!(compared, 2 * 41 * 2);
    assert!(hits >= compared - 2 * 2 * 2, "{hits} of {compared} marked");
}

#[test]
fn a_longer_word_ending_inside_an_expansion_yields_to_the_shorter_word() {
    // Arrange: "abi" matches the folding of "ab\u{130}" only up to the first
    // of U+0130's two folded chars, so it has no original boundary to end
    // on; "ab" at the same start does.
    let words = vec!["abi".to_string(), "ab".to_string()];
    let text = "xab\u{130}y";
    let matcher = SensitiveWordMatcher::build(&words)
        .expect("within bounds")
        .expect("valid words");

    // Act
    let out = matcher.obfuscate(text);

    // Assert
    assert_eq!(out.as_deref(), Some("xa\u{200B}b\u{130}y"));
    assert_eq!(
        out,
        OracleMatcher::build(&words).expect("valid").obfuscate(text)
    );
}

#[test]
fn a_match_straddling_a_window_edge_is_marked_once() {
    // Arrange: two-byte windows put an edge inside every occurrence.
    let words = vec!["secret".to_string()];
    let text = "xxSECRETsecret";
    let matcher = SensitiveWordMatcher::build_with_chunk_bytes(&words, 2)
        .expect("within bounds")
        .expect("valid words");

    // Act
    let out = matcher.obfuscate(text);

    // Assert
    assert_eq!(out.as_deref(), Some("xxS\u{200B}ECRETs\u{200B}ecret"));
}

#[test]
fn every_scan_window_is_bounded_by_chunk_plus_overlap_not_by_text_length() {
    // Arrange: a text far longer than one window, of mixed char widths so
    // rounding to a char boundary is exercised at both window ends.
    let text = "a\u{E9}\u{4E00}\u{1F600}".repeat(64 * 1024);
    let (chunk, overlap) = (1024, 128);
    let rounding = 2 * (MAX_UTF8_CHAR_BYTES - 1);

    // Act
    let mut widest = 0usize;
    let mut windows = 0usize;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let span = window_span(&text, cursor, chunk, overlap);
        widest = widest.max(span.end - cursor);
        windows += 1;
        assert!(span.core_end > cursor, "the scan must advance");
        cursor = span.core_end;
    }

    // Assert: the positive control (many windows) makes the bound real.
    assert!(
        windows > text.len() / (chunk + rounding),
        "{windows} windows"
    );
    assert!(
        widest <= chunk + overlap + rounding,
        "a window of {widest} bytes exceeds the bound"
    );
}

#[test]
fn the_lookahead_covers_the_longest_word_at_the_widest_original_chars() {
    // Arrange: a word of four-byte chars at the folded-length bound is the
    // widest original span a match can cover; one-byte windows force every
    // such match to straddle an edge.
    let word = "\u{1F600}".repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS);
    let text = format!("x{word}y");
    let words = vec![word.clone()];
    let matcher = SensitiveWordMatcher::build_with_chunk_bytes(&words, 1)
        .expect("within bounds")
        .expect("valid words");

    // Act
    let out = matcher.obfuscate(&text);

    // Assert
    let rest: String = word.chars().skip(1).collect();
    assert_eq!(out, Some(format!("x\u{1F600}\u{200B}{rest}y")));
}
