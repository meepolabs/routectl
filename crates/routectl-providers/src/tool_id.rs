//! Tool-call id charset sanitization shared across egresses.
//!
//! Anthropic's Messages API publishes no pattern for `tool_use.id` /
//! `tool_result.tool_use_id`, but rejects an id carrying `.`/`:`/`/`
//! (an OpenAI-origin id such as `call.foo:1` 400s the upstream). Bedrock
//! Converse documents `[a-zA-Z0-9_.:-]+`, max 64, for `toolUseId`.
//! `[a-zA-Z0-9_-]` is therefore the conservative floor both accept, and
//! is the charset this module targets at every id-emit site AND every
//! matching tool_result correlation site.
//!
//! The mapping is INJECTIVE, not lossy: two distinct raw ids never share
//! a wire id. A lossy char-replacement (`.` and `:` both -> `_`) let
//! `call.a` and `call:a` collapse onto one wire id, and two tool_use
//! blocks with the same id are themselves a 400. Instead, an id that
//! needs escaping is emitted as `esc_<escaped>` (see
//! [`sanitize_tool_id`]), a namespace disjoint from every id that passes
//! through verbatim.
//!
//! The mapping is also PURE and deterministic: a `tool_use` and the
//! `tool_result` answering it are translated at different call sites with
//! no shared state, so equal raw ids must land on equal wire ids for the
//! result not to be orphaned.
//!
//! Escaping expands an id (up to 3x), so an escaped form that would
//! exceed Bedrock's documented 64-char `toolUseId` ceiling is folded to
//! a digest form under its own prefix rather than trading one 400 for
//! another.
//!
//! An already-valid id is returned unchanged (no allocation). The
//! deterministic empty-id fallback (`call_<index>`) lives at the call
//! sites and is itself charset-valid, so sanitizing it is a no-op.

use std::borrow::Cow;

/// Marker prefix for the escaped form. An id that needs escaping is
/// emitted under this prefix, and an id that already starts with one of
/// the marker prefixes is escaped too, so the verbatim, escaped, and
/// digest namespaces are pairwise disjoint -- the property that makes the
/// mapping injective.
const ESCAPE_PREFIX: &str = "esc_";

/// Marker prefix for the digest form used when the escaped form would
/// exceed [`MAX_TOOL_ID_LEN`]. Distinct from `ESCAPE_PREFIX` (the fourth
/// byte is `t`, not `_`), so a digest id can never equal an escaped one.
const DIGEST_PREFIX: &str = "esct_";

/// The escape character inside an escaped id. It is itself in the allowed
/// charset, so it must be escaped when it occurs literally.
const ESCAPE_CHAR: u8 = b'_';

/// Ceiling on an emitted id. Bedrock Converse documents `toolUseId` as
/// max length 64; Anthropic publishes no limit, so 64 is the floor both
/// accept.
const MAX_TOOL_ID_LEN: usize = 64;

/// Hex width of the digest-form suffix.
const DIGEST_HEX_LEN: usize = 16;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// True iff `b` is in the target tool-id charset `[a-zA-Z0-9_-]`.
const fn is_allowed_tool_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// True iff `id` can reach the wire verbatim: every byte is in the
/// allowed charset AND it does not intrude on a marker namespace.
fn is_wire_safe(id: &str) -> bool {
    id.bytes().all(is_allowed_tool_id_byte)
        && !id.starts_with(ESCAPE_PREFIX)
        && !id.starts_with(DIGEST_PREFIX)
}

/// Map `id` into the `[a-zA-Z0-9_-]` tool-id charset, injectively.
///
/// A wire-safe id borrows through unchanged. Anything else becomes
/// `esc_<escaped>`, where `escaped` encodes each byte outside
/// `[a-zA-Z0-9-]` -- including a literal `_` -- as `_<lowercase hex>`.
/// That encoding is reversible, so distinct raw ids yield distinct
/// escaped bodies, and the marker prefix keeps an escaped id from ever
/// equalling a verbatim one. Callers therefore cannot emit two tool_use
/// blocks sharing a wire id, and because the function is pure a
/// tool_result still correlates to its tool_use.
///
/// An escaped form over [`MAX_TOOL_ID_LEN`] folds to
/// `esct_<prefix>_<digest>`, which is exactly at the ceiling and stays
/// deterministic; injectivity there rests on the 64-bit digest rather
/// than on reversibility.
///
/// An empty id maps to empty: the non-empty guarantee is the call sites'
/// (`call_<index>`), not this function's.
pub fn sanitize_tool_id(id: &str) -> Cow<'_, str> {
    if is_wire_safe(id) {
        return Cow::Borrowed(id);
    }
    let mut out = String::with_capacity(ESCAPE_PREFIX.len() + id.len() * 3);
    out.push_str(ESCAPE_PREFIX);
    for b in id.bytes() {
        if is_allowed_tool_id_byte(b) && b != ESCAPE_CHAR {
            out.push(char::from(b));
        } else {
            out.push(char::from(ESCAPE_CHAR));
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
    }
    if out.len() > MAX_TOOL_ID_LEN {
        return Cow::Owned(digest_form(id, &out));
    }
    Cow::Owned(out)
}

/// Fold an over-long escaped id to `esct_<escaped prefix>_<digest>`,
/// exactly [`MAX_TOOL_ID_LEN`] bytes. The retained prefix is for operator
/// legibility only; the digest carries the distinctness. `escaped` is
/// pure ASCII by construction, so the prefix slice is on a char boundary.
fn digest_form(raw: &str, escaped: &str) -> String {
    let keep = MAX_TOOL_ID_LEN - DIGEST_PREFIX.len() - 1 - DIGEST_HEX_LEN;
    let body = &escaped[ESCAPE_PREFIX.len()..];
    let head = &body[..body.len().min(keep)];
    format!(
        "{DIGEST_PREFIX}{head}_{digest:016x}",
        digest = fnv1a64(raw.as_bytes())
    )
}

/// FNV-1a, 64-bit. Hand-rolled rather than `DefaultHasher` because the
/// digest must be stable across builds: a `tool_use` emitted by one
/// routectl process can be answered by a `tool_result` a later process
/// re-sanitizes, and std's default hasher makes no cross-version
/// stability promise. Not a security primitive -- distinctness only.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_is_escaped_not_folded_to_underscore() {
        // Arrange / Act / Assert -- `.` is 0x2e.
        assert_eq!(sanitize_tool_id("call.1"), "esc_call_2e1");
    }

    #[test]
    fn colon_and_slash_are_escaped() {
        // Arrange / Act / Assert -- `:` is 0x3a, `/` is 0x2f.
        assert_eq!(sanitize_tool_id("tool:use/x"), "esc_tool_3ause_2fx");
    }

    #[test]
    fn literal_underscore_is_escaped_inside_an_escaped_id() {
        // The escape char must be escaped for the encoding to stay
        // reversible; `_` is 0x5f.
        assert_eq!(sanitize_tool_id("a_b.c"), "esc_a_5fb_2ec");
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
        assert_eq!(sanitize_tool_id("call.foo:1"), "esc_call_2efoo_3a1");
    }

    #[test]
    fn distinct_ids_differing_only_in_escaped_chars_stay_distinct() {
        // The collision the lossy char-replacement mapping produced: two
        // distinct source tool calls in ONE message both reached the wire
        // as `call_a`, and duplicate tool_use ids are a 400.
        assert_ne!(sanitize_tool_id("call.a"), sanitize_tool_id("call:a"));
    }

    #[test]
    fn escaped_id_never_equals_a_verbatim_id() {
        // `call_a` is wire-safe and passes through; `call.a` must not land
        // on it, or a client sending both would emit a duplicate wire id.
        assert_eq!(sanitize_tool_id("call_a"), "call_a");
        assert_ne!(sanitize_tool_id("call.a"), sanitize_tool_id("call_a"));
    }

    #[test]
    fn valid_id_intruding_on_the_escaped_namespace_is_itself_escaped() {
        // Otherwise a raw `esc_call_2e1` would collide with the escaped
        // form of `call.1`.
        assert_eq!(sanitize_tool_id("esc_call_2e1"), "esc_esc_5fcall_5f2e1");
        assert_ne!(sanitize_tool_id("esc_call_2e1"), sanitize_tool_id("call.1"));
    }

    #[test]
    fn valid_id_intruding_on_the_digest_namespace_is_itself_escaped() {
        // The digest namespace needs the same guard as the escape one.
        let out = sanitize_tool_id("esct_x");
        assert!(out.starts_with(ESCAPE_PREFIX));
        assert_ne!(out, "esct_x");
    }

    #[test]
    fn output_is_always_in_the_target_charset() {
        // Arrange -- multi-byte and control input, the worst shapes a
        // client can send.
        for id in ["call.a", "a b", "\u{1f600}", "esc_x", "tool:1/2"] {
            // Act
            let out = sanitize_tool_id(id);
            // Assert
            assert!(
                out.bytes().all(is_allowed_tool_id_byte),
                "sanitized `{out}` left the target charset"
            );
        }
    }

    #[test]
    fn distinct_inputs_map_to_distinct_outputs_across_a_colliding_set() {
        // Arrange -- every pair here collapsed to one value under the
        // lossy mapping.
        let ids = ["call.a", "call:a", "call/a", "call_a", "call-a", "call a"];

        // Act
        let out: Vec<String> = ids
            .iter()
            .map(|id| sanitize_tool_id(id).into_owned())
            .collect();

        // Assert
        let mut unique = out.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), out.len(), "sanitization must be injective");
    }

    #[test]
    fn over_long_escaped_id_folds_to_the_digest_form_within_the_ceiling() {
        // Arrange -- 40 dots escape to 120 bytes, well over the ceiling.
        let id = ".".repeat(40);

        // Act
        let out = sanitize_tool_id(&id);

        // Assert
        assert!(out.starts_with(DIGEST_PREFIX), "got `{out}`");
        assert_eq!(out.len(), MAX_TOOL_ID_LEN);
        assert!(out.bytes().all(is_allowed_tool_id_byte));
    }

    #[test]
    fn digest_form_is_deterministic_and_distinct_per_input() {
        // Arrange -- two over-long ids sharing the retained prefix, so
        // only the digest separates them.
        let a = format!("{}a", ".".repeat(40));
        let b = format!("{}b", ".".repeat(40));

        // Act
        let out_a = sanitize_tool_id(&a);
        let out_b = sanitize_tool_id(&b);

        // Assert
        assert_eq!(out_a, sanitize_tool_id(&a), "must be deterministic");
        assert_ne!(out_a, out_b, "digest must separate distinct inputs");
    }

    #[test]
    fn long_wire_safe_id_is_not_truncated() {
        // The ceiling applies only to the escape expansion. An id the
        // client already sent in the target charset passes through, even
        // over 64 bytes -- truncating it would orphan its tool_result.
        let id = "a".repeat(80);
        assert_eq!(sanitize_tool_id(&id), id);
    }
}
