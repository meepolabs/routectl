//! Session-keyed in-memory cache for K estimator samples.
//!
//! Sibling to [`crate::seat_pool::StickyPins`]: same `parking_lot::Mutex<
//! lru::LruCache>` shape, same capacity, same carry-over discipline. Each
//! entry is a bounded ring of recent reuse observations for one
//! (session, provider_kind, model) triple; the triple key prevents K from
//! leaking across providers or models inside a long-lived session.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::time::SystemTime;

use lru::LruCache;
use parking_lot::Mutex;

/// Bound on the number of distinct (session, provider_kind, model) triples
/// tracked at once. Matches [`crate::seat_pool::STICKY_PIN_CAPACITY`] so a
/// session that is alive in the sticky-pin map also has a live K window.
pub const K_SESSION_CAPACITY: usize = 4096;

/// Bound on the number of samples retained per session window. The cap is
/// load-bearing: at scale the per-process memory floor is
/// `K_SESSION_CAPACITY * SAMPLES_PER_WINDOW` samples regardless of traffic
/// shape. Picked to give the estimator enough recent history to set a
/// defensible confidence floor without unbounded growth on a chatty session.
const SAMPLES_PER_WINDOW: usize = 32;

/// Triple-keyed identity for a per-session K window.
///
/// Including `provider_kind` and `model` alongside `session_key` is
/// load-bearing: a session that fails over from one provider-kind to
/// another (or switches model) has fundamentally different cache
/// economics on each, and a single-key map would silently bleed the
/// observed reuse rate from one onto the other.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KSessionKey {
    /// Inbound session identifier (the same key used for sticky-seat pins).
    pub session_key: String,
    /// Stable provider-kind token of the served target.
    pub provider_kind: String,
    /// Served model nickname.
    pub model: String,
}

/// One observation of cache reuse behavior on a dispatched request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// Wall-clock time the sample was recorded. Used by the estimator to
    /// age out observations older than the cache prefix TTL.
    pub ts: SystemTime,
    /// Did the dispatch observe a non-zero cache_read on the upstream
    /// response? The estimator weights these into the floor/point/ceiling.
    pub observed_reuse: bool,
}

/// Bounded ring of recent samples for one session triple.
///
/// New samples are pushed to the back via [`KSessionWindow::push`]; once
/// the window is at [`SAMPLES_PER_WINDOW`] the oldest is dropped. The
/// invariant `len() <= SAMPLES_PER_WINDOW` is enforced inside `push` so
/// no caller can grow the window past the cap by accident.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KSessionWindow {
    samples: VecDeque<Sample>,
}

impl KSessionWindow {
    /// Construct an empty window.
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(SAMPLES_PER_WINDOW),
        }
    }

    /// Append a sample, dropping the oldest one if the window is full.
    pub fn push(&mut self, sample: Sample) {
        if self.samples.len() == SAMPLES_PER_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Current number of samples retained.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when no samples have been recorded.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Borrowed iterator over the retained samples in arrival order
    /// (oldest first, newest last). The estimator uses this when scoring.
    pub fn iter(&self) -> impl Iterator<Item = &Sample> {
        self.samples.iter()
    }
}

/// Bounded LRU map of [`KSessionKey`] -> [`KSessionWindow`], sibling to
/// [`crate::seat_pool::StickyPins`].
///
/// Wraps a `parking_lot::Mutex<LruCache<..>>` for interior mutability so
/// the map is read/written on the `&self` dispatch path. Carried over on a
/// Router rebuild (see `Router::carry_over_k_store_from`): dropping windows
/// mid-incident would collapse every learned estimate back to `Cold` and
/// silently un-arm the cost gate.
pub struct KSessionStore {
    sessions: Mutex<LruCache<KSessionKey, KSessionWindow>>,
}

impl Default for KSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KSessionStore {
    /// Construct an empty store bounded at [`K_SESSION_CAPACITY`].
    pub fn new() -> Self {
        let cap = NonZeroUsize::new(K_SESSION_CAPACITY).expect("K_SESSION_CAPACITY > 0");
        Self {
            sessions: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Insert or update the window for `key`, marking it most-recently used.
    pub fn put(&self, key: KSessionKey, window: KSessionWindow) {
        self.sessions.lock().put(key, window);
    }

    /// Return a cloned snapshot of the window for `key`, bumping its LRU
    /// recency under the lock. `None` when no window has been recorded for
    /// the triple.
    pub fn get(&self, key: &KSessionKey) -> Option<KSessionWindow> {
        self.sessions.lock().get(key).cloned()
    }

    /// Append one live `sample` to the window for `key`, creating an empty
    /// window first when the triple is unseen, and bump the triple's LRU
    /// recency. The whole get-or-insert-then-push happens under a single
    /// lock so a concurrent dispatch can never observe a half-updated
    /// window or race two pushes into the same slot.
    ///
    /// This is the live-recording mutator: it mutates the stored window in
    /// place via `get_mut` rather than the `get().cloned()` -> push ->
    /// `put` round-trip, so a chatty session does not clone its whole ring
    /// on every turn.
    pub fn record_sample(&self, key: KSessionKey, sample: Sample) {
        let mut guard = self.sessions.lock();
        if let Some(window) = guard.get_mut(&key) {
            window.push(sample);
        } else {
            let mut window = KSessionWindow::new();
            window.push(sample);
            guard.put(key, window);
        }
    }

    /// Number of live triples currently tracked.
    pub fn len(&self) -> usize {
        self.sessions.lock().len()
    }

    /// True when no triples are tracked.
    pub fn is_empty(&self) -> bool {
        self.sessions.lock().is_empty()
    }

    /// Snapshot all entries in LRU order: least-recently-used FIRST,
    /// most-recently-used LAST. Used for carry-over on a Router rebuild
    /// so the destination map can re-`put` in the same recency order and
    /// the eviction frontier stays consistent across the rebuild.
    /// (`iter` yields MRU->LRU, so the collected order is reversed.)
    pub fn export_entries(&self) -> Vec<(KSessionKey, KSessionWindow)> {
        let guard = self.sessions.lock();
        guard
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .rev()
            .collect()
    }

    /// Re-`put` each entry in iteration order so the LAST one becomes
    /// most-recently-used. When fed the output of [`Self::export_entries`]
    /// against a fresh store this preserves the LRU ordering exactly.
    pub fn import_entries(&self, entries: Vec<(KSessionKey, KSessionWindow)>) {
        let mut guard = self.sessions.lock();
        for (key, window) in entries {
            guard.put(key, window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn key(session: &str, kind: &str, model: &str) -> KSessionKey {
        KSessionKey {
            session_key: session.into(),
            provider_kind: kind.into(),
            model: model.into(),
        }
    }

    fn window_with(n: usize) -> KSessionWindow {
        let mut w = KSessionWindow::new();
        for i in 0..n {
            w.push(Sample {
                ts: UNIX_EPOCH + Duration::from_secs(i as u64),
                observed_reuse: i % 2 == 0,
            });
        }
        w
    }

    #[test]
    fn k_session_window_push_caps_at_samples_per_window() {
        // The cap is load-bearing for memory bounds at scale; a regression
        // here lets a chatty session retain unbounded samples.
        let mut w = KSessionWindow::new();
        for i in 0..(SAMPLES_PER_WINDOW + 5) {
            w.push(Sample {
                ts: UNIX_EPOCH + Duration::from_secs(i as u64),
                observed_reuse: false,
            });
        }
        assert_eq!(w.len(), SAMPLES_PER_WINDOW);
        // The oldest 5 samples were dropped: the front timestamp is now 5,
        // not 0.
        let first = w.iter().next().expect("non-empty window");
        assert_eq!(first.ts, UNIX_EPOCH + Duration::from_secs(5));
    }

    #[test]
    fn k_session_store_get_returns_inserted_window() {
        // Arrange
        let store = KSessionStore::new();
        let k = key("sess-1", "anthropic-api", "opus");
        let w = window_with(3);
        store.put(k.clone(), w.clone());

        // Act + Assert: exact-key hit
        assert_eq!(store.get(&k), Some(w));

        // Single-component mismatches each return None: the triple is
        // load-bearing, a session that switches provider or model must
        // not bleed K from the previous one.
        assert_eq!(
            store.get(&key("sess-1", "bedrock", "opus")),
            None,
            "provider_kind mismatch must miss",
        );
        assert_eq!(
            store.get(&key("sess-1", "anthropic-api", "sonnet")),
            None,
            "model mismatch must miss",
        );
        assert_eq!(
            store.get(&key("sess-2", "anthropic-api", "opus")),
            None,
            "session_key mismatch must miss",
        );
    }

    #[test]
    fn record_sample_creates_then_appends_within_one_triple() {
        // Arrange
        let store = KSessionStore::new();
        let k = key("sess-1", "anthropic-api", "opus");

        // Act: first record creates a 1-sample window.
        store.record_sample(
            k.clone(),
            Sample {
                ts: UNIX_EPOCH + Duration::from_secs(1),
                observed_reuse: true,
            },
        );

        // Assert
        let after_first = store.get(&k).expect("triple created");
        assert_eq!(after_first.len(), 1);
        assert!(after_first.iter().next().expect("sample").observed_reuse);

        // Act: a second record appends to the SAME window.
        store.record_sample(
            k.clone(),
            Sample {
                ts: UNIX_EPOCH + Duration::from_secs(2),
                observed_reuse: false,
            },
        );

        // Assert: two samples, in arrival order.
        let after_second = store.get(&k).expect("triple present");
        assert_eq!(after_second.len(), 2);
        let reuse: Vec<bool> = after_second.iter().map(|s| s.observed_reuse).collect();
        assert_eq!(reuse, vec![true, false]);
    }

    #[test]
    fn record_sample_respects_per_window_cap() {
        // Arrange: record more than the window cap into one triple.
        let store = KSessionStore::new();
        let k = key("sess-1", "anthropic-api", "opus");

        // Act
        for i in 0..(SAMPLES_PER_WINDOW + 10) {
            store.record_sample(
                k.clone(),
                Sample {
                    ts: UNIX_EPOCH + Duration::from_secs(i as u64),
                    observed_reuse: false,
                },
            );
        }

        // Assert: the window stays capped and dropped the oldest entries.
        let window = store.get(&k).expect("triple present");
        assert_eq!(window.len(), SAMPLES_PER_WINDOW);
        let oldest = window.iter().next().expect("non-empty window");
        assert_eq!(oldest.ts, UNIX_EPOCH + Duration::from_secs(10));
    }

    #[test]
    fn record_sample_keeps_distinct_triples_separate() {
        // Arrange + Act: three triples differing by one component each.
        let store = KSessionStore::new();
        let mk = |k: KSessionKey| {
            store.record_sample(
                k,
                Sample {
                    ts: UNIX_EPOCH,
                    observed_reuse: true,
                },
            );
        };
        mk(key("s1", "anthropic-api", "opus"));
        mk(key("s1", "bedrock", "opus"));
        mk(key("s1", "anthropic-api", "sonnet"));

        // Assert: three independent windows, each carrying one sample.
        assert_eq!(store.len(), 3);
        assert_eq!(
            store
                .get(&key("s1", "anthropic-api", "opus"))
                .map(|w| w.len()),
            Some(1)
        );
        assert_eq!(
            store.get(&key("s1", "bedrock", "opus")).map(|w| w.len()),
            Some(1)
        );
        assert_eq!(
            store
                .get(&key("s1", "anthropic-api", "sonnet"))
                .map(|w| w.len()),
            Some(1)
        );
    }

    #[test]
    fn record_sample_bumps_lru_recency() {
        // Arrange: fill to capacity so the next insert forces an eviction,
        // then record into the oldest triple to promote it to MRU.
        let store = KSessionStore::new();
        for i in 0..K_SESSION_CAPACITY {
            store.record_sample(
                key(&format!("sess-{i}"), "anthropic-api", "opus"),
                Sample {
                    ts: UNIX_EPOCH,
                    observed_reuse: false,
                },
            );
        }
        let oldest = key("sess-0", "anthropic-api", "opus");

        // Act: recording into the LRU triple bumps it to MRU.
        store.record_sample(
            oldest.clone(),
            Sample {
                ts: UNIX_EPOCH + Duration::from_secs(1),
                observed_reuse: true,
            },
        );
        // Now the new LRU is sess-1; an overflow insert must evict THAT,
        // not the just-bumped sess-0.
        store.record_sample(
            key("sess-overflow", "anthropic-api", "opus"),
            Sample {
                ts: UNIX_EPOCH,
                observed_reuse: false,
            },
        );

        // Assert: the bumped triple survived; the new LRU was evicted.
        assert_eq!(store.len(), K_SESSION_CAPACITY);
        assert!(
            store.get(&oldest).is_some(),
            "recording bumped the LRU triple to MRU, so it must survive the overflow"
        );
        assert_eq!(
            store.get(&key("sess-1", "anthropic-api", "opus")),
            None,
            "the post-bump LRU triple must be the one evicted"
        );
    }

    #[test]
    fn k_session_store_evicts_lru_at_capacity() {
        // Arrange: fill exactly to capacity, then add one more. The very
        // first inserted (LRU) must be evicted.
        let store = KSessionStore::new();
        for i in 0..K_SESSION_CAPACITY {
            store.put(
                key(&format!("sess-{i}"), "anthropic-api", "opus"),
                window_with(1),
            );
        }
        assert_eq!(store.len(), K_SESSION_CAPACITY);
        let first = key("sess-0", "anthropic-api", "opus");
        assert!(store.get(&first).is_some(), "sentinel still present");

        // The probe above bumped sess-0 to MRU. Reset and re-arrange so the
        // FIRST inserted truly is LRU at the moment of the overflow put.
        let store = KSessionStore::new();
        for i in 0..K_SESSION_CAPACITY {
            store.put(
                key(&format!("sess-{i}"), "anthropic-api", "opus"),
                window_with(1),
            );
        }

        // Act
        store.put(
            key("sess-overflow", "anthropic-api", "opus"),
            window_with(1),
        );

        // Assert: capacity held; the first inserted is now gone.
        assert_eq!(store.len(), K_SESSION_CAPACITY);
        assert_eq!(
            store.get(&key("sess-0", "anthropic-api", "opus")),
            None,
            "LRU entry must be evicted on overflow put",
        );
        assert!(
            store
                .get(&key("sess-overflow", "anthropic-api", "opus"))
                .is_some()
        );
    }

    #[test]
    fn k_session_store_export_returns_lru_order() {
        // Arrange: insert A, B, C in that order.
        let store = KSessionStore::new();
        let a = key("A", "anthropic-api", "opus");
        let b = key("B", "anthropic-api", "opus");
        let c = key("C", "anthropic-api", "opus");
        store.put(a.clone(), window_with(1));
        store.put(b.clone(), window_with(2));
        store.put(c.clone(), window_with(3));

        // Touch A so it becomes MRU; the new order is B (LRU), C, A (MRU).
        let _ = store.get(&a);

        // Act
        let entries = store.export_entries();

        // Assert: LRU first, MRU last.
        let keys: Vec<&KSessionKey> = entries.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![&b, &c, &a]);
    }

    #[test]
    fn k_session_store_roundtrip_preserves_order() {
        // Arrange: build a non-trivial recency ordering in the source.
        let source = KSessionStore::new();
        let a = key("A", "anthropic-api", "opus");
        let b = key("B", "anthropic-api", "opus");
        let c = key("C", "anthropic-api", "opus");
        source.put(a.clone(), window_with(1));
        source.put(b, window_with(2));
        source.put(c, window_with(3));
        let _ = source.get(&a); // promote A to MRU
        let exported = source.export_entries();

        // Act: import into a fresh store, then re-export.
        let dest = KSessionStore::new();
        dest.import_entries(exported.clone());
        let reexported = dest.export_entries();

        // Assert: the destination's LRU ordering matches the source's.
        assert_eq!(reexported, exported);
    }
}
