//! Session-keyed last-fingerprint store for the shadow misfire monitor.
//!
//! Mirrors the `KSessionStore` shape (bounded LRU map behind a
//! `parking_lot::Mutex`). Keyed by the same
//! (session_key, provider_kind, model) triple so the shadow monitor is
//! independent from the K estimator but uses the same identity semantics.
//!
//! Each entry stores the last trimmed-prefix fingerprint observed for a
//! triple. On every turn the monitor compares the new fingerprint against
//! the stored one and returns a `ShadowOutcome`: `FirstSeen` when no prior
//! record exists, `Stable` when they match, `Misfire` when they differ. A
//! Misfire means the trimmed cacheable prefix changed turn-to-turn -- the
//! canary that a real cut would break the upstream cache.
//!
//! Recording only: the monitor NEVER mutates a dispatched request.

use std::num::NonZeroUsize;
use std::time::SystemTime;

use lru::LruCache;
use parking_lot::Mutex;

use super::store::KSessionKey;

/// Bound on the number of distinct (session, provider_kind, model) triples
/// tracked at once. Matches `K_SESSION_CAPACITY` so a session alive in the
/// K-estimator store also has a shadow entry.
const SHADOW_CAPACITY: usize = super::store::K_SESSION_CAPACITY;

/// Outcome of a single `record_and_compare` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowOutcome {
    /// No prior fingerprint for this triple. The new fingerprint was stored;
    /// no misfire verdict is emitted.
    FirstSeen,
    /// The fingerprint matches the stored value: the trimmed cacheable prefix
    /// is byte-stable across turns. Advisory 0.
    Stable,
    /// The fingerprint DIFFERS from the stored value: the trimmed cacheable
    /// prefix shifted turn-to-turn. The stored value is updated to the new
    /// fingerprint. Advisory 1.
    Misfire,
}

/// One entry in the shadow store: the last observed fingerprint and the
/// timestamp when it was first recorded.
#[derive(Debug, Clone)]
struct ShadowEntry {
    fingerprint: u64,
    /// Wall-clock time the entry was last updated (informational).
    ts: SystemTime,
}

/// Bounded LRU map of `KSessionKey` -> `ShadowEntry`.
///
/// Interior-mutable via `parking_lot::Mutex` so the store can be read and
/// written on the `&self` dispatch path. Held on `Router` behind an `Arc`
/// like `k_session_store`.
pub struct ShadowStore {
    entries: Mutex<LruCache<KSessionKey, ShadowEntry>>,
}

impl Default for ShadowStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ShadowStore {
    /// Construct an empty store bounded at `SHADOW_CAPACITY`.
    pub fn new() -> Self {
        let cap = NonZeroUsize::new(SHADOW_CAPACITY).expect("SHADOW_CAPACITY > 0");
        Self {
            entries: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Compare `fingerprint` against the stored value for `key` and update
    /// the store, returning the outcome:
    ///
    /// - `FirstSeen`: no prior entry -- store and return.
    /// - `Stable`: fingerprints match -- no update needed, return.
    /// - `Misfire`: fingerprints differ -- update stored to new, return.
    pub fn record_and_compare(
        &self,
        key: KSessionKey,
        fingerprint: u64,
        ts: SystemTime,
    ) -> ShadowOutcome {
        let mut guard = self.entries.lock();
        if let Some(entry) = guard.get_mut(&key) {
            if entry.fingerprint == fingerprint {
                ShadowOutcome::Stable
            } else {
                entry.fingerprint = fingerprint;
                entry.ts = ts;
                ShadowOutcome::Misfire
            }
        } else {
            guard.put(key, ShadowEntry { fingerprint, ts });
            ShadowOutcome::FirstSeen
        }
    }

    /// Number of triples currently tracked.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// True when no triples are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn key(session: &str, kind: &str, model: &str) -> KSessionKey {
        KSessionKey {
            session_key: session.into(),
            provider_kind: kind.into(),
            model: model.into(),
        }
    }

    #[test]
    fn first_call_returns_first_seen() {
        // Arrange
        let store = ShadowStore::new();
        let k = key("sess-1", "anthropic-api", "opus");

        // Act
        let outcome = store.record_and_compare(k, 0xdeadbeef, UNIX_EPOCH);

        // Assert
        assert_eq!(outcome, ShadowOutcome::FirstSeen);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn matching_fingerprint_returns_stable() {
        // Arrange
        let store = ShadowStore::new();
        let k = key("sess-1", "anthropic-api", "opus");
        store.record_and_compare(k.clone(), 0xdeadbeef, UNIX_EPOCH);

        // Act
        let outcome = store.record_and_compare(k, 0xdeadbeef, UNIX_EPOCH);

        // Assert
        assert_eq!(outcome, ShadowOutcome::Stable);
    }

    #[test]
    fn different_fingerprint_returns_misfire_and_updates_stored() {
        // Arrange
        let store = ShadowStore::new();
        let k = key("sess-1", "anthropic-api", "opus");
        store.record_and_compare(k.clone(), 0xdeadbeef, UNIX_EPOCH);

        // Act
        let outcome = store.record_and_compare(k.clone(), 0xcafebabe, UNIX_EPOCH);

        // Assert
        assert_eq!(outcome, ShadowOutcome::Misfire);

        // A third call with the NEW fingerprint must return Stable (the store
        // updated on Misfire).
        let follow_up = store.record_and_compare(k, 0xcafebabe, UNIX_EPOCH);
        assert_eq!(follow_up, ShadowOutcome::Stable);
    }

    #[test]
    fn distinct_triples_are_independent() {
        // Arrange: two triples sharing the session but differing by
        // provider_kind; their fingerprints must not bleed into each other.
        let store = ShadowStore::new();
        let k_api = key("sess-1", "anthropic-api", "opus");
        let k_bed = key("sess-1", "bedrock", "opus");

        // Act: seed both with different fingerprints.
        store.record_and_compare(k_api.clone(), 0x11, UNIX_EPOCH);
        store.record_and_compare(k_bed.clone(), 0x22, UNIX_EPOCH);

        // The api triple gets a new fingerprint -> Misfire.
        let api_out = store.record_and_compare(k_api, 0x33, UNIX_EPOCH);
        // The bedrock triple gets the same fingerprint -> Stable.
        let bed_out = store.record_and_compare(k_bed, 0x22, UNIX_EPOCH);

        // Assert
        assert_eq!(api_out, ShadowOutcome::Misfire);
        assert_eq!(bed_out, ShadowOutcome::Stable);
    }
}
