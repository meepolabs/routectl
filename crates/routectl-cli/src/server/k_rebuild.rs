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
use std::sync::atomic::{AtomicUsize, Ordering};
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
    loaded_rows: AtomicUsize,
}

impl UsageLedgerReader {
    const fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            loaded_rows: AtomicUsize::new(0),
        }
    }

    /// Warn that a best-effort ledger read failed AFTER the bootstrap
    /// probe had already opened the DB cleanly. `failure_class` separates
    /// an open failure (`"open"`) from a query failure (`"query"`) so a
    /// broken ledger is never mistaken downstream for a legitimately empty
    /// one (both otherwise yield zero rows). Samples still return empty:
    /// the warm stays best-effort and a read failure never fails bootstrap.
    fn warn_read_failure(&self, failure_class: &str, error: &dyn std::fmt::Display) {
        tracing::warn!(
            db_path = %self.db_path.display(),
            failure_class,
            error = %error,
            "usage ledger read failed during K-estimator startup warm; leaving store cold"
        );
    }
}

impl LedgerReader for UsageLedgerReader {
    fn read_reuse_samples(&self, window_start: SystemTime, limit: usize) -> Vec<LedgerSampleRow> {
        let window_start_ms = window_start
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);

        let db = match open_readonly(&self.db_path) {
            Ok(db) => db,
            // An absent file or a table-less DB is the legitimately-empty
            // ledger, not a failure: return no samples silently, matching
            // the cold-start path the bootstrap probe already expects.
            Err(OpenError::NoData { .. }) => return Vec::new(),
            Err(e) => {
                self.warn_read_failure("open", &e);
                return Vec::new();
            }
        };

        match read_reuse_samples_since(db.conn(), window_start_ms, limit) {
            Ok(rows) => {
                self.loaded_rows.store(rows.len(), Ordering::Relaxed);
                rows.into_iter().map(reuse_row_to_ledger_row).collect()
            }
            Err(e) => {
                self.warn_read_failure("query", &e);
                Vec::new()
            }
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
    emit_rebuild_log(store.len(), reader.loaded_rows.load(Ordering::Relaxed));
}

/// Report the rebuild outcome: an `info` with the loaded-row count, the row
/// cap, and the window size, plus a one-shot `warn` when the load hit the cap
/// (warm state may then be truncated to the newest `REBUILD_ROW_LIMIT` rows).
fn emit_rebuild_log(tracked_sessions: usize, loaded_rows: usize) {
    let window_hours = REBUILD_WINDOW.as_secs() / 3600;
    if loaded_rows == REBUILD_ROW_LIMIT {
        tracing::warn!(
            loaded_rows,
            row_cap = REBUILD_ROW_LIMIT,
            window_hours,
            "K-estimator warm rebuild hit the row cap; warm state may be truncated"
        );
    }
    tracing::info!(
        tracked_sessions,
        loaded_rows,
        row_cap = REBUILD_ROW_LIMIT,
        window_hours,
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
    /// clamps can be exercised against a real read-only open. Always
    /// `outcome = 'ok'` -- see `insert_reuse_row_with_outcome` for the
    /// admission-contract test.
    fn insert_reuse_row(
        db: &routectl_usage::UsageDb,
        request_id: &str,
        ts_start: i64,
        session_id: &str,
        provider_kind: &str,
        model: &str,
        cache_read: Option<i64>,
    ) {
        insert_reuse_row_with_outcome(
            db,
            request_id,
            ts_start,
            session_id,
            provider_kind,
            model,
            cache_read,
            "ok",
        );
    }

    /// Same as `insert_reuse_row`, with an explicit `outcome` so the
    /// admission-contract filter (`outcome = 'ok'` only) is exercisable.
    #[allow(clippy::too_many_arguments)]
    fn insert_reuse_row_with_outcome(
        db: &routectl_usage::UsageDb,
        request_id: &str,
        ts_start: i64,
        session_id: &str,
        provider_kind: &str,
        model: &str,
        cache_read: Option<i64>,
        outcome: &str,
    ) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider_kind, session_id, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, cache_read) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, ?4, ?5, 1, ?6, \
                 5, 0, 0, 1, 0, ?7)",
                rusqlite::params![
                    ts_start,
                    request_id,
                    model,
                    provider_kind,
                    session_id,
                    outcome,
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

        // Act + Assert: a missing DB is the legitimately-empty ledger --
        // it yields no rows, never an error, and must NOT warn.
        let reader = UsageLedgerReader::new(path);
        let events = routectl_testkit::capture_events(|| {
            assert!(reader.read_reuse_samples(UNIX_EPOCH, 100).is_empty());
        });
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "an empty ledger must not emit a read-failure warning"
        );
    }

    #[test]
    fn reader_warns_and_returns_empty_on_unreadable_db() {
        // Arrange: a non-DB file at the path. It exists, so the read-only
        // open clears the existence probe and fails on the first PRAGMA --
        // a genuine read failure, not an empty ledger.
        let (_dir, path) = temp_db_path();
        std::fs::write(&path, b"this is not a sqlite database").expect("write junk");

        // Act
        let reader = UsageLedgerReader::new(path);
        let events = routectl_testkit::capture_events(|| {
            assert!(
                reader.read_reuse_samples(UNIX_EPOCH, 100).is_empty(),
                "a read failure yields no samples, never a panic"
            );
        });

        // Assert: a WARN fired naming the open failure class, distinct
        // from the silent empty-ledger path.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .expect("a read failure must emit a WARN");
        assert_eq!(warn.field("failure_class"), Some("open"));
        assert!(
            warn.field("error").is_some(),
            "the WARN must carry the error Display"
        );
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
    fn warm_excludes_mid_stream_failed_row_matching_live_admission() {
        // Arrange: an ok row and a mid-stream-failed row (upstream_error)
        // sharing a triple -- the failed row still carries a full triple and
        // a non-null cache_read (it observed partial usage before failing),
        // which is exactly the divergence case: the live path never records
        // it (record_k_sample only fires on the success finalize / natural
        // stream EOS) but a filter-less rebuild would replay it after a
        // restart. Plus a second triple that is ENTIRELY a failed row, which
        // must warm nothing.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as i64;
        insert_reuse_row(
            &db,
            "ok1",
            now_ms - 1000,
            "s1",
            "anthropic-api",
            "opus",
            Some(9),
        );
        insert_reuse_row_with_outcome(
            &db,
            "failed1",
            now_ms - 900,
            "s1",
            "anthropic-api",
            "opus",
            Some(5),
            "upstream_error",
        );
        insert_reuse_row_with_outcome(
            &db,
            "failed-only",
            now_ms - 800,
            "s2",
            "anthropic-api",
            "opus",
            Some(3),
            "upstream_error",
        );
        drop(db);

        // Act
        let store = KSessionStore::new();
        warm_k_store_from_ledger(&path, &store);

        // Assert: only the ok-row triple warms, with exactly its one sample.
        assert_eq!(store.len(), 1);
        let primary = store
            .get(&KSessionKey {
                session_key: "s1".into(),
                provider_kind: "anthropic-api".into(),
                model: "opus".into(),
            })
            .expect("ok-row triple warmed");
        assert_eq!(
            primary.len(),
            1,
            "the failed row must not contribute a sample"
        );
        assert!(
            store
                .get(&KSessionKey {
                    session_key: "s2".into(),
                    provider_kind: "anthropic-api".into(),
                    model: "opus".into(),
                })
                .is_none(),
            "a triple with only a failed row must not warm at all"
        );
    }

    #[test]
    fn rebuild_log_carries_loaded_rows_and_no_warn_under_cap() {
        // Arrange + Act: a normal rebuild outcome, loaded_rows below the cap.
        let events = routectl_testkit::capture_events(|| {
            emit_rebuild_log(4, REBUILD_ROW_LIMIT - 1);
        });

        // Assert: the info line carries loaded_rows, row_cap, and window_hours,
        // and no cap-hit warning fires.
        let info = events
            .iter()
            .find(|e| e.level == tracing::Level::INFO)
            .expect("info rebuild log emitted");
        assert_eq!(info.field("loaded_rows"), Some("4999"));
        assert_eq!(info.field("row_cap"), Some("5000"));
        assert_eq!(info.field("window_hours"), Some("192"));
        assert_eq!(info.field("tracked_sessions"), Some("4"));
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "no cap-hit warning under the row cap"
        );
    }

    #[test]
    fn rebuild_log_warns_when_row_cap_hit() {
        // Arrange + Act: loaded_rows exactly at the cap -- the truncation risk.
        let events = routectl_testkit::capture_events(|| {
            emit_rebuild_log(7, REBUILD_ROW_LIMIT);
        });

        // Assert: a WARN fires carrying the loaded-row count and cap, and the
        // info line still reports the loaded rows.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .expect("cap-hit warning emitted");
        assert!(warn.message.contains("hit the row cap"));
        assert_eq!(warn.field("loaded_rows"), Some("5000"));
        assert_eq!(warn.field("row_cap"), Some("5000"));
        let info = events
            .iter()
            .find(|e| e.level == tracing::Level::INFO)
            .expect("info rebuild log emitted");
        assert_eq!(info.field("loaded_rows"), Some("5000"));
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
                .parse_request_value(&axum::http::HeaderMap::new(), body)
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
