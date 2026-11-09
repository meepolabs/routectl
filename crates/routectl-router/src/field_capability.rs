//! The envelope-field capability namespace: one prefix, one bounded
//! constructor, one namespace predicate.
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
//!   way to test one is [`capability_key_is_field_verdict`]. A sibling
//!   module cannot assemble a key by hand.
//! - **The path is qualified, and preserved byte for byte.** The key
//!   carries the full dotted path the upstream named, never its leaf
//!   segment: two structurally distinct fields can share a leaf name, and
//!   because the token is permanent such a collision could not be
//!   un-minted -- it would become two different facts sharing one row of
//!   history.
//!
//! [`capability_key_is_field_verdict`] is the read-side counterpart: the
//! namespace test every surface that reports or resets field verdicts
//! applies. [`capability_key_is_catalog_scoped`] is a separate question,
//! asked only by the revision and generation boundary paths, and its
//! answer is now `false` for every key.

use routectl_core::capability::normalize_capability_key;

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
/// the grammar's already-a-key rule and the field-verdict predicate.
fn field_capability_path(key: &str) -> Option<&str> {
    key.strip_prefix(FIELD_CAPABILITY_PREFIX)
}

/// True when `key` names a fact whose truth is scoped to a catalog revision,
/// which the revision and generation boundary paths would then discard when
/// that revision changes.
///
/// `false` for every key. The revision stamps are frozen constants, and no
/// learned verdict depends on catalog data, so a ledger written under any
/// earlier revision carries every verdict forward. Asked only by the boundary
/// paths; a caller asking whether a key is a field verdict uses
/// [`capability_key_is_field_verdict`], which is not this predicate's
/// complement.
pub const fn capability_key_is_catalog_scoped(_key: &str) -> bool {
    false
}

/// True when `key` names an envelope-field wire-shape fact.
///
/// The test every surface that reports, seeds or resets field verdicts
/// applies, and what the daemon's event-drain needs in order to route a field
/// verdict's row through the ACKNOWLEDGED write path (its pre-flight
/// eligibility depends on the row being durable) while every other capability
/// row stays best effort.
///
/// This is a NAMESPACE test rather than a validity test: it answers only
/// whether the key sits inside the field namespace, and never re-checks the
/// grammar of a key already persisted.
pub fn capability_key_is_field_verdict(key: &str) -> bool {
    field_capability_path(key).is_some()
}

/// True when any `.`-separated segment of `token` sits in the field namespace.
///
/// The question a producer of a NON-field key has to ask about upstream or
/// client text. Testing the whole token is not enough: a capability-key
/// normalizer may reduce a dotted token to one of its segments (Bedrock keeps
/// the head, or the segment after a request-bag prefix), so
/// `toolConfig.<field key>` would be stored as the field key it embeds.
pub fn token_reaches_field_namespace(token: &str) -> bool {
    token
        .split(SEGMENT_SEPARATOR)
        .any(capability_key_is_field_verdict)
}

/// True when a key that did NOT come from the field-repair guard would land in
/// the field namespace: either as written, or once `provider_kind`'s
/// capability-key normalization has run -- which is what the registry stores
/// and the ledger replays.
pub fn learned_key_enters_field_namespace(raw: &str, provider_kind: &str) -> bool {
    capability_key_is_field_verdict(raw)
        || capability_key_is_field_verdict(&normalize_capability_key(raw, provider_kind))
}

#[cfg(test)]
#[path = "field_capability_tests.rs"]
mod tests;
