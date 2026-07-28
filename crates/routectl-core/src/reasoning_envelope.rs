//! Self-describing envelope for a reasoning artifact crossing a dialect
//! that cannot represent its id and scheme.
//!
//! When a reasoning artifact produced by one lane is rendered toward a
//! client speaking a dialect with no slot for the artifact's item id or
//! its scheme, both are lost: the blob flattens to an opaque field. When
//! the client later echoes that blob back, nothing on the wire says what
//! it is, so the artifact cannot be replayed onto the lane that issued
//! it and reasoning continuity is destroyed.
//!
//! This codec makes the blob self-describing so the round trip survives
//! with NO server-side state. Being stateless is the point: continuity
//! survives a restart, an unbounded session length, and several router
//! instances behind a balancer without session affinity -- none of which
//! a server-side recovery table can offer.
//!
//! # The envelope is a HINT, never an authorization
//!
//! [`unwrap`] parses CLIENT-CONTROLLED bytes. Anyone can mint a string
//! claiming any scheme and any id. The `(scheme, id)` it returns is a
//! HINT ONLY: it says what the bytes CLAIM to be, never what they are
//! and never what may be done with them. Callers MUST run the same
//! carry-vs-strip policy on an unwrapped result as on a natively tagged
//! artifact -- a claim that a blob belongs to some lane can never be
//! what admits it to that lane. This module deliberately holds no
//! policy; it only parses.
//!
//! Parsing is therefore TOTAL. Malformed input, an absent or unknown
//! version prefix, and any non-matching shape all return `None`, which
//! leaves the caller in exactly the state it would have been in without
//! this module: holding an opaque foreign blob. It never errors and
//! never panics.
//!
//! # Format
//!
//! ```text
//! rctl1.<scheme>.<id>.<blob>
//! ```
//!
//! The version prefix is a CLOSED set -- an unrecognized version is a
//! non-match, not a best-effort parse, so a future format can be added
//! without any older reader mis-reading it.
//!
//! `<blob>` is the remainder of the string after the third separator and
//! rides through UNTOUCHED whatever its alphabet, including separator
//! characters of its own. [`wrap`] copies it verbatim and [`unwrap`]
//! returns a borrowed slice of the input, so a wrapped blob is returned
//! byte-identical to the bytes the provider issued. That byte-identity
//! is load-bearing: replayed artifacts must reach the provider unchanged
//! or prompt-cache affinity breaks.
//!
//! # Separator invariant
//!
//! `.` is the separator because no artifact family routectl carries uses
//! it: probed content-prefixed blobs and dialect-native signatures
//! contain no `.`, and Fernet-shaped blobs are base64url, which excludes
//! `.` by construction. The probed prefixes this rests on are pinned by
//! a round-trip test rather than by prose.
//!
//! The invariant is a collision-avoidance argument, not a safety
//! requirement. Because the version prefix is closed and the leading
//! fields are constrained tokens, a blob that ever did contain `.`
//! degrades to `None` -- the safe direction.
//!
//! # Log hygiene
//!
//! Blob bytes, artifact ids, and any digest of either are never logged
//! at any level, here or in any caller.

/// Closed-set version prefix of the only envelope format this build
/// emits and the only one it recognizes.
const VERSION_1: &str = "rctl1";

/// Field separator. See the module-level separator invariant.
const SEPARATOR: char = '.';

/// Number of fields the envelope splits into: version, scheme, id, and
/// the blob remainder.
const FIELD_COUNT: usize = 4;

/// Id field standing for "this artifact carried no id".
///
/// Not every artifact has a recoverable id, and one lane family validates
/// content while ignoring the id entirely -- such an artifact is fully
/// replayable without one. An explicit sentinel keeps the id field a
/// strict token (so a hostile envelope still cannot smuggle a separator
/// or control character through it) while letting absence round-trip
/// instead of collapsing the whole envelope to an opaque blob and losing
/// the scheme along with it.
///
/// The value is RESERVED: [`wrap`] takes `Option<&str>` so absence is
/// stated rather than inferred, and a present id equal to this sentinel
/// is rejected rather than silently collapsed into absence. No observed
/// provider mints a bare `-` id, but inferring absence from a value a
/// provider could legitimately mint would silently drop a real id and
/// turn a replayable artifact into a rejected one.
const ID_ABSENT: &str = "-";

/// Upper bound on a scheme or id field, in bytes.
///
/// Both fields are CLIENT-CONTROLLED and are copied by callers into
/// comparisons, learned-state keys, and metrics labels. Real values are
/// short closed-vocabulary tokens; without a bound, a hostile envelope
/// could carry body-sized fields into those surfaces and inflate key
/// cardinality. Generous enough that no legitimate value approaches it.
const MAX_FIELD_BYTES: usize = 128;

/// Blob prefixes observed on the wire that the separator choice rests
/// on: none of these families contains the separator anywhere in its
/// payload. Exposed so the invariant is pinned by a test rather than
/// living only in prose.
#[cfg(test)]
const SEPARATOR_ABSENT_FROM_PROBED_BLOBS: [&str; 4] = ["rsn_", "smry_", "CAIS", "Erk"];

/// Returns `true` when `field` is usable as an envelope scheme or id.
///
/// Constrained to a conservative token alphabet and a length bound so
/// that a hostile or simply malformed envelope cannot smuggle
/// separators, whitespace, or control characters into a field a caller
/// may go on to compare, key, or surface, nor inflate one into a
/// body-sized value.
fn is_token(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= MAX_FIELD_BYTES
        && field
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Wraps `blob` in a self-describing envelope carrying `scheme_tag` and
/// `id`.
///
/// `blob` is copied verbatim; no re-encoding of any kind is applied.
///
/// `id` is `None` for an artifact that carried none; it comes back from
/// [`unwrap`] as `None`, so the scheme still survives the round trip.
///
/// `scheme_tag` and a `Some` id must be ASCII alphanumeric / `-` / `_`
/// bounded-length tokens, and a `Some` id may not be the reserved
/// absent-id sentinel. Callers draw both from closed
/// vocabularies that satisfy this, so it is not a runtime check: a
/// violating value simply produces a string [`unwrap`] rejects,
/// degrading to an opaque foreign blob rather than to a mis-parse.
/// Wrapping an empty `blob` is likewise rejected on the way back, since
/// an artifact with no content carries nothing to replay.
pub fn wrap(scheme_tag: &str, id: Option<&str>, blob: &str) -> String {
    // A present id equal to the sentinel would be indistinguishable from
    // absence on the wire. Emit a field `unwrap` rejects outright rather
    // than one it would silently read as "no id": dropping a real id can
    // turn a replayable artifact into a rejected one.
    let id = match id {
        Some(ID_ABSENT) => "",
        Some(id) => id,
        None => ID_ABSENT,
    };
    format!("{VERSION_1}{SEPARATOR}{scheme_tag}{SEPARATOR}{id}{SEPARATOR}{blob}")
}

/// Parses an envelope, returning the claimed `(scheme, id, blob)`.
///
/// Returns `None` for every input that is not exactly a well-formed
/// envelope of a recognized version: empty input, an unwrapped provider
/// blob, a truncated or separator-corrupted envelope, an unknown version
/// prefix, a non-token or over-long scheme or id, and an empty blob.
/// Total: it never errors and never panics.
///
/// The `id` is `None` when the artifact carried none; the scheme still
/// applies.
///
/// The returned `scheme` and `id` are the envelope's CLAIM about itself
/// and carry no authority -- see the module documentation. `blob` is a
/// slice of `envelope` and is therefore byte-identical to the wrapped
/// bytes.
pub fn unwrap(envelope: &str) -> Option<(&str, Option<&str>, &str)> {
    let mut fields = envelope.splitn(FIELD_COUNT, SEPARATOR);
    let version = fields.next()?;
    let scheme = fields.next()?;
    let id = fields.next()?;
    let blob = fields.next()?;

    if version != VERSION_1 || !is_token(scheme) || !is_token(id) || blob.is_empty() {
        return None;
    }

    let id = if id == ID_ABSENT { None } else { Some(id) };
    Some((scheme, id, blob))
}

#[cfg(test)]
#[path = "reasoning_envelope_tests.rs"]
mod tests;
