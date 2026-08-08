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
/// # Panics
///
/// If `src` contains MORE than one `mod tests {`. The cut would then be
/// ambiguous, and picking either occurrence silently changes how much of the
/// file the caller's guard actually covers. Fail loudly instead: the author of
/// the second module is the right person to decide what the guard should scan.
#[cfg(test)]
pub(super) fn production_source(src: &str) -> &str {
    let occurrences = src.matches("mod tests {").count();
    assert!(
        occurrences <= 1,
        "a self-scanning guard's source has {occurrences} `mod tests {{` openers, so the \
         production cut is ambiguous and the scanned region would silently shrink; decide \
         explicitly what the guard must cover"
    );
    match src.find("mod tests {") {
        Some(idx) => &src[..idx],
        None => src,
    }
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
}
