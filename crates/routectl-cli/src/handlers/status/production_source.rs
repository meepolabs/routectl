//! Test-only helper for the structural guards that scan their OWN source.
//!
//! Several `/status` guards prove a property of a module by `include_str!`-ing
//! it and scanning the PRODUCTION region for a forbidden call. That requires
//! cutting the inline `mod tests { .. }` tail off, and the cut is the part that
//! historically went wrong: keying on the first literal `#[cfg(test)]` silently
//! truncates the scanned region, because `#[cfg(test)]` also decorates
//! test-only items that sit ABOVE the real test module. A guard whose scanned
//! region quietly shrinks is worse than a noisy one -- it stays GREEN while
//! covering less and less.
//!
//! [`production_source`] keys on the module opener instead (the needle
//! `crate::server::serve_tests`'s route-inventory guard already proved correct)
//! and asserts the needle cannot be ambiguous, so a second test module forces
//! whoever adds it to come here rather than silently halving a guard's reach.

/// The production prefix of `src`: everything above its inline
/// `mod tests { .. }` tail.
///
/// A source with NO inline test module is returned whole -- there is nothing to
/// cut and therefore nothing that can truncate. Sidecar test modules
/// (`#[path = "..._tests.rs"] mod tests;`) are that case: their bodies are not
/// in this file's text at all.
///
/// Openers that are not code -- inside a line or block comment (nested
/// included), a string literal (`b`, `c` and raw prefixes included), or a char
/// literal -- are prose or data, not modules, and are ignored for both the
/// count and the cut.
///
/// # Panics
///
/// If `src` contains MORE than one `mod tests {` in code. The cut would then
/// be ambiguous, and picking either occurrence silently changes how much of
/// the file the caller's guard actually covers. Fail loudly instead: the
/// author of the second module is the right person to decide what the guard
/// should scan.
#[cfg(test)]
pub(super) fn production_source(src: &str) -> &str {
    const OPENER: &str = "mod tests {";
    let mask = code_mask(src);
    let openers: Vec<usize> = src
        .match_indices(OPENER)
        .map(|(idx, _)| idx)
        .filter(|&idx| mask[idx..idx + OPENER.len()].iter().all(|&is_code| is_code))
        .collect();
    assert!(
        openers.len() <= 1,
        "a self-scanning guard's source has {} `mod tests {{` openers in code, so the \
         production cut is ambiguous and the scanned region would silently shrink; decide \
         explicitly what the guard must cover",
        openers.len()
    );
    openers.first().map_or(src, |&idx| &src[..idx])
}

/// Per-byte classification of `src`: `true` where the byte is code, `false`
/// inside a comment or a string, raw-string, or char literal.
#[cfg(test)]
fn code_mask(src: &str) -> Vec<bool> {
    let bytes = src.as_bytes();
    let mut mask = vec![true; bytes.len()];
    let mut i = 0;
    while i < bytes.len() {
        let end = non_code_end(bytes, i);
        if end > i {
            mask[i..end].fill(false);
            i = end;
        } else {
            i += 1;
        }
    }
    mask
}

/// End (exclusive) of the comment or literal starting at `i`, or `i` itself
/// when `i` starts code. An unterminated span runs to the end of input.
#[cfg(test)]
fn non_code_end(bytes: &[u8], i: usize) -> usize {
    let rest = &bytes[i..];
    if rest.starts_with(b"//") {
        return bytes[i..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |off| i + off);
    }
    if rest.starts_with(b"/*") {
        return block_comment_end(bytes, i);
    }
    if bytes[i] == b'r'
        && starts_token(bytes, i)
        && let Some(end) = raw_string_end(bytes, i)
    {
        return end;
    }
    if bytes[i] == b'"' {
        return quoted_end(bytes, i);
    }
    if bytes[i] == b'\'' {
        // A char literal closes within three bytes (`'a'`, `'\n'`); a lifetime
        // (`'a` with no close) stays code.
        let close = if bytes.get(i + 1) == Some(&b'\\') {
            i + 3
        } else {
            i + 2
        };
        if bytes.get(close) == Some(&b'\'') {
            return close + 1;
        }
    }
    i
}

/// Whether an `r` at `i` begins a token (so `r"` is a raw-string prefix,
/// not the tail of an identifier like `bar"`), allowing the `br` byte and
/// `cr` C-string prefixes. The plain `b"` / `c"` prefixes need no rule: the
/// prefix byte stays code and the `"` after it opens an ordinary string.
#[cfg(test)]
fn starts_token(bytes: &[u8], i: usize) -> bool {
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    match i.checked_sub(1).map(|p| bytes[p]) {
        None => true,
        Some(b'b' | b'c') => i < 2 || !is_ident(bytes[i - 2]),
        Some(prev) => !is_ident(prev),
    }
}

#[cfg(test)]
fn block_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// End of a raw string `r#*"..."#*` starting at `start`, or `None` when the
/// `r` does not open one.
#[cfg(test)]
fn raw_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let hashes = bytes[start + 1..]
        .iter()
        .take_while(|&&b| b == b'#')
        .count();
    let quote = start + 1 + hashes;
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }
    let mut closer = vec![b'"'];
    closer.resize(1 + hashes, b'#');
    let body = quote + 1;
    Some(
        bytes[body..]
            .windows(closer.len())
            .position(|w| w == closer.as_slice())
            .map_or(bytes.len(), |off| body + off + closer.len()),
    )
}

#[cfg(test)]
fn quoted_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::production_source;

    #[test]
    fn returns_the_prefix_above_a_single_inline_test_module() {
        let src = "fn production() {}\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n}\n";
        assert_eq!(production_source(src), "fn production() {}\n#[cfg(test)]\n");
    }

    /// A source with no inline test module is scanned WHOLE. This is the case
    /// that the old `split("#[cfg(test)]")` shape got most wrong: a test-only
    /// item near the top cut the region to almost nothing.
    #[test]
    fn returns_the_whole_source_when_there_is_no_inline_test_module() {
        let src = "#[cfg(test)]\nfn only_in_tests() {}\nfn production() {}\n";
        assert_eq!(production_source(src), src);
    }

    /// THE durable proof artifact for this whole class: a second test module
    /// makes the cut ambiguous, and the helper refuses rather than quietly
    /// choosing one and shrinking every caller's scanned region.
    #[test]
    #[should_panic(expected = "production cut is ambiguous")]
    fn panics_when_a_second_test_module_makes_the_cut_ambiguous() {
        let src = "\
#[cfg(test)]
mod tests {
    fn a() {}
}
fn production_below_the_first_cut() {}
#[cfg(test)]
mod tests {
    fn b() {}
}
";
        let _ = production_source(src);
    }

    #[test]
    fn ignores_an_opener_spelled_in_a_comment() {
        let src = "\
//! Cuts at the `mod tests {` opener.
fn production() {} // not a `mod tests {` either
/// Mentions `mod tests {` in prose.
#[cfg(test)]
mod tests {
    fn helper() {}
}
";
        let expected = "\
//! Cuts at the `mod tests {` opener.
fn production() {} // not a `mod tests {` either
/// Mentions `mod tests {` in prose.
#[cfg(test)]
";
        assert_eq!(production_source(src), expected);
    }

    #[test]
    fn returns_the_whole_source_when_the_only_opener_is_in_a_comment() {
        let src = "// mod tests { is prose\nfn production() {}\n";
        assert_eq!(production_source(src), src);
    }

    #[test]
    fn ignores_openers_hidden_in_block_comments_including_nested_ones() {
        let src = "\
/* mod tests { */
/* outer /* inner mod tests { */ still comment mod tests { */
fn production() {}
mod tests {
}
";
        assert_eq!(
            production_source(src),
            "/* mod tests { */\n/* outer /* inner mod tests { */ still comment mod tests { */\n\
             fn production() {}\n"
        );
    }

    #[test]
    fn ignores_an_opener_inside_a_string_literal() {
        let src = "const S: &str = \"say \\\" mod tests { \";\nmod tests {\n}\n";
        assert_eq!(
            production_source(src),
            "const S: &str = \"say \\\" mod tests { \";\n"
        );
    }

    #[test]
    fn ignores_openers_inside_raw_string_literals() {
        let src = "\
const A: &str = r\"mod tests {\";
const B: &str = r#\"has \" quote and mod tests {\"#;
mod tests {
}
";
        assert_eq!(
            production_source(src),
            "const A: &str = r\"mod tests {\";\n\
             const B: &str = r#\"has \" quote and mod tests {\"#;\n"
        );
    }

    #[test]
    fn ignores_openers_inside_prefixed_string_literals() {
        let cases = [
            ("raw C string with a quote", "cr#\"quote \" mod tests {\"#"),
            ("C string", "c\"mod tests {\""),
            ("byte string", "b\"mod tests {\""),
            ("raw byte string", "br#\"quote \" mod tests {\"#"),
        ];
        for (name, literal) in cases {
            let production = format!("const S: &[u8] = {literal};\n");
            let src = format!("{production}mod tests {{\n}}\n");

            assert_eq!(production_source(&src), production, "{name}");
        }
    }

    /// An `r` or `cr` ending an identifier is not a raw-string prefix. Only
    /// the adjacent rows can tell: there a raw reading closes at the escaped
    /// quote and exposes the opener, while the ordinary reading hides it. Those
    /// rows are not legal Rust, which the scanner never relies on.
    #[test]
    fn an_identifier_ending_in_a_prefix_letter_is_not_a_string_prefix() {
        let cases = [
            ("r tail, spaced", "foo_r \"\\\" mod tests {\""),
            ("c tail, spaced", "foo_c \"\\\" mod tests {\""),
            ("r tail, adjacent", "foo_r\"\\\" mod tests {\""),
            ("cr tail, adjacent", "foo_cr\"\\\" mod tests {\""),
            ("br tail, adjacent", "foo_br\"\\\" mod tests {\""),
        ];
        for (name, tokens) in cases {
            let production = format!("m!({tokens});\n");
            let src = format!("{production}mod tests {{\n}}\n");

            assert_eq!(production_source(&src), production, "{name}");
        }
    }

    #[test]
    fn a_quote_char_literal_does_not_open_a_string() {
        let src = "const Q: char = '\"';\nmod tests {\n}\n";
        assert_eq!(production_source(src), "const Q: char = '\"';\n");
    }

    #[test]
    #[should_panic(expected = "production cut is ambiguous")]
    fn panics_on_a_second_real_module_after_hidden_openers() {
        let src = "\
/* mod tests { */
const S: &str = \"mod tests {\";
const R: &str = r#\"mod tests {\"#;
mod tests {
}
mod tests {
}
";
        let _ = production_source(src);
    }

    #[test]
    #[should_panic(expected = "production cut is ambiguous")]
    fn panics_on_a_second_module_even_when_a_comment_also_names_the_opener() {
        let src = "\
// mod tests { prose
mod tests {
}
mod tests {
}
";
        let _ = production_source(src);
    }
}
