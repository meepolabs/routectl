//! Sanitized RFC 6901 pointer naming a location in a wire body.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Most segments a [`WirePath`] carries; deeper segments are dropped.
pub const MAX_WIRE_PATH_DEPTH: usize = 8;

/// Most bytes a rendered [`WirePath`] occupies. A segment that would push
/// the pointer past this bound coarsens it: that segment and every later one
/// are dropped, so a coarsened pointer names an ancestor of the location and
/// never skips a middle segment.
pub const MAX_WIRE_PATH_LEN: usize = 256;

/// Most bytes an object key may have and still render verbatim.
const MAX_WIRE_KEY_LEN: usize = 32;

/// Segment that stands in for a masked object key. In an edit pattern the
/// same segment matches exactly one array index or one masked key, never a
/// verbatim object key.
pub const WILDCARD_SEGMENT: &str = "*";

/// True when `key` may render verbatim by shape alone: an ASCII identifier
/// (`[A-Za-z_][A-Za-z0-9_-]*`) of at most [`MAX_WIRE_KEY_LEN`] bytes. Such a
/// key never needs RFC 6901 escaping.
pub(super) fn is_identifier_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    key.len() <= MAX_WIRE_KEY_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// True when a rendered segment stands for an array element: an index, or
/// the wildcard an index may render as in a pattern.
fn is_element_segment(segment: &str) -> bool {
    segment == WILDCARD_SEGMENT
        || (!segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()))
}

/// Keys that open a client-keyed object in at least one position.
const CONTAINER_KEYS: [&str; 10] = [
    "metadata",
    "client_metadata",
    "input_schema",
    "arguments",
    "headers",
    "extra_headers",
    "parameters",
    "schema",
    "input",
    "variables",
];

/// True when verbatim object key `key`, placed under the location whose
/// rendered segments are `ancestors`, opens an object whose keys the client
/// chooses: every key at any depth below it is masked.
///
/// The containers are request `metadata` and Responses `client_metadata`;
/// a tool's argument schema (`input_schema`, function `parameters`) and a
/// structured-output `schema`, masked whole because schema keywords and
/// property names share one namespace below them; the arguments a tool call
/// carries (a `tool_use` block's `input`, a function call's `arguments`);
/// header maps (`headers`, `extra_headers`); and Responses prompt
/// `variables`.
pub(super) fn opens_user_keyed(ancestors: &[&str], key: &str) -> bool {
    if !CONTAINER_KEYS.contains(&key) {
        return false;
    }
    let parent = ancestors.last().copied();
    let grandparent = ancestors
        .len()
        .checked_sub(2)
        .and_then(|i| ancestors.get(i));
    match key {
        "metadata" | "client_metadata" => ancestors.is_empty(),
        "input_schema" | "arguments" | "headers" | "extra_headers" => true,
        "parameters" => ancestors.first() == Some(&"tools"),
        "schema" => matches!(parent, Some("json_schema" | "format")),
        "input" => parent.is_some_and(is_element_segment) && grandparent == Some(&"content"),
        "variables" => ancestors == ["prompt"],
        _ => false,
    }
}

/// RFC 6901 JSON pointer over a wire body, safe to log or use as a label.
///
/// An object key renders verbatim only when it is an ASCII identifier
/// (`[A-Za-z_][A-Za-z0-9_-]*`, at most 32 bytes) AND no ancestor opens an
/// object whose keys the client chooses (request `metadata`, a tool's
/// argument schema, a tool call's arguments, a header map, among others);
/// every other key is replaced by [`WILDCARD_SEGMENT`], so neither arbitrary
/// text nor a client-chosen name can reach a log line or a status row. The
/// pointer is bounded to [`MAX_WIRE_PATH_DEPTH`] segments and
/// [`MAX_WIRE_PATH_LEN`] bytes; a
/// pointer that dropped a segment to stay inside either bound is coarsened.
/// The root pointer is the empty string.
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
    /// True when this location is inside a client-keyed object.
    masks_keys: bool,
}

impl WirePath {
    /// The root pointer (`""`).
    pub const fn root() -> Self {
        Self {
            pointer: String::new(),
            coarsened: false,
            wildcard_mask: 0,
            masks_keys: false,
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

    /// This pointer extended by one raw (unescaped) object key, masked
    /// unless it may render verbatim. Returns a coarsened copy, without the
    /// key, once either bound would be exceeded.
    pub fn child(&self, segment: &str) -> Self {
        if self.masks_keys || !is_identifier_key(segment) {
            return self.push(WILDCARD_SEGMENT, true, self.masks_keys);
        }
        let opens = CONTAINER_KEYS.contains(&segment)
            && opens_user_keyed(&self.segments().collect::<Vec<_>>(), segment);
        self.push(segment, false, opens)
    }

    /// This pointer extended by one array index. Returns a coarsened copy,
    /// without the index, once either bound would be exceeded.
    pub fn index(&self, index: usize) -> Self {
        self.push(&index.to_string(), true, self.masks_keys)
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

    fn push(&self, rendered: &str, wildcard: bool, masks_keys: bool) -> Self {
        let depth = self.depth();
        if self.coarsened
            || depth >= MAX_WIRE_PATH_DEPTH
            || self.pointer.len() + 1 + rendered.len() > MAX_WIRE_PATH_LEN
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
        let mut pointer = String::with_capacity(self.pointer.len() + 1 + rendered.len());
        pointer.push_str(&self.pointer);
        pointer.push('/');
        pointer.push_str(rendered);
        Self {
            pointer,
            coarsened: false,
            wildcard_mask: self.wildcard_mask | bit,
            masks_keys,
        }
    }

    /// Number of segments.
    pub fn depth(&self) -> usize {
        self.pointer.bytes().filter(|b| *b == b'/').count()
    }

    /// The rendered segments, in order. No rendered segment needs RFC 6901
    /// escaping.
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
    use super::{MAX_WIRE_KEY_LEN, MAX_WIRE_PATH_DEPTH, MAX_WIRE_PATH_LEN, WirePath};

    /// A path where each all-digit segment is an array index and every
    /// other segment is an object key.
    fn path_of(segments: &[&str]) -> WirePath {
        segments.iter().fold(WirePath::root(), |path, segment| {
            match segment.parse::<usize>() {
                Ok(index) => path.index(index),
                Err(_) => path.child(segment),
            }
        })
    }

    #[test]
    fn keys_render_verbatim_only_when_identifier_shaped_outside_client_keyed_objects() {
        let at_cap = "k".repeat(MAX_WIRE_KEY_LEN);
        let over_cap = "k".repeat(MAX_WIRE_KEY_LEN + 1);
        let at_cap_expected = format!("/{at_cap}");
        let rows: [(&str, Vec<&str>, &str); 23] = [
            ("root", vec![], ""),
            ("schema key", vec!["max_tokens"], "/max_tokens"),
            (
                "nested schema keys and indices",
                vec!["messages", "0", "content", "1", "cache_control"],
                "/messages/0/content/1/cache_control",
            ),
            ("hyphenated key", vec!["anthropic-beta"], "/anthropic-beta"),
            ("email-like key", vec!["ops@example.com"], "/*"),
            (
                "api-key-like key",
                vec!["sk-ant-api03-Zx9yQ2wV8uT7sR6pO5nM4lK3"],
                "/*",
            ),
            (
                "key at the length cap",
                vec![at_cap.as_str()],
                at_cap_expected.as_str(),
            ),
            ("key over the length cap", vec![over_cap.as_str()], "/*"),
            ("leading digit", vec!["1st"], "/*"),
            ("empty key", vec![""], "/*"),
            ("whitespace", vec!["two words"], "/*"),
            ("control byte", vec!["a\nb"], "/*"),
            ("non ascii", vec!["caf\u{e9}"], "/*"),
            ("tilde and slash", vec!["a~b/c"], "/*"),
            (
                "key under metadata",
                vec!["metadata", "user_id"],
                "/metadata/*",
            ),
            (
                "key under client_metadata",
                vec!["client_metadata", "x-codex-installation-id"],
                "/client_metadata/*",
            ),
            (
                "key under input_schema properties",
                vec!["tools", "0", "input_schema", "properties", "account_id"],
                "/tools/0/input_schema/*/*",
            ),
            (
                "key under function parameters",
                vec!["tools", "0", "function", "parameters", "type"],
                "/tools/0/function/parameters/*",
            ),
            (
                "key under tool_use input",
                vec!["messages", "1", "content", "0", "input", "path"],
                "/messages/1/content/0/input/*",
            ),
            (
                "key under function arguments",
                vec!["input", "2", "arguments", "query"],
                "/input/2/arguments/*",
            ),
            (
                "key under a structured-output schema",
                vec!["response_format", "json_schema", "schema", "type"],
                "/response_format/json_schema/schema/*",
            ),
            (
                "key under a header map",
                vec!["headers", "x-team"],
                "/headers/*",
            ),
            (
                "Responses input items are not client-keyed",
                vec!["input", "0", "content"],
                "/input/0/content",
            ),
        ];

        for (name, segments, expected) in rows {
            let path = path_of(&segments);

            assert_eq!(path.as_str(), expected, "row {name}");
        }
    }

    #[test]
    fn an_all_digit_object_key_never_renders_as_an_index() {
        let path = WirePath::root().child("x").child("0");

        assert_eq!(path.as_str(), "/x/*");
    }

    #[test]
    fn metadata_is_client_keyed_only_at_the_top_level() {
        let path = path_of(&["tools", "0", "metadata", "owner"]);

        assert_eq!(path.as_str(), "/tools/0/metadata/owner");
    }

    #[test]
    fn masking_reaches_every_depth_below_a_client_keyed_object() {
        let path = path_of(&["metadata", "a", "0", "b"]);

        assert_eq!(path.as_str(), "/metadata/*/0/*");
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
        let wide = "k".repeat(MAX_WIRE_KEY_LEN);
        let segments = [wide.as_str(); MAX_WIRE_PATH_DEPTH];

        let path = WirePath::from_segments(segments);

        assert!(
            path.as_str().len() <= MAX_WIRE_PATH_LEN,
            "{}",
            path.as_str().len()
        );
        assert_eq!(path.depth(), MAX_WIRE_PATH_DEPTH - 1);
        assert!(path.is_coarsened());
    }

    #[test]
    fn segments_iterate_rendered_values() {
        let path = path_of(&["tools", "1", "a/b"]);

        assert_eq!(path.segments().collect::<Vec<_>>(), ["tools", "1", "*"]);
        assert_eq!(WirePath::root().segments().count(), 0);
    }
}
