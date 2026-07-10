//! Startup warm of the K-estimator session store from the usage ledger.
//!
//! On a fresh process start the in-memory `KSessionStore` is empty, so
//! every estimate collapses to a cold default until live traffic re-seeds
//! it. This module bridges the leaf usage crate to the router-side
//! `LedgerReader` seam (which the router crate cannot satisfy itself, since
//! it does not depend on the usage crate) and runs a one-shot rebuild at
//! the serve bootstrap.
//!
//! The warm runs ONLY at the initial bootstrap, never on a hot-reload:
//! `Router::carry_over_k_store_from` already preserves the live in-memory
//! store across a reload, so re-running the rebuild there would clobber
//! fresher live samples with older ledger history.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use routectl_router::{KSessionStore, LedgerReader, LedgerSampleRow, rebuild_into};
use routectl_usage::{OpenError, open_readonly, read_reuse_samples_since};

/// How far back the startup rebuild reads the ledger. A conservative
/// bound: 8x the longest known cache prefix TTL (24h), so a session whose
/// prefix could still be warm is always covered. The per-estimate TTL-gap
/// split uses the actual per-request TTL, so this only bounds how much
/// history to load, not how samples are aged.
const REBUILD_WINDOW: Duration = Duration::from_hours(192);

/// Upper bound on the number of ledger rows the startup rebuild reads. A
/// plain compile-time cap (never derived from runtime input) on the boot
/// read; the per-session window cap bounds in-memory retention regardless
/// of how many rows are returned.
const REBUILD_ROW_LIMIT: usize = 5000;

/// Bridges the leaf usage ledger to the router-side `LedgerReader` seam.
///
/// Holds the resolved DB path and opens a fresh read-only connection per
/// `read_reuse_samples` call. The rebuild calls it exactly once at startup,
/// so a per-call open is fine and avoids holding a connection open for the
/// daemon's whole life.
struct UsageLedgerReader {
    db_path: PathBuf,
}

impl UsageLedgerReader {
    const fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }
}

impl LedgerReader for UsageLedgerReader {
    fn read_reuse_samples(&self, window_start: SystemTime, limit: usize) -> Vec<LedgerSampleRow> {
        let window_start_ms = window_start
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);

        let db = match open_readonly(&self.db_path) {
            Ok(db) => db,
            Err(_) => return Vec::new(),
        };

        match read_reuse_samples_since(db.conn(), window_start_ms, limit) {
            Ok(rows) => rows.into_iter().map(reuse_row_to_ledger_row).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Map one usage-local `ReuseSampleRow` into the router-side
/// `LedgerSampleRow`, clamping the two theoretically-negative columns to a
/// safe floor: a negative `cache_read` becomes 0 (no reuse), a negative
/// epoch-ms timestamp becomes the UNIX epoch.
fn reuse_row_to_ledger_row(row: routectl_usage::ReuseSampleRow) -> LedgerSampleRow {
    LedgerSampleRow::new(
        row.session_id,
        row.provider_kind,
        row.model,
        UNIX_EPOCH + Duration::from_millis(row.ts_start_ms.max(0) as u64),
        row.cache_read.max(0) as u64,
    )
}

/// One-shot warm of `store` from the usage ledger at serve bootstrap.
///
/// Best-effort: a missing / unreadable DB (`NoData`, open error) skips the
/// rebuild and leaves the store cold -- a cold start is the safe default
/// and must never fail bootstrap. On a successful open the rebuild reads
/// the `[now - REBUILD_WINDOW, now]` window, capped at `REBUILD_ROW_LIMIT`
/// rows.
pub(crate) fn warm_k_store_from_ledger(db_path: &Path, store: &KSessionStore) {
    match open_readonly(db_path) {
        Ok(_) => {}
        Err(OpenError::NoData { .. }) => {
            tracing::debug!(
                db_path = %db_path.display(),
                "no usage data yet; skipping K-estimator startup warm (cold start)"
            );
            return;
        }
        Err(e) => {
            tracing::debug!(
                db_path = %db_path.display(),
                error = %e,
                "usage db not readable; skipping K-estimator startup warm (cold start)"
            );
            return;
        }
    }

    let reader = UsageLedgerReader::new(db_path.to_path_buf());
    let window_start = SystemTime::now()
        .checked_sub(REBUILD_WINDOW)
        .unwrap_or(UNIX_EPOCH);
    rebuild_into(&reader, store, window_start, REBUILD_ROW_LIMIT);
    tracing::info!(
        tracked_sessions = store.len(),
        "warmed K-estimator session store from usage ledger"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_router::KSessionKey;
    use routectl_usage::open;
    use tempfile::TempDir;

    fn temp_db_path() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("usage.db");
        (dir, path)
    }

    /// Insert a reuse-bearing row directly so the reader's mapping +
    /// clamps can be exercised against a real read-only open.
    fn insert_reuse_row(
        db: &routectl_usage::UsageDb,
        request_id: &str,
        ts_start: i64,
        session_id: &str,
        provider_kind: &str,
        model: &str,
        cache_read: Option<i64>,
    ) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider_kind, session_id, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, cache_read) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, ?4, ?5, 1, 'ok', \
                 5, 0, 0, 1, 0, ?6)",
                rusqlite::params![
                    ts_start,
                    request_id,
                    model,
                    provider_kind,
                    session_id,
                    cache_read,
                ],
            )
            .expect("insert reuse row");
    }

    #[test]
    fn reader_maps_rows_and_derives_window_against_real_db() {
        // Arrange: seed two rows for one triple, then drop the writer so
        // the file is read through the read-only open path.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_reuse_row(&db, "r1", 100, "s1", "anthropic-api", "opus", Some(42));
        insert_reuse_row(&db, "r2", 110, "s1", "anthropic-api", "opus", None);
        drop(db);

        // Act
        let reader = UsageLedgerReader::new(path);
        let rows = reader.read_reuse_samples(UNIX_EPOCH, 100);

        // Assert: both rows map, NULL cache_read coalesced to 0, the
        // epoch-ms timestamps reconstruct to the right SystemTime.
        assert_eq!(rows.len(), 2);
        let first = &rows[0];
        assert_eq!(first.session_key, "s1");
        assert_eq!(first.provider_kind, "anthropic-api");
        assert_eq!(first.model, "opus");
        assert_eq!(first.cache_read, 42);
        assert_eq!(first.ts, UNIX_EPOCH + Duration::from_millis(100));
        let second = &rows[1];
        assert_eq!(second.cache_read, 0, "NULL cache_read coalesces to 0");
        assert_eq!(second.ts, UNIX_EPOCH + Duration::from_millis(110));
    }

    #[test]
    fn reader_returns_empty_for_absent_db() {
        // Arrange: a path with no DB file.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("absent.db");

        // Act + Assert: a missing DB yields no rows, never an error.
        let reader = UsageLedgerReader::new(path);
        assert!(reader.read_reuse_samples(UNIX_EPOCH, 100).is_empty());
    }

    #[test]
    fn warm_populates_store_from_seeded_ledger() {
        // Arrange: seed two triples, recent enough to fall inside the
        // rebuild window.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as i64;
        insert_reuse_row(
            &db,
            "a1",
            now_ms - 1000,
            "s1",
            "anthropic-api",
            "opus",
            Some(9),
        );
        insert_reuse_row(
            &db,
            "a2",
            now_ms - 900,
            "s1",
            "anthropic-api",
            "opus",
            Some(0),
        );
        insert_reuse_row(&db, "b1", now_ms - 800, "s1", "bedrock", "opus", Some(3));
        drop(db);

        // Act
        let store = KSessionStore::new();
        warm_k_store_from_ledger(&path, &store);

        // Assert: two distinct triples warmed; the primary carries both
        // samples with reuse derived from cache_read > 0.
        assert_eq!(store.len(), 2);
        let primary = store
            .get(&KSessionKey {
                session_key: "s1".into(),
                provider_kind: "anthropic-api".into(),
                model: "opus".into(),
            })
            .expect("primary triple warmed");
        let reuse: Vec<bool> = primary.iter().map(|s| s.observed_reuse).collect();
        assert_eq!(reuse, vec![true, false]);
        assert!(
            store
                .get(&KSessionKey {
                    session_key: "s1".into(),
                    provider_kind: "bedrock".into(),
                    model: "opus".into(),
                })
                .is_some()
        );
    }

    #[test]
    fn warm_skips_absent_db_without_panicking() {
        // Arrange: no DB file at the path.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("absent.db");

        // Act + Assert: warm is a no-op, the store stays cold.
        let store = KSessionStore::new();
        warm_k_store_from_ledger(&path, &store);
        assert!(store.is_empty());
    }

    /// A session identified ONLY via the body `metadata.session_id`
    /// fallback (no `x-claude-code-session-id` header) must still warm
    /// the K store on rebuild -- the restart-survival gap this task
    /// closes. `build_usage_draft` derives `session_id` from the SAME
    /// `inbound_session_key` the K-estimator keys on, so the ledger row
    /// this test writes through the REAL `UsageWriter` path carries a
    /// non-NULL `session_id` even though the header was never sent.
    #[test]
    fn warm_reidentifies_metadata_derived_session_after_restart() {
        // Arrange: parse a header-absent, metadata-only Anthropic request
        // through the real ingress so `inbound_session_key` is genuinely
        // metadata-derived, then seed a draft from it exactly as the
        // production boundary does.
        let body = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "metadata": {"session_id": "meta-sid"}
        });
        let req = {
            use crate::ingress::IngressAdapter;
            use crate::ingress::anthropic::AnthropicIngress;
            AnthropicIngress
                .parse_request(&axum::http::HeaderMap::new(), body)
                .expect("parse anthropic request")
        };
        let mut draft = crate::handlers::usage_capture::build_usage_draft(
            "anthropic",
            &req,
            "req-meta-rebuild".to_string(),
        );
        assert_eq!(
            draft.session_id.as_deref(),
            Some("meta-sid"),
            "draft session_id must come from the metadata-derived inbound_session_key"
        );
        // Stamp the dispatch-derived columns the real `observe_meta` /
        // `observe_response` calls would set, so the row clears the
        // reuse-sample query's NOT NULL filters.
        draft.provider_kind = Some("anthropic-api".to_string());
        draft.model = Some("opus".to_string());
        draft.cache_read = Some(42);
        draft.outcome = routectl_usage::Outcome::Ok;

        let (_dir, path) = temp_db_path();
        let (handle, writer) = routectl_usage::UsageWriter::start(
            path.clone(),
            routectl_usage::CHANNEL_CAPACITY,
            0,
            true,
        );
        handle.try_send(draft);
        // Drop the producer handle BEFORE shutdown: `UsageWriter::shutdown`
        // waits for the channel to CLOSE (all senders dropped) to detect
        // drain completion; a live handle clone would starve that signal
        // and shutdown would sit out the full drain deadline.
        drop(handle);
        writer.shutdown();

        // Act
        let store = KSessionStore::new();
        warm_k_store_from_ledger(&path, &store);

        // Assert: the metadata-derived session is warmed under its
        // resolved session key, not dropped as a NULL-session row.
        assert!(
            store
                .get(&KSessionKey {
                    session_key: "meta-sid".into(),
                    provider_kind: "anthropic-api".into(),
                    model: "opus".into(),
                })
                .is_some(),
            "a session identified only via metadata.session_id must survive rebuild"
        );
    }
}
