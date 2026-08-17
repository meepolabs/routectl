//! Per-process salted digest of a ledger session key, so a read query can
//! report session IDENTITY (this group of rows shares one session) without
//! ever handing a raw client-supplied session key to a caller that renders
//! it.
//!
//! Same construction and the same guarantee as the router's log-correlation
//! hash, duplicated rather than imported because this crate is a leaf and
//! must not depend on the router: stable for the life of the process, so an
//! operator can tell two reported groups apart within one report;
//! unpredictable across processes, so a digest that reaches a terminal, a
//! pasted issue, or a log archive is not invertible offline against a
//! dictionary of guessable session keys (a client is free to key its session
//! by an email address).
//!
//! Never a fingerprint: nothing may persist one of these values or compare
//! one across runs.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::OnceLock;

/// Process-lifetime hash seed. One instance for the whole process makes
/// every derived digest comparable within a run and unpredictable across
/// runs.
static SESSION_REF_SEED: OnceLock<RandomState> = OnceLock::new();

/// Opaque per-process reference for a raw session key.
pub(super) fn session_ref(session_id: &str) -> u64 {
    SESSION_REF_SEED
        .get_or_init(RandomState::new)
        .hash_one(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Grouping is the whole point: two rows carrying the same session key
    /// must report the same reference within one run.
    #[test]
    fn the_same_session_key_maps_to_the_same_reference() {
        assert_eq!(session_ref("sess-a"), session_ref("sess-a"));
    }

    #[test]
    fn distinct_session_keys_map_to_distinct_references() {
        assert_ne!(session_ref("sess-a"), session_ref("sess-b"));
    }
}
