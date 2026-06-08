//! Tool-call id charset sanitization shared across egresses.
//!
//! Anthropic (and Bedrock Converse, which mirrors the same constraint)
//! require `tool_use.id` to match `^[a-zA-Z0-9_-]+$`. An OpenAI-origin
//! id frequently carries `.`/`:`/`/` (e.g. `call.foo:1`) and 400s the
//! upstream. The fix is to replace every char outside `[a-zA-Z0-9_-]`
//! with `_` at every id-emit site AND every matching tool_result
//! correlation site. The mapping is deterministic (same input ->
//! same output) so a sanitized `tool_use.id` and the `tool_result`
//! answering it land on the same value and the result is not orphaned.
//!
//! An already-valid id is returned unchanged (no allocation). The
//! deterministic empty-id fallback (`call_<index>`) lives at the call
//! sites and is itself charset-valid, so sanitizing it is a no-op.

use std::borrow::Cow;

/// True iff `c` is in Anthropic's allowed tool-id charset
/// `[a-zA-Z0-9_-]`.
fn is_allowed_tool_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Replace every char of `id` not in `[a-zA-Z0-9_-]` with `_`.
///
/// Deterministic: the same input always maps to the same output, which
/// is REQUIRED so a sanitized tool_use id and the tool_result that
/// correlates to it remain equal. An already-valid id borrows through
/// without allocating.
pub(crate) fn sanitize_tool_id(id: &str) -> Cow<'_, str> {
    if id.chars().all(is_allowed_tool_id_char) {
        return Cow::Borrowed(id);
    }
    Cow::Owned(
        id.chars()
            .map(|c| if is_allowed_tool_id_char(c) { c } else { '_' })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_is_replaced_with_underscore() {
        // Arrange / Act / Assert
        assert_eq!(sanitize_tool_id("call.1"), "call_1");
    }

    #[test]
    fn colon_and_slash_are_replaced_with_underscore() {
        // Arrange / Act / Assert
        assert_eq!(sanitize_tool_id("tool:use/x"), "tool_use_x");
    }

    #[test]
    fn already_valid_id_is_unchanged() {
        // Arrange / Act / Assert
        assert_eq!(sanitize_tool_id("call_abc-1_2"), "call_abc-1_2");
    }

    #[test]
    fn already_valid_id_borrows_without_allocating() {
        // Arrange
        let id = "call_abc-1_2";
        // Act
        let out = sanitize_tool_id(id);
        // Assert
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn empty_id_round_trips_empty() {
        // The deterministic `call_<index>` empty fallback lives at the
        // call sites; the helper itself leaves an empty string empty.
        assert_eq!(sanitize_tool_id(""), "");
    }

    #[test]
    fn sanitization_is_deterministic_for_correlation() {
        // Same input -> same output, the invariant correlation relies on.
        assert_eq!(
            sanitize_tool_id("call.foo:1"),
            sanitize_tool_id("call.foo:1")
        );
        assert_eq!(sanitize_tool_id("call.foo:1"), "call_foo_1");
    }
}
