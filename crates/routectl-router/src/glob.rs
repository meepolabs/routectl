//! Suffix-glob matcher for the `[aliases]` table.
//!
//! Operators sometimes want a prefix-style alias entry like
//! `"claude-opus-*"` that matches any wire model id starting with
//! `claude-opus-`. The pattern grammar is intentionally narrow:
//!
//!   - exact strings (no wildcards)               -- `"claude-opus-4-7"`
//!   - prefix-followed-by-asterisk                -- `"claude-opus-*"`
//!
//! Anything else is rejected at parse time:
//!
//!   - bare `"*"`                                 (matches everything)
//!   - middle/embedded asterisks                  (`"foo-*-bar"`)
//!   - multiple asterisks                         (`"foo-*-*"`)
//!   - asterisk anywhere except the trailing position
//!
//! Lookup precedence (the caller is expected to enforce this order):
//!
//!   1. exact match on the full key
//!   2. longest-prefix-wins glob match
//!   3. fall through to a `default = "..."` entry, if any
//!
//! Implementation note: glob lookup is over a sorted vector of patterns.
//! For the alias-table sizes routectl realistically deals with (dozens
//! of entries, not thousands) the linear scan is faster and simpler
//! than a trie. The vector is sorted by descending prefix length so
//! the first match is also the longest match.

/// One parsed pattern from the `[aliases]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasPattern {
    /// Match the wire model exactly (no wildcards).
    Exact(String),
    /// Match any wire model that starts with the given prefix.
    /// The asterisk is stored implicitly: stored prefix is the
    /// part BEFORE the trailing `*`.
    Prefix(String),
}

impl AliasPattern {
    /// Parse one operator-supplied alias key. Returns `Err` on any
    /// shape this module doesn't accept. The error message names the
    /// offending pattern verbatim so an operator running
    /// `routectl config check` sees exactly which key to fix.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw == "*" {
            return Err(
                "alias pattern `*` matches every wire model; use `default = \"...\"` instead"
                    .into(),
            );
        }
        let asterisk_count = raw.chars().filter(|c| *c == '*').count();
        match asterisk_count {
            0 => Ok(Self::Exact(raw.to_string())),
            1 if raw.ends_with('*') => {
                let prefix = &raw[..raw.len() - 1];
                if prefix.is_empty() {
                    // Already covered by the bare `*` branch above,
                    // but leave the guard explicit so a future grammar
                    // change can't sneak past.
                    Err(
                        "alias pattern `*` matches every wire model; use `default = \"...\"` instead"
                            .into(),
                    )
                } else {
                    Ok(Self::Prefix(prefix.to_string()))
                }
            }
            _ => Err(format!(
                "alias pattern `{raw}` is not supported; \
                 only exact strings and trailing-asterisk prefixes (e.g. `claude-opus-*`) are accepted"
            )),
        }
    }

    /// True when this pattern matches the given wire model.
    pub fn matches(&self, wire: &str) -> bool {
        match self {
            Self::Exact(s) => s == wire,
            Self::Prefix(p) => wire.starts_with(p),
        }
    }

    /// The prefix length used for longest-match ordering. Exact patterns
    /// are not consulted via this path (the caller does an O(1) lookup
    /// for those first), but the length is well-defined for them too.
    pub const fn prefix_len(&self) -> usize {
        match self {
            Self::Exact(s) | Self::Prefix(s) => s.len(),
        }
    }
}

/// Sorted list of `(pattern, value)` pairs ordered by descending prefix
/// length so the first match in a linear scan is the longest match.
/// Build once at `Router::new` and consult on every dispatch whose
/// wire model didn't hit an exact alias.
#[derive(Debug, Clone, Default)]
pub struct PrefixIndex<V: Clone> {
    /// Sorted by `prefix_len()` descending. Each entry is a
    /// `Prefix`-variant pattern; exact patterns are not stored here
    /// because the caller dispatches them via direct map lookup.
    entries: Vec<(AliasPattern, V)>,
}

impl<V: Clone> PrefixIndex<V> {
    /// An empty prefix index.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Insert a pattern + value. Caller is responsible for not
    /// inserting `Exact` patterns (they should live in a separate
    /// O(1) map). Re-sorts the storage after each push.
    pub fn insert(&mut self, pattern: AliasPattern, value: V) {
        debug_assert!(matches!(pattern, AliasPattern::Prefix(_)));
        self.entries.push((pattern, value));
        // Stable-sort descending so equal-length patterns preserve
        // insertion order. `BTreeMap` -> `PrefixIndex` is the typical
        // construction path; both produce deterministic ordering.
        self.entries
            .sort_by_key(|(p, _)| std::cmp::Reverse(p.prefix_len()));
    }

    /// Find the longest-prefix match for `wire`. Returns the stored
    /// value clone; lookup is O(N) over the prefix list.
    pub fn longest_match(&self, wire: &str) -> Option<V> {
        for (pat, v) in &self.entries {
            if pat.matches(wire) {
                return Some(v.clone());
            }
        }
        None
    }

    /// True when no prefix patterns are registered (most operators
    /// don't use globs).
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of registered prefix patterns.
    pub const fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_strings() {
        let p = AliasPattern::parse("claude-opus-4-7").expect("parses");
        assert_eq!(p, AliasPattern::Exact("claude-opus-4-7".into()));
    }

    #[test]
    fn parses_trailing_asterisk_as_prefix() {
        let p = AliasPattern::parse("claude-opus-*").expect("parses");
        assert_eq!(p, AliasPattern::Prefix("claude-opus-".into()));
    }

    #[test]
    fn rejects_bare_asterisk() {
        let err = AliasPattern::parse("*").expect_err("must reject");
        assert!(err.contains("default"), "msg: {err}");
    }

    #[test]
    fn rejects_middle_asterisk() {
        let err = AliasPattern::parse("foo-*-bar").expect_err("must reject");
        assert!(err.contains("trailing-asterisk"), "msg: {err}");
    }

    #[test]
    fn longest_prefix_wins() {
        let mut idx: PrefixIndex<&str> = PrefixIndex::new();
        idx.insert(AliasPattern::Prefix("claude-".into()), "broad");
        idx.insert(AliasPattern::Prefix("claude-opus-".into()), "narrow");
        // "claude-opus-4-7" matches both; the longer prefix wins.
        assert_eq!(idx.longest_match("claude-opus-4-7"), Some("narrow"));
        // Only the broad prefix matches.
        assert_eq!(idx.longest_match("claude-haiku-4-5"), Some("broad"));
        // Neither prefix matches.
        assert_eq!(idx.longest_match("gpt-4"), None);
    }

    #[test]
    fn matches_exact_only_for_full_string() {
        let p = AliasPattern::Exact("claude-opus".into());
        assert!(p.matches("claude-opus"));
        assert!(!p.matches("claude-opus-4-7"));
    }

    #[test]
    fn matches_prefix_for_starts_with() {
        let p = AliasPattern::Prefix("claude-opus-".into());
        assert!(p.matches("claude-opus-4-7"));
        assert!(p.matches("claude-opus-"));
        assert!(!p.matches("claude-opus")); // no trailing dash -> doesn't start with "claude-opus-"
        assert!(!p.matches("claude-haiku-4-5"));
    }
}
