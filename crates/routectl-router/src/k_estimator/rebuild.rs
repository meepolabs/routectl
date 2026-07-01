//! Ledger-backed rebuild of the K estimator's session windows.
//!
//! On a fresh process start the in-memory [`KSessionStore`] is empty, so
//! every estimate would collapse to `Cold` until live traffic re-seeds it.
//! This module repopulates the store from a recent slice of the usage
//! ledger so the estimator answers from history immediately.
//!
//! The ledger lives in a leaf crate that this crate does not depend on, so
//! the read is expressed through the [`LedgerReader`] dependency-inversion
//! seam: a concrete reader bridging the usage crate to the router-side row
//! type is injected from the binary that owns both. The router OWNS the
//! reuse definition (`cache_read > 0`); the reader hands back raw columns.

use std::collections::HashMap;
use std::time::SystemTime;

use super::store::{KSessionKey, KSessionStore, KSessionWindow, Sample};

/// One ledger row the rebuild consumes, in router-side terms.
///
/// `#[non_exhaustive]` so a later increment can carry an additional column
/// (e.g. the served cache TTL) without a breaking change. The reader maps
/// usage-local column types into these fields; the rebuild derives the
/// boolean reuse observation from `cache_read` here, not at the reader.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSampleRow {
    /// Inbound session identifier recorded on the request.
    pub session_key: String,
    /// Stable provider-kind token of the served target.
    pub provider_kind: String,
    /// Served model nickname.
    pub model: String,
    /// Wall-clock start time of the request.
    pub ts: SystemTime,
    /// Cached prefix tokens re-read on the upstream response. `> 0` means a
    /// cache hit was observed on this turn.
    pub cache_read: u64,
}

impl LedgerSampleRow {
    /// Construct a row from the mapped ledger columns. Provided because the
    /// type is `#[non_exhaustive]`: a concrete `LedgerReader` lives in a
    /// different crate and cannot use a struct literal, so it builds rows
    /// through here. A later additive column gets a defaulted parameter or a
    /// dedicated setter without breaking this signature's callers.
    pub const fn new(
        session_key: String,
        provider_kind: String,
        model: String,
        ts: SystemTime,
        cache_read: u64,
    ) -> Self {
        Self {
            session_key,
            provider_kind,
            model,
            ts,
            cache_read,
        }
    }
}

/// Dependency-inversion seam between the usage ledger and the K estimator.
///
/// The concrete implementation lives in the binary that depends on both the
/// usage crate and the router crate; this crate (and its tests) only ever
/// sees the trait. `Send + Sync` so a caller may hold one behind an `Arc`.
pub trait LedgerReader: Send + Sync {
    /// Return up to `limit` reuse samples whose request start time is at or
    /// after `window_start`, ordered oldest-first. Implementations perform
    /// the ledger IO; they never derive the reuse boolean (that is the
    /// router's definition, applied in [`rebuild_into`]).
    fn read_reuse_samples(&self, window_start: SystemTime, limit: usize) -> Vec<LedgerSampleRow>;
}

/// Repopulate `store` from the ledger window `[window_start, now]`.
///
/// Reads up to `limit` rows via `reader`, groups them by their
/// (session, provider_kind, model) triple, sorts each group ascending by
/// timestamp, and pushes one [`Sample`] per row into that triple's window.
/// The window's own `push` caps retention at its bound, keeping the most
/// recent samples. The reuse observation is derived here as
/// `cache_read > 0` so the router owns the reuse definition.
pub fn rebuild_into(
    reader: &dyn LedgerReader,
    store: &KSessionStore,
    window_start: SystemTime,
    limit: usize,
) {
    let rows = reader.read_reuse_samples(window_start, limit);

    let mut grouped: HashMap<KSessionKey, Vec<LedgerSampleRow>> = HashMap::new();
    for row in rows {
        let key = KSessionKey {
            session_key: row.session_key.clone(),
            provider_kind: row.provider_kind.clone(),
            model: row.model.clone(),
        };
        grouped.entry(key).or_default().push(row);
    }

    for (key, mut group) in grouped {
        group.sort_by_key(|row| row.ts);
        let mut window = KSessionWindow::new();
        for row in group {
            window.push(Sample {
                ts: row.ts,
                observed_reuse: row.cache_read > 0,
            });
        }
        store.put(key, window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    /// A reader that returns a fixed row set, ignoring its arguments. The
    /// rebuild's windowing/limit contract belongs to the reader; here we
    /// pin the router-side grouping and reuse-derivation behavior.
    struct FakeReader {
        rows: Vec<LedgerSampleRow>,
    }

    impl LedgerReader for FakeReader {
        fn read_reuse_samples(
            &self,
            _window_start: SystemTime,
            _limit: usize,
        ) -> Vec<LedgerSampleRow> {
            self.rows.clone()
        }
    }

    fn row(session: &str, kind: &str, model: &str, secs: u64, cache_read: u64) -> LedgerSampleRow {
        LedgerSampleRow {
            session_key: session.into(),
            provider_kind: kind.into(),
            model: model.into(),
            ts: UNIX_EPOCH + Duration::from_secs(secs),
            cache_read,
        }
    }

    fn key(session: &str, kind: &str, model: &str) -> KSessionKey {
        KSessionKey {
            session_key: session.into(),
            provider_kind: kind.into(),
            model: model.into(),
        }
    }

    #[test]
    fn rebuild_groups_distinct_triples_without_merging() {
        // Arrange: two triples that differ only by provider_kind and by
        // model must land in separate windows -- the triple is load-bearing.
        let reader = FakeReader {
            rows: vec![
                row("s1", "anthropic-api", "opus", 100, 10),
                row("s1", "anthropic-api", "opus", 110, 0),
                row("s1", "bedrock", "opus", 120, 5),
                row("s1", "anthropic-api", "sonnet", 130, 7),
            ],
        };
        let store = KSessionStore::new();

        // Act
        rebuild_into(&reader, &store, UNIX_EPOCH, 1000);

        // Assert: three distinct triples, the first carrying two samples.
        assert_eq!(store.len(), 3);
        let primary = store
            .get(&key("s1", "anthropic-api", "opus"))
            .expect("primary triple present");
        assert_eq!(primary.len(), 2);
        assert!(store.get(&key("s1", "bedrock", "opus")).is_some());
        assert!(store.get(&key("s1", "anthropic-api", "sonnet")).is_some());
    }

    #[test]
    fn rebuild_derives_observed_reuse_from_cache_read() {
        // Arrange: a hit (cache_read > 0) and a miss (cache_read == 0).
        let reader = FakeReader {
            rows: vec![
                row("s1", "anthropic-api", "opus", 100, 0),
                row("s1", "anthropic-api", "opus", 110, 42),
            ],
        };
        let store = KSessionStore::new();

        // Act
        rebuild_into(&reader, &store, UNIX_EPOCH, 1000);

        // Assert: reuse boolean tracks cache_read > 0, in ascending ts order.
        let window = store
            .get(&key("s1", "anthropic-api", "opus"))
            .expect("triple present");
        let reuse: Vec<bool> = window.iter().map(|s| s.observed_reuse).collect();
        assert_eq!(reuse, vec![false, true]);
    }

    #[test]
    fn rebuild_sorts_each_window_ascending_by_ts() {
        // Arrange: rows delivered out of order must end up oldest-first.
        let reader = FakeReader {
            rows: vec![
                row("s1", "anthropic-api", "opus", 300, 1),
                row("s1", "anthropic-api", "opus", 100, 1),
                row("s1", "anthropic-api", "opus", 200, 1),
            ],
        };
        let store = KSessionStore::new();

        // Act
        rebuild_into(&reader, &store, UNIX_EPOCH, 1000);

        // Assert
        let window = store
            .get(&key("s1", "anthropic-api", "opus"))
            .expect("triple present");
        let tss: Vec<SystemTime> = window.iter().map(|s| s.ts).collect();
        assert_eq!(
            tss,
            vec![
                UNIX_EPOCH + Duration::from_secs(100),
                UNIX_EPOCH + Duration::from_secs(200),
                UNIX_EPOCH + Duration::from_secs(300),
            ]
        );
    }

    #[test]
    fn rebuild_caps_window_keeping_most_recent() {
        // Arrange: more rows than the window cap. The window must retain the
        // most recent samples (the oldest are dropped on push).
        let mut rows = Vec::new();
        for i in 0..40u64 {
            rows.push(row("s1", "anthropic-api", "opus", i, 1));
        }
        let reader = FakeReader { rows };
        let store = KSessionStore::new();

        // Act
        rebuild_into(&reader, &store, UNIX_EPOCH, 1000);

        // Assert: capped, and the oldest retained sample is NOT ts 0 -- the
        // early rows were evicted in favor of the most recent.
        let window = store
            .get(&key("s1", "anthropic-api", "opus"))
            .expect("triple present");
        assert!(window.len() <= 40);
        let oldest = window.iter().next().expect("non-empty window");
        assert_ne!(oldest.ts, UNIX_EPOCH, "oldest rows must be evicted");
        let newest = window.iter().last().expect("non-empty window");
        assert_eq!(newest.ts, UNIX_EPOCH + Duration::from_secs(39));
    }
}
