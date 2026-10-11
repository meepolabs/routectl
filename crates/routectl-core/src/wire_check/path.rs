//! Sanitized RFC 6901 pointer naming a location in a wire body.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::safe_token::is_safe_token;

/// Most segments a [`WirePath`] carries; deeper segments are dropped.
pub const MAX_WIRE_PATH_DEPTH: usize = 8;

/// Most bytes a rendered [`WirePath`] occupies. A segment that would push
/// the pointer past this bound seals it: that segment and every later one
/// are dropped, so a sealed pointer names an ancestor of the location and
/// never skips a middle segment.
pub const MAX_WIRE_PATH_LEN: usize = 256;

/// Segment that stands in for a key that is not token-shaped. In an edit
/// pattern the same segment matches exactly one segment of any value.
pub const WILDCARD_SEGMENT: &str = "*";

/// RFC 6901 JSON pointer over a wire body, safe to log or use as a label.
///
/// A segment failing [`is_safe_token`] (empty, whitespace, control bytes,
/// non-ASCII, longer than 64 bytes) is replaced by [`WILDCARD_SEGMENT`], so
/// a client-chosen key can never inject arbitrary text. The pointer is
/// bounded to [`MAX_WIRE_PATH_DEPTH`] segments and [`MAX_WIRE_PATH_LEN`]
/// bytes. The root pointer is the empty string.
///
/// Equality, ordering and hashing compare the rendered pointer only.
#[derive(Debug, Clone, Default)]
pub struct WirePath {
    pointer: String,
    sealed: bool,
}

impl WirePath {
    /// The root pointer (`""`).
    pub const fn root() -> Self {
        Self {
            pointer: String::new(),
            sealed: false,
        }
    }

    /// Build a pointer from raw (unescaped) segments, in order.
    pub fn from_segments<'s, I>(segments: I) -> Self
    where
        I: IntoIterator<Item = &'s str>,
    {
        segments
            .into_iter()
            .fold(Self::root(), |path, segment| path.child(segment))
    }

    /// This pointer extended by one raw (unescaped) segment. Returns an
    /// unchanged copy once the depth bound is reached, and a sealed copy
    /// when the length bound would be exceeded.
    pub fn child(&self, segment: &str) -> Self {
        if self.sealed || self.depth() >= MAX_WIRE_PATH_DEPTH {
            return self.clone();
        }
        let escaped = escape_segment(segment);
        if self.pointer.len() + 1 + escaped.len() > MAX_WIRE_PATH_LEN {
            return Self {
                pointer: self.pointer.clone(),
                sealed: true,
            };
        }
        let mut pointer = String::with_capacity(self.pointer.len() + 1 + escaped.len());
        pointer.push_str(&self.pointer);
        pointer.push('/');
        pointer.push_str(&escaped);
        Self {
            pointer,
            sealed: false,
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

fn escape_segment(segment: &str) -> String {
    if !is_safe_token(segment) {
        return WILDCARD_SEGMENT.to_string();
    }
    segment.replace('~', "~0").replace('/', "~1")
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
    }

    #[test]
    fn segments_iterate_escaped_values() {
        let path = WirePath::from_segments(["tools", "1", "a/b"]);

        assert_eq!(path.segments().collect::<Vec<_>>(), ["tools", "1", "a~1b"]);
        assert_eq!(WirePath::root().segments().count(), 0);
    }
}
