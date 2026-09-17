//! The envelope-field capability namespace: one prefix, one bounded
//! constructor, one catalog-scope predicate.
//!
//! An upstream that rejects a request by naming a WIRE FIELD PATH
//! (`thinking.enabled.display`) states a fact about the request envelope
//! that lane accepts, not about a catalog-listed capability. Such a fact
//! is recorded in the existing learned-capability key space as
//! `field:<qualified.dotted.path>`, so the registry, the events ledger,
//! the warm rebuild and the doctor surfaces all carry it unchanged --
//! there is no second store and no schema change.
//!
//! Two properties make this module the single owner of that namespace:
//!
//! - **The prefix is permanent.** A minted key is written verbatim to an
//!   append-only ledger and read back on every later boot, so it can only
//!   ever be superseded, never renamed or reinterpreted. A second copy of
//!   the prefix literal anywhere else could drift from this one and
//!   re-partition history, so the prefix is private to this module: the
//!   only way to obtain a key is [`field_capability_key`], and the only
//!   way to test one is [`capability_key_is_catalog_scoped`]. A sibling
//!   module cannot assemble a key by hand.
//! - **The path is qualified, and preserved byte for byte.** The key
//!   carries the full dotted path the upstream named, never its leaf
//!   segment: two structurally distinct fields can share a leaf name, and
//!   because the token is permanent such a collision could not be
//!   un-minted -- it would become two different facts sharing one row of
//!   history.
//!
//! [`capability_key_is_catalog_scoped`] is the read-side counterpart. A
//! catalog-scoped fact is only meaningful under the catalog revision that
//! was live when it was observed, so the invalidation paths discard it on
//! a revision change. A wire-shape fact is independent of the catalog and
//! must survive that change, so the two classes are distinguished by one
//! predicate rather than by each call site re-deciding.

/// The permanent prefix of every envelope-field capability key.
///
/// Private on purpose: a sibling module holding the prefix could assemble
/// a key that never passed [`field_capability_key`]'s grammar, and every
/// such key would be permanent. The namespace has exactly one spelling and
/// exactly one constructor.
const FIELD_CAPABILITY_PREFIX: &str = "field:";

/// Ceiling on the dotted path inside a field capability key.
///
/// A real envelope path is a handful of short segments
/// (`thinking.enabled.display` is 24 bytes). The cap only bounds what a
/// buggy or adversarial upstream can push into a permanent key and into
/// the operator-visible surfaces that render it.
const MAX_FIELD_PATH_BYTES: usize = 128;

/// Separator between segments of a qualified envelope path, as the
/// upstream error spells it.
const SEGMENT_SEPARATOR: char = '.';

/// Build the capability key for the qualified dotted envelope path an
/// upstream rejection named, or `None` when `path` is not a well-formed
/// path.
///
/// The accepted grammar is one or more non-empty dot-separated segments of
/// printable ASCII, bounded by [`MAX_FIELD_PATH_BYTES`]. An accepted path
/// is appended to the prefix unchanged, so the minted key preserves the
/// upstream's spelling byte for byte -- normalization would fuse distinct
/// fields onto one permanent token.
///
/// Rejected: an empty path, an empty segment (which covers a leading or
/// trailing dot and any run of dots), any byte outside printable ASCII
/// (whitespace, control bytes, and every multi-byte sequence), a path over
/// the cap, and a path that is itself already a key -- the prefix is
/// printable ASCII, so without that last rule a caller passing a key back
/// in would mint a doubled-prefix token no reader could attribute, and the
/// token being permanent means it could not be un-minted.
pub fn field_capability_key(path: &str) -> Option<String> {
    is_qualified_field_path(path).then(|| format!("{FIELD_CAPABILITY_PREFIX}{path}"))
}

/// True when `path` is a well-formed qualified envelope path: outside the
/// key namespace, bounded, and one or more non-empty dot-separated
/// segments of printable ASCII (`is_ascii_graphic` is the printable range
/// excluding space, so it rejects whitespace, control bytes and every
/// multi-byte sequence at once).
fn is_qualified_field_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_FIELD_PATH_BYTES
        && field_capability_path(path).is_none()
        && path
            .split(SEGMENT_SEPARATOR)
            .all(|segment| !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_graphic()))
}

/// The dotted path inside a field capability key, or `None` for a key
/// outside the namespace. The exact inverse of [`field_capability_key`]'s
/// append: it returns the stored bytes, so a round trip is lossless.
///
/// Private: no caller outside this module needs the path, and every one
/// that has the prefix could hand-assemble a key. The two callers here are
/// the grammar's already-a-key rule and the catalog-scope predicate.
fn field_capability_path(key: &str) -> Option<&str> {
    key.strip_prefix(FIELD_CAPABILITY_PREFIX)
}

/// True when `key` names a fact whose truth is scoped to the live catalog
/// revision, and which the invalidation paths must therefore discard when
/// that revision changes.
///
/// Defaults to `true`: every known catalog capability key and every key
/// from a namespace this build does not recognize is treated as
/// catalog-scoped, so a new producer inherits the conservative behavior
/// rather than accidental permanence. Only the field namespace is carved
/// out, because a wire-shape fact does not depend on the catalog at all.
///
/// This is a namespace test, not a validity test: it answers only whether
/// the key sits inside the field namespace, and never re-checks the
/// grammar of a key already persisted.
pub fn capability_key_is_catalog_scoped(key: &str) -> bool {
    field_capability_path(key).is_none()
}

#[cfg(test)]
#[path = "field_capability_tests.rs"]
mod tests;
