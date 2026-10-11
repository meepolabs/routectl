//! Sanitized RFC 6901 pointer naming a location in a wire body.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::safe_token::is_safe_token;

/// Most segments a [`WirePath`] carries; deeper segments are dropped.
pub const MAX_WIRE_PATH_DEPTH: usize = 8;

/// Most bytes a rendered [`WirePath`] occupies. A segment that would push
/// the pointer past this bound coarsens it: that segment and every later one
/// are dropped, so a coarsened pointer names an ancestor of the location and
/// never skips a middle segment.
pub const MAX_WIRE_PATH_LEN: usize = 256;

/// Segment that stands in for a key that is not token-shaped. In an edit
/// pattern the same segment matches exactly one array index or one such
/// masked key, never a token-shaped object key.
pub const WILDCARD_SEGMENT: &str = "*";

/// RFC 6901 JSON pointer over a wire body, safe to log or use as a label.
///
/// A segment failing [`is_safe_token`] (empty, whitespace, control bytes,
/// non-ASCII, longer than 64 bytes) is replaced by [`WILDCARD_SEGMENT`], so
/// a client-chosen key can never inject arbitrary text. The pointer is
/// bounded to [`MAX_WIRE_PATH_DEPTH`] segments and [`MAX_WIRE_PATH_LEN`]
/// bytes; a pointer that dropped a segment to stay inside either bound is
/// coarsened. The root pointer is the empty string.
///
/// Besides the rendered text, a pointer remembers which of its segments an
/// edit-pattern wildcard may match: array indices and masked keys.
///
/// Equality, ordering and hashing compare the rendered pointer only.
#[derive(Debug, Clone, Default)]
pub struct WirePath {
    pointer: String,
    coarsened: bool,
    /// Bit `i` is set when segment `i` is an array index or a masked key.
    wildcard_mask: u8,
}

impl WirePath {
    /// The root pointer (`""`).
    pub const fn root() -> Self {
        Self {
            pointer: String::new(),
            coarsened: false,
            wildcard_mask: 0,
        }
    }

    /// Build a pointer from raw (unescaped) object keys, in order.
    pub fn from_segments<'s, I>(segments: I) -> Self
    where
        I: IntoIterator<Item = &'s str>,
    {
        segments
            .into_iter()
            .fold(Self::root(), |path, segment| path.child(segment))
    }

    /// This pointer extended by one raw (unescaped) object key. Returns a
    /// coarsened copy, without the key, once either bound would be exceeded.
    pub fn child(&self, segment: &str) -> Self {
        if is_safe_token(segment) {
            self.push(&segment.replace('~', "~0").replace('/', "~1"), false)
        } else {
            self.push(WILDCARD_SEGMENT, true)
        }
    }

    /// This pointer extended by one array index. Returns a coarsened copy,
    /// without the index, once either bound would be exceeded.
    pub fn index(&self, index: usize) -> Self {
        self.push(&index.to_string(), true)
    }

    /// True when a segment was dropped to keep the pointer inside its
    /// bounds, so it names an ancestor of the location rather than the
    /// location itself.
    pub const fn is_coarsened(&self) -> bool {
        self.coarsened
    }

    /// True when segment `position` is an array index or a masked key.
    pub(super) fn is_wildcard_segment(&self, position: usize) -> bool {
        u32::try_from(position)
            .ok()
            .and_then(|shift| 1u8.checked_shl(shift))
            .is_some_and(|bit| self.wildcard_mask & bit != 0)
    }

    fn push(&self, escaped: &str, wildcard: bool) -> Self {
        let depth = self.depth();
        if self.coarsened
            || depth >= MAX_WIRE_PATH_DEPTH
            || self.pointer.len() + 1 + escaped.len() > MAX_WIRE_PATH_LEN
        {
            return Self {
                coarsened: true,
                ..self.clone()
            };
        }
        let bit = u32::try_from(depth)
            .ok()
            .and_then(|shift| 1u8.checked_shl(shift))
            .filter(|_| wildcard)
            .unwrap_or(0);
        let mut pointer = String::with_capacity(self.pointer.len() + 1 + escaped.len());
        pointer.push_str(&self.pointer);
        pointer.push('/');
        pointer.push_str(escaped);
        Self {
            pointer,
            coarsened: false,
            wildcard_mask: self.wildcard_mask | bit,
        }
    }

    /// Number of segments.
    pub fn depth(&self) -> usize {
        self.pointer.bytes().filter(|b| *b == b'/').count()
    }

    /// The escaped segments, in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.pointer.split('/').skip(1)
    }

    /// The rendered pointer.
    pub fn as_str(&self) -> &str {
        &self.pointer
    }
}

impl PartialEq for WirePath {
    fn eq(&self, other: &Self) -> bool {
        self.pointer == other.pointer
    }
}

impl Eq for WirePath {}

impl Hash for WirePath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pointer.hash(state);
    }
}

impl PartialOrd for WirePath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WirePath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.pointer.cmp(&other.pointer)
    }
}

impl fmt::Display for WirePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pointer)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_WIRE_PATH_DEPTH, MAX_WIRE_PATH_LEN, WirePath};

    #[test]
    fn segments_are_sanitized_and_escaped() {
        let over_cap = "k".repeat(65);
        let at_cap = "k".repeat(64);
        let at_cap_expected = format!("/{at_cap}");
        let rows: [(&str, Vec<&str>, &str); 10] = [
            ("root", vec![], ""),
            (
                "plain keys",
                vec!["messages", "0", "content"],
                "/messages/0/content",
            ),
            ("empty segment", vec!["messages", ""], "/messages/*"),
            ("whitespace segment", vec!["two words"], "/*"),
            ("control byte segment", vec!["a\nb"], "/*"),
            ("escape byte segment", vec!["\x1b[31m"], "/*"),
            ("non ascii segment", vec!["caf\u{e9}"], "/*"),
            ("over 64 bytes", vec![over_cap.as_str()], "/*"),
            (
                "exactly 64 bytes",
                vec![at_cap.as_str()],
                at_cap_expected.as_str(),
            ),
            ("tilde and slash escaped", vec!["a~b/c"], "/a~0b~1c"),
        ];

        for (name, segments, expected) in rows {
            let path = WirePath::from_segments(segments);

            assert_eq!(path.as_str(), expected, "row {name}");
        }
    }

    #[test]
    fn depth_is_capped() {
        let segments = ["a"; MAX_WIRE_PATH_DEPTH + 3];

        let path = WirePath::from_segments(segments);

        assert_eq!(path.depth(), MAX_WIRE_PATH_DEPTH);
        assert_eq!(path.as_str(), "/a/a/a/a/a/a/a/a");
        assert!(path.is_coarsened());
    }

    #[test]
    fn a_pointer_inside_its_bounds_is_not_coarsened() {
        let path = WirePath::from_segments(["a"; MAX_WIRE_PATH_DEPTH]);

        assert!(!path.is_coarsened());
    }

    #[test]
    fn only_indices_and_masked_keys_are_wildcard_segments() {
        let path = WirePath::root().child("tools").index(3).child("a b");

        let flags: Vec<bool> = (0..4).map(|i| path.is_wildcard_segment(i)).collect();

        assert_eq!(path.as_str(), "/tools/3/*");
        assert_eq!(flags, [false, true, true, false]);
    }

    #[test]
    fn length_is_capped_and_seals_the_pointer() {
        let wide = "~".repeat(64);
        let segments = [wide.as_str(), wide.as_str(), "short"];

        let path = WirePath::from_segments(segments);

        assert!(
            path.as_str().len() <= MAX_WIRE_PATH_LEN,
            "{}",
            path.as_str().len()
        );
        assert_eq!(path.as_str(), format!("/{}", "~0".repeat(64)));
        assert!(path.is_coarsened());
    }

    #[test]
    fn segments_iterate_escaped_values() {
        let path = WirePath::from_segments(["tools", "1", "a/b"]);

        assert_eq!(path.segments().collect::<Vec<_>>(), ["tools", "1", "a~1b"]);
        assert_eq!(WirePath::root().segments().count(), 0);
    }
}
