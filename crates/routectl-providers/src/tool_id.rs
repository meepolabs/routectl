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
//! The mapping is NOT lossy in the char-replacement sense: a lossy
//! replacement (`.` and `:` both -> `_`) let `call.a` and `call:a`
//! collapse onto one wire id, and two tool_use blocks with the same id
//! are themselves a 400. Instead, an id that needs escaping is emitted as
//! `esc_<escaped>` (see [`sanitize_tool_id`]), a namespace disjoint from
//! every id that passes through verbatim.
//!
//! Distinctness comes with two different strengths, and they are not
//! interchangeable:
//!
//! - **Escape path -- INJECTIVE.** The `_<hex>` encoding is reversible,
//!   so distinct raw ids always yield distinct escaped bodies.
//! - **Digest path -- COLLISION-RESISTANT ONLY.** An id whose emitted
//!   form would exceed the 64-byte ceiling folds to
//!   `esct_<prefix>_<digest>` (see [`sanitize_tool_id`]). Output is
//!   capped at 64 bytes while input is unbounded, so by pigeonhole some
//!   pair of inputs must collide; distinctness rests on a 64-bit FNV-1a
//!   digest over a retained prefix that unequal inputs can already
//!   saturate. Not adversary-resistant -- FNV is not the primitive for
//!   that, and a keyed or cryptographic digest would be if it were ever
//!   wanted. It is not swapped speculatively because the digest must
//!   stay stable across processes and builds (see [`fnv1a64`]).
//!
//! The mapping is also PURE and deterministic: a `tool_use` and the
//! `tool_result` answering it are translated at different call sites with
//! no shared state, so equal raw ids must land on equal wire ids for the
//! result not to be orphaned. That is also why the ceiling is applied
//! uniformly rather than per-lane: a lane-dependent mapping would send
//! one lane's `tool_use` verbatim and a later lane's `tool_result`
//! folded, and the two would no longer correlate across a fallback chain.
//!
//! Escaping expands an id (up to 3x), and even an already-wire-safe id
//! can exceed Bedrock's documented 64-char `toolUseId` ceiling, so any
//! emitted form over the ceiling is folded to the digest form under its
//! own prefix rather than trading one 400 for another.
//!
//! An already-valid id within the ceiling is returned unchanged (no
//! allocation). The deterministic empty-id fallback (`call_<index>`) lives
//! at the call sites and is itself charset-valid, so sanitizing it is a
//! no-op.

use std::borrow::Cow;

/// Marker prefix for the escaped form. An id that needs escaping is
/// emitted under this prefix, and an id that already starts with one of
/// the marker prefixes is escaped too, so the verbatim, escaped, and
/// digest namespaces are pairwise disjoint -- no id from one form can
/// ever equal an id from another.
const ESCAPE_PREFIX: &str = "esc_";

/// Marker prefix for the digest form used when an emitted id would exceed
/// [`MAX_TOOL_ID_LEN`]. Distinct from `ESCAPE_PREFIX` (the fourth byte is
/// `t`, not `_`), so a digest id can never equal an escaped one.
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

/// Map `id` into the `[a-zA-Z0-9_-]` tool-id charset within
/// [`MAX_TOOL_ID_LEN`] bytes.
///
/// A wire-safe id at or under the ceiling borrows through unchanged.
/// Anything else becomes `esc_<escaped>`, where `escaped` encodes each
/// byte outside `[a-zA-Z0-9-]` -- including a literal `_` -- as
/// `_<lowercase hex>`. That encoding is reversible, so on this path the
/// mapping is INJECTIVE: distinct raw ids yield distinct escaped bodies,
/// and the marker prefix keeps an escaped id from ever equalling a
/// verbatim one. Because the function is pure, a tool_result still
/// correlates to its tool_use.
///
/// Any emitted form over [`MAX_TOOL_ID_LEN`] -- an escape expansion, or a
/// wire-safe id that is simply too long -- folds to
/// `esct_<prefix>_<digest>`, exactly at the ceiling and still
/// deterministic. That path is COLLISION-RESISTANT, not injective: a
/// 64-byte output over unbounded input must collide by pigeonhole, and
/// only the 64-bit digest separates inputs whose retained prefix agrees.
/// See the module docs for why that tradeoff is taken and why the digest
/// is not a cryptographic one.
///
/// An empty id maps to empty: the non-empty guarantee is the call sites'
/// (`call_<index>`), not this function's.
///
/// # Input contract
///
/// `id` MUST be a RAW client / canonical tool-call id -- never an output
/// of this function. Applying it twice is not a no-op and is not meant to
/// be: the function is deliberately NOT idempotent. Idempotence would
/// require passing an id that already starts with a marker prefix
/// through unchanged, and that is exactly what destroys injectivity -- a
/// raw client id `esc_x` would then land on the same wire id as the
/// escaped form of `x`. Escaping such an id again is what keeps the
/// verbatim, escaped, and digest namespaces disjoint.
pub fn sanitize_tool_id(id: &str) -> Cow<'_, str> {
    // No debug_assert that `id` lacks a marker prefix: a legitimate raw
    // client id may genuinely start with `esc_` / `esct_` (the case the
    // prefix check in `is_wire_safe` exists to escape), so such a guard
    // would fire on valid input. Nothing at this boundary can tell that
    // id apart from this function's own output, so the single-application
    // invariant is documented above rather than asserted.
    if is_wire_safe(id) {
        if id.len() > MAX_TOOL_ID_LEN {
            // Already in the target charset, so the id is its own
            // legibility prefix -- there is no escaped form to slice.
            return Cow::Owned(digest_form(id, id));
        }
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
        return Cow::Owned(digest_form(id, &out[ESCAPE_PREFIX.len()..]));
    }
    Cow::Owned(out)
}

/// Fold an over-long id to `esct_<prefix>_<digest>`, exactly
/// [`MAX_TOOL_ID_LEN`] bytes. `body` is the text the retained prefix is
/// sliced from -- the escaped body on the escape path, the raw id itself
/// on the wire-safe path (already in the target charset, so it needs no
/// encoding). The prefix is for operator legibility only; the digest,
/// always keyed on the RAW id, carries the distinctness, so the same raw
/// id folds identically at every call site. `body` is pure ASCII by
/// construction, so the prefix slice is on a char boundary.
fn digest_form(raw: &str, body: &str) -> String {
    let keep = MAX_TOOL_ID_LEN - DIGEST_PREFIX.len() - 1 - DIGEST_HEX_LEN;
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
    fn double_application_is_not_a_no_op_by_design() {
        // The inequality below is the INTENDED behavior, not a bug to
        // "fix" into idempotence: idempotence would mean passing an id
        // that already carries a marker prefix through unchanged, and
        // then a raw client id `esc_x` would collide with the escaped
        // form of `x`. Non-idempotence is what keeps the mapping
        // injective, so every call site must pass a RAW id.
        let raw = "call.foo:1";
        let once = sanitize_tool_id(raw).into_owned();
        let twice = sanitize_tool_id(&once).into_owned();

        assert_eq!(once, "esc_call_2efoo_3a1");
        assert_eq!(twice, "esc_esc_5fcall_5f2efoo_5f3a1");
        assert_ne!(once, twice, "sanitization must NOT be idempotent");

        // The prefix check is what preserves disjointness: an
        // already-escaped-LOOKING raw id is escaped again rather than
        // passed through.
        assert_eq!(sanitize_tool_id("esc_x"), "esc_esc_5fx");
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
    fn long_wire_safe_id_folds_to_the_digest_form_within_the_ceiling() {
        // The ceiling is Bedrock's `toolUseId` max length, which applies
        // to any emitted id -- not just to an escape expansion. An
        // over-long id already in the target charset used to pass through
        // verbatim and 400 the upstream.
        let id = "a".repeat(MAX_TOOL_ID_LEN + 1);

        // Act
        let out = sanitize_tool_id(&id);

        // Assert
        assert!(out.starts_with(DIGEST_PREFIX), "got `{out}`");
        assert_eq!(out.len(), MAX_TOOL_ID_LEN);
        assert!(out.bytes().all(is_allowed_tool_id_byte));
    }

    #[test]
    fn wire_safe_id_exactly_at_the_ceiling_still_borrows_unchanged() {
        // Arrange -- the boundary: 64 bytes is legal, 65 is not.
        let id = "a".repeat(MAX_TOOL_ID_LEN);

        // Act
        let out = sanitize_tool_id(&id);

        // Assert
        assert!(matches!(out, Cow::Borrowed(_)), "must not allocate");
        assert_eq!(out, id);
    }

    #[test]
    fn over_long_wire_safe_use_and_result_fold_identically() {
        // Correlation: a tool_use id and the tool_result answering it are
        // sanitized at different call sites with no shared state, so the
        // fold must be a pure function of the raw id. Asserting
        // `f(x) == f(x)` alone is tautological and passes even under a raw
        // passthrough, so pin the folded SHAPE too: prefix, exact ceiling
        // length, and that the raw id is not what reaches the wire.
        let id = "a".repeat(200);
        let use_id = sanitize_tool_id(&id).into_owned();
        let result_id = sanitize_tool_id(&id).into_owned();

        assert_eq!(use_id, result_id, "the fold must be deterministic");
        assert!(
            use_id.starts_with(DIGEST_PREFIX),
            "an over-ceiling id must reach the digest namespace; got {use_id}"
        );
        assert_eq!(
            use_id.len(),
            MAX_TOOL_ID_LEN,
            "the fold sits AT the ceiling"
        );
        assert_ne!(use_id, id, "the raw over-ceiling id must not pass through");
    }

    #[test]
    fn over_long_wire_safe_ids_stay_distinct_past_the_retained_prefix() {
        // Arrange -- identical for the whole retained prefix, so only the
        // digest separates them.
        let a = format!("{}a", "z".repeat(MAX_TOOL_ID_LEN));
        let b = format!("{}b", "z".repeat(MAX_TOOL_ID_LEN));

        // Act
        let out_a = sanitize_tool_id(&a).into_owned();
        let out_b = sanitize_tool_id(&b).into_owned();

        // Assert -- distinctness is necessary but NOT sufficient: two raw
        // passthroughs are also distinct. Require the digest form as well,
        // so reverting the fold fails this test.
        assert_ne!(out_a, out_b, "the digest must separate them");
        for out in [&out_a, &out_b] {
            assert!(
                out.starts_with(DIGEST_PREFIX),
                "must be folded, not passed through; got {out}"
            );
            assert_eq!(out.len(), MAX_TOOL_ID_LEN);
        }
    }

    #[test]
    fn folded_wire_safe_id_never_equals_a_folded_escaped_id() {
        // The two folds share the digest namespace, so the retained
        // prefix must keep them apart: the escaped fold's prefix is the
        // escaped body, the wire-safe fold's is the raw id.
        let wire_safe = "a".repeat(MAX_TOOL_ID_LEN + 1);
        let needs_escape = ".".repeat(MAX_TOOL_ID_LEN + 1);

        let out_safe = sanitize_tool_id(&wire_safe).into_owned();
        let out_escaped = sanitize_tool_id(&needs_escape).into_owned();

        assert_ne!(out_safe, out_escaped);
        // Both must actually be FOLDED -- under a raw passthrough the
        // wire-safe side is never folded and the inequality above holds
        // vacuously.
        assert!(out_safe.starts_with(DIGEST_PREFIX), "got {out_safe}");
        assert!(out_escaped.starts_with(DIGEST_PREFIX), "got {out_escaped}");
        assert_eq!(out_safe.len(), MAX_TOOL_ID_LEN);
        assert_eq!(out_escaped.len(), MAX_TOOL_ID_LEN);
    }
}
