//! The off-hot-path bounded writer task.
//!
//! A dedicated OS thread owns the (blocking) `rusqlite` connection and is
//! the single writer to the usage DB. It receives `UsageRecord`s over a
//! bounded `tokio::sync::mpsc` channel via `blocking_recv` -- so the
//! blocking SQLite writes never run on a tokio runtime worker, and the
//! async producer side never blocks. A DB failure degrades the subsystem
//! (log + count + keep draining) rather than crashing the proxy. On
//! shutdown the sender is dropped, the thread drains the already-queued
//! rows under a bounded deadline, and the thread is joined.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use tokio::sync::mpsc;

use crate::db::{self, UsageDb};
use crate::handle::{UsageCounters, UsageHandle};
use crate::learn_event::{CapabilityLearnEvent, insert_learn_event};
use crate::record::UsageRecord;
use crate::retention::{self, PruneOutcome};

/// A message on the producer -> writer channel. One channel, one actor, one
/// SQLite connection serves both usage-request rows and capability
/// learn-event rows -- the message kind selects the destination table. The
/// `UsageRecord` is boxed so the two variants stay close in size (the record
/// dwarfs a learn event), keeping the channel's per-slot footprint small.
pub enum WriterMessage {
    /// A usage-accounting row bound for the `requests` table.
    Request(Box<UsageRecord>),
    /// A capability learn event bound for the `capability_learn_events` table.
    LearnEvent(CapabilityLearnEvent),
}

/// Bounded capacity of the producer -> writer channel. Sized to absorb a
/// short burst without back-pressuring callers; overflow drops a row
/// rather than blocking the hot path.
pub const CHANNEL_CAPACITY: usize = 2048;

/// Upper bound on how long shutdown waits for the consumer thread to
/// drain queued rows before abandoning them. Keeps a wedged DB from
/// hanging daemon shutdown.
const SHUTDOWN_DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// Log an ERROR about write failures at most this often (every Nth
/// error), in addition to the always-logged degraded-state transition.
const WRITE_ERROR_LOG_INTERVAL: u64 = 1024;

/// Shutdown handle for the writer subsystem. Not `Clone` -- the daemon
/// owns exactly one and calls [`UsageWriter::shutdown`] once on teardown.
///
/// When the consumer thread could not be spawned the writer is
/// constructed in a degraded form (`done`/`join` are `None`); both
/// `shutdown` and `Drop` then no-op.
pub struct UsageWriter {
    sender: Option<mpsc::Sender<WriterMessage>>,
    done: Option<std::sync::mpsc::Receiver<()>>,
    join: Option<std::thread::JoinHandle<()>>,
    counters: Arc<UsageCounters>,
}

impl UsageWriter {
    /// Start the writer subsystem.
    ///
    /// Opens the DB at `db_path` (degrading to a no-DB drain loop if the
    /// open fails -- construction never hard-fails), runs the one-shot
    /// startup retention prune, spawns the dedicated consumer thread, and
    /// returns the `Clone` [`UsageHandle`] for callers plus this shutdown
    /// handle. The writer thread is always spawned, even when
    /// `initial_enabled` is false -- the gate is checked per-record on the
    /// producer side so it can be flipped at runtime.
    pub fn start(
        db_path: PathBuf,
        capacity: usize,
        retention_days: u32,
        initial_enabled: bool,
    ) -> (UsageHandle, Self) {
        let counters = Arc::new(UsageCounters::default());
        let enabled = Arc::new(AtomicBool::new(initial_enabled));
        let (tx, rx) = mpsc::channel::<WriterMessage>(capacity.max(1));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        let thread_counters = Arc::clone(&counters);
        let spawn_result = std::thread::Builder::new()
            .name("routectl-usage-writer".to_string())
            .spawn(move || {
                run_writer(db_path, retention_days, rx, thread_counters);
                let _ = done_tx.send(());
            });

        let join = match spawn_result {
            Ok(join) => join,
            Err(err) => {
                counters.incr_write_errors();
                tracing::error!(
                    target: "routectl_usage::writer",
                    error = %err,
                    "usage writer thread spawn failed -- running degraded (records will be dropped)"
                );
                return Self::degraded(tx, enabled, counters);
            }
        };

        let handle = UsageHandle::new(tx.clone(), enabled, Arc::clone(&counters));
        let writer = Self {
            sender: Some(tx),
            done: Some(done_rx),
            join: Some(join),
            counters,
        };
        (handle, writer)
    }

    /// Build a degraded handle/writer pair with no consumer thread.
    ///
    /// Used when the OS refuses to spawn the writer thread. The handle's
    /// `try_send` accepts-and-drops (the receiver `rx` was already moved
    /// into the failed spawn closure and dropped, so the channel is
    /// closed); the writer's `shutdown`/`Drop` no-op because there is no
    /// thread to drain or join.
    fn degraded(
        sender: mpsc::Sender<WriterMessage>,
        enabled: Arc<AtomicBool>,
        counters: Arc<UsageCounters>,
    ) -> (UsageHandle, Self) {
        let handle = UsageHandle::new(sender, enabled, Arc::clone(&counters));
        let writer = Self {
            sender: None,
            done: None,
            join: None,
            counters,
        };
        (handle, writer)
    }

    /// Read-only view of the shared health counters.
    pub const fn counters(&self) -> &Arc<UsageCounters> {
        &self.counters
    }

    /// Close the channel, drain queued rows under a bounded deadline, and
    /// join the consumer thread.
    ///
    /// Dropping the sender lets the consumer's `blocking_recv` return
    /// `None` once the queue empties, so a healthy DB drains fully. If the
    /// DB is wedged the drain is abandoned after [`SHUTDOWN_DRAIN_DEADLINE`]
    /// and the thread is left detached so shutdown never hangs.
    ///
    /// # Blocking
    ///
    /// This performs a blocking drain of up to [`SHUTDOWN_DRAIN_DEADLINE`]
    /// and MUST be called from a blocking context. The daemon dispatches it
    /// via `tokio::task::spawn_blocking`; it must NEVER be called directly
    /// on an async runtime worker, or it can stall that worker for the full
    /// deadline. (`Drop` is non-blocking and is the safe fallback if an
    /// explicit shutdown was missed.)
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        // Drop the sender so the consumer's recv loop can terminate.
        self.sender.take();
        // A degraded writer (no thread) has nothing to drain or join.
        let Some(done) = self.done.take() else {
            return;
        };
        let persisted_before = self.counters.persisted();

        match done.recv_timeout(SHUTDOWN_DRAIN_DEADLINE) {
            Ok(()) => {
                self.join_once();
                tracing::info!(
                    target: "routectl_usage::writer",
                    flushed = self.counters.persisted() - persisted_before,
                    "usage writer drained and stopped"
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The thread ended without signalling done -- it panicked.
                // Join now (instant: it is already gone) to surface the
                // panic payload instead of silently abandoning it.
                if let Some(Err(_)) = self.join.take().map(std::thread::JoinHandle::join) {
                    tracing::error!(
                        target: "routectl_usage::writer",
                        "usage writer thread panicked during drain"
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    target: "routectl_usage::writer",
                    deadline_secs = SHUTDOWN_DRAIN_DEADLINE.as_secs(),
                    flushed = self.counters.persisted() - persisted_before,
                    "usage writer drain deadline exceeded -- abandoning queued rows"
                );
            }
        }
    }

    /// Join the consumer thread at most once. Safe to call after a prior
    /// shutdown took the handle (double-shutdown / shutdown-then-Drop).
    fn join_once(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for UsageWriter {
    /// Non-blocking teardown: signal the consumer to stop by dropping the
    /// sender, then return immediately. Rows still queued at drop may be
    /// lost -- at-most-once durability is the accepted contract. The 5s
    /// drain + join lives only in the explicit [`UsageWriter::shutdown`],
    /// so dropping a writer on a runtime worker never stalls it.
    fn drop(&mut self) {
        self.sender.take();
    }
}

/// Current wall-clock time as epoch milliseconds (saturating).
fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Consumer-thread entry point: open the DB (or degrade), prune once,
/// then drain the channel until all senders are gone.
fn run_writer(
    db_path: PathBuf,
    retention_days: u32,
    mut rx: mpsc::Receiver<WriterMessage>,
    counters: Arc<UsageCounters>,
) {
    let mut state = WriterState::open(db_path, &counters);
    state.prune_once(retention_days, &counters);

    while let Some(msg) = rx.blocking_recv() {
        match msg {
            WriterMessage::Request(record) => state.persist(&record, &counters),
            WriterMessage::LearnEvent(event) => state.persist_learn_event(&event, &counters),
        }
    }
}

/// Per-thread mutable state: the (optional) connection plus the
/// healthy/degraded flag for transition logging.
struct WriterState {
    conn: Option<Connection>,
    degraded: bool,
}

impl WriterState {
    /// Open the DB, degrading to a no-connection drain loop on failure.
    /// A failed open is logged once and counted; the thread keeps running
    /// so callers are never affected.
    fn open(db_path: PathBuf, counters: &Arc<UsageCounters>) -> Self {
        match db::open(&db_path) {
            Ok(db) => Self {
                conn: Some(UsageDb::into_conn(db)),
                degraded: false,
            },
            Err(err) => {
                counters.incr_write_errors();
                tracing::error!(
                    target: "routectl_usage::writer",
                    error = %err,
                    "usage db open failed -- running degraded (records will be dropped)"
                );
                Self {
                    conn: None,
                    degraded: true,
                }
            }
        }
    }

    /// Run the one-shot startup retention prune. Best-effort: failures log
    /// a WARN and bump a counter but never abort the writer.
    fn prune_once(&self, retention_days: u32, counters: &Arc<UsageCounters>) {
        let Some(conn) = self.conn.as_ref() else {
            return;
        };
        match retention::prune(conn, retention_days, now_epoch_ms()) {
            Ok(PruneOutcome::Pruned { deleted }) => tracing::info!(
                target: "routectl_usage::writer",
                deleted,
                retention_days,
                "usage retention prune complete"
            ),
            Ok(PruneOutcome::Skipped) => {}
            Err(err) => {
                counters.incr_prune_errors();
                tracing::warn!(
                    target: "routectl_usage::writer",
                    error = %err,
                    "usage retention prune failed -- continuing"
                );
            }
        }
    }

    /// Persist one record, tracking degraded-state transitions. A real
    /// insert (1 row) bumps `persisted`; an `INSERT OR IGNORE` no-op (0
    /// rows -- duplicate request_id) is healthy but not counted. A write
    /// error (or a missing connection) drops the row, counts the error,
    /// and -- on the healthy->degraded edge -- logs once at ERROR.
    fn persist(&mut self, record: &UsageRecord, counters: &Arc<UsageCounters>) {
        let Some(conn) = self.conn.as_ref() else {
            self.record_failure(None, counters);
            return;
        };
        match insert_record(conn, record) {
            Ok(1) => {
                counters.incr_persisted();
                self.mark_healthy();
            }
            Ok(0) => {
                // Duplicate request_id collapsed by INSERT OR IGNORE: the
                // write succeeded (DB is healthy) but no new row landed, so
                // it must not inflate the persisted counter.
                self.mark_healthy();
                tracing::debug!(
                    target: "routectl_usage::writer",
                    "usage writer ignored duplicate request_id"
                );
            }
            Ok(n) => {
                // A single-row INSERT can only affect 0 or 1 rows; anything
                // else is impossible. Treat defensively in production.
                debug_assert!(false, "insert_record affected {n} rows");
                self.mark_healthy();
                tracing::error!(
                    target: "routectl_usage::writer",
                    rows = n,
                    "usage writer insert affected unexpected row count"
                );
            }
            Err(err) => self.record_failure(Some(err), counters),
        }
    }

    /// Persist one capability learn event to `capability_learn_events`.
    /// Append-only (no duplicate collapsing), so any success bumps the
    /// learn-event persisted counter. A missing connection or an insert
    /// error drops the event and routes through the shared DB-health
    /// failure path (write-error counter + degraded-transition log).
    fn persist_learn_event(&mut self, event: &CapabilityLearnEvent, counters: &Arc<UsageCounters>) {
        let Some(conn) = self.conn.as_ref() else {
            self.record_failure(None, counters);
            return;
        };
        match insert_learn_event(conn, event) {
            Ok(_) => {
                counters.incr_learn_events_persisted();
                self.mark_healthy();
            }
            Err(err) => self.record_failure(Some(err), counters),
        }
    }

    /// Count a write failure, emit a rate-limited ERROR, and log the
    /// healthy->degraded transition exactly once on its leading edge.
    fn record_failure(&mut self, err: Option<rusqlite::Error>, counters: &Arc<UsageCounters>) {
        let prior = counters.incr_write_errors();
        if !self.degraded {
            self.degraded = true;
            tracing::error!(
                target: "routectl_usage::writer",
                error = err.as_ref().map(std::string::ToString::to_string).unwrap_or_default(),
                "usage writer degraded -- dropping rows it cannot persist"
            );
        } else if (prior + 1).is_multiple_of(WRITE_ERROR_LOG_INTERVAL) {
            tracing::error!(
                target: "routectl_usage::writer",
                write_errors = prior + 1,
                "usage writer still degraded"
            );
        }
    }

    /// Log the degraded->healthy recovery edge exactly once.
    fn mark_healthy(&mut self) {
        if self.degraded {
            self.degraded = false;
            tracing::info!(
                target: "routectl_usage::writer",
                "usage writer recovered -- persisting rows again"
            );
        }
    }
}

/// Serialize an optional JSON value column to owned TEXT, or `None` for a
/// SQL NULL. A serialization failure degrades to NULL rather than failing
/// the whole row.
fn json_text(value: &Option<serde_json::Value>) -> Option<String> {
    value.as_ref().and_then(|v| serde_json::to_string(v).ok())
}

/// Per-row byte cap on the `would_trim_raw_marks` blob (D8 bounded-capture
/// house style -- see `routectl_providers::anthropic_api::sse_opaque` --
/// degrade gracefully on overflow rather than mid-value truncate into
/// invalid JSON). 64 KB is generous for any reasonable per-request mark
/// count while keeping the column bounded against an adversarial input.
const MAX_RAW_MARKS_BYTES: usize = 64 * 1024;

/// Serialize the `would_trim_raw_marks` JSON value to owned TEXT, bounded to
/// `MAX_RAW_MARKS_BYTES`. If the full serialization fits, it is stored
/// verbatim. If `value` is a JSON array and the full form overflows,
/// trailing elements are dropped (in order) until the remainder fits --
/// this always produces valid, parseable JSON, unlike a byte-offset
/// truncation of the serialized string. A non-array value that overflows
/// has no element boundary to trim, so it degrades to `None` rather than
/// storing invalid JSON.
fn capped_raw_marks_text(value: &Option<serde_json::Value>) -> Option<String> {
    let value = value.as_ref()?;
    let full = serde_json::to_string(value).ok()?;
    if full.len() <= MAX_RAW_MARKS_BYTES {
        return Some(full);
    }
    let array = value.as_array()?;
    let mut kept = Vec::new();
    let mut budget = 2; // the enclosing "[" + "]"
    for item in array {
        let item_text = serde_json::to_string(item).ok()?;
        let separator_len = usize::from(!kept.is_empty());
        let added = item_text.len() + separator_len;
        if budget + added > MAX_RAW_MARKS_BYTES {
            break;
        }
        budget += added;
        kept.push(item_text);
    }
    Some(format!("[{}]", kept.join(",")))
}

/// `INSERT OR IGNORE` one record, binding every column from
/// `UsageRecord` in schema order. Duplicate `request_id`s are silently
/// ignored (the idempotency contract). All values are bound parameters.
/// Returns the number of rows actually inserted (1 for a new row, 0 for
/// an ignored duplicate).
fn insert_record(conn: &Connection, r: &UsageRecord) -> Result<usize, rusqlite::Error> {
    let server_tool_use = json_text(&r.server_tool_use);
    let quota_extras = json_text(&r.quota_extras);
    let extra = json_text(&r.extra);
    let would_trim_raw_marks = capped_raw_marks_text(&r.would_trim_raw_marks);
    conn.execute(
        INSERT_SQL,
        rusqlite::params![
            r.ts_start,
            r.ts_end,
            r.request_id,
            r.ingress_dialect,
            r.requested_model,
            r.alias,
            r.model,
            r.upstream,
            r.provider,
            r.provider_kind,
            r.seat,
            r.session_id,
            r.stream as i64,
            r.max_tokens_req,
            r.tool_count,
            r.thinking_req,
            r.thinking_req_kind,
            r.msg_count,
            r.service_tier,
            r.outcome.as_str(),
            r.http_status,
            r.error_class,
            r.finish_reason,
            r.attempt_count,
            r.fallback_count,
            r.latency_ms,
            r.ttfb_ms,
            r.input_tokens,
            r.output_tokens,
            r.reasoning_tokens,
            r.cache_read,
            r.cache_write_5m,
            r.cache_write_1h,
            server_tool_use,
            r.quota_claim,
            r.quota_status,
            r.quota_overage_status,
            r.quota_utilization,
            r.quota_overage_utilization,
            r.quota_reset,
            quota_extras,
            extra,
            r.strategy,
            r.reduction_strategy,
            r.selection_decision,
            r.would_trim_tokens,
            r.would_trim_break_even_k,
            r.would_trim_k_floor,
            r.would_trim_shadow_misfire,
            r.would_trim_dedup_tokens,
            r.would_trim_supersession_tokens,
            r.would_trim_path_units,
            r.would_trim_path_extractable,
            r.would_trim_recorder_version,
            would_trim_raw_marks,
            r.would_trim_context_fraction,
        ],
    )
}

/// The bound `INSERT OR IGNORE`. Column order mirrors `record.rs` /
/// `schema.rs` exactly; `?1..?56` positions match the params list above.
const INSERT_SQL: &str = "\
INSERT OR IGNORE INTO requests (
    ts_start, ts_end, request_id, ingress_dialect, requested_model, alias,
    model, upstream, provider, provider_kind, seat, session_id,
    stream, max_tokens_req, tool_count, thinking_req, thinking_req_kind,
    msg_count, service_tier,
    outcome, http_status, error_class, finish_reason, attempt_count, fallback_count,
    latency_ms, ttfb_ms,
    input_tokens, output_tokens, reasoning_tokens, cache_read, cache_write_5m,
    cache_write_1h, server_tool_use,
    quota_claim, quota_status, quota_overage_status, quota_utilization,
    quota_overage_utilization, quota_reset, quota_extras,
    extra,
    strategy,
    reduction_strategy,
    selection_decision,
    would_trim_tokens,
    would_trim_break_even_k,
    would_trim_k_floor,
    would_trim_shadow_misfire,
    would_trim_dedup_tokens,
    would_trim_supersession_tokens,
    would_trim_path_units,
    would_trim_path_extractable,
    would_trim_recorder_version,
    would_trim_raw_marks,
    would_trim_context_fraction
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6,
    ?7, ?8, ?9, ?10, ?11, ?12,
    ?13, ?14, ?15, ?16, ?17,
    ?18, ?19,
    ?20, ?21, ?22, ?23, ?24, ?25,
    ?26, ?27,
    ?28, ?29, ?30, ?31, ?32,
    ?33, ?34,
    ?35, ?36, ?37, ?38,
    ?39, ?40, ?41,
    ?42,
    ?43,
    ?44,
    ?45,
    ?46,
    ?47,
    ?48,
    ?49,
    ?50,
    ?51,
    ?52,
    ?53,
    ?54,
    ?55,
    ?56
)";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Outcome;
    use rusqlite::Connection;
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Build a minimal valid record with the given id; `enrich` lets a
    /// test populate the columns it asserts on.
    fn record(request_id: &str) -> UsageRecord {
        UsageRecord {
            ts_start: 0,
            ts_end: 0,
            request_id: request_id.to_string(),
            ingress_dialect: "openai".to_string(),
            requested_model: "m".to_string(),
            alias: "a".to_string(),
            model: None,
            upstream: None,
            provider: None,
            provider_kind: None,
            seat: None,
            session_id: None,
            stream: false,
            max_tokens_req: None,
            tool_count: 0,
            thinking_req: None,
            thinking_req_kind: None,
            msg_count: 1,
            service_tier: None,
            outcome: Outcome::Ok,
            http_status: None,
            error_class: None,
            finish_reason: None,
            attempt_count: 1,
            fallback_count: 0,
            latency_ms: 0,
            ttfb_ms: None,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            server_tool_use: None,
            quota_claim: None,
            quota_status: None,
            quota_overage_status: None,
            quota_utilization: None,
            quota_overage_utilization: None,
            quota_reset: None,
            quota_extras: None,
            extra: None,
            strategy: None,
            reduction_strategy: None,
            selection_decision: None,
            would_trim_tokens: None,
            would_trim_break_even_k: None,
            would_trim_k_floor: None,
            would_trim_shadow_misfire: None,
            would_trim_dedup_tokens: None,
            would_trim_supersession_tokens: None,
            would_trim_path_units: None,
            would_trim_path_extractable: None,
            would_trim_recorder_version: None,
            would_trim_raw_marks: None,
            would_trim_context_fraction: None,
        }
    }

    fn temp_path() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("usage.db");
        (dir, path)
    }

    fn row_count(path: &PathBuf) -> i64 {
        let conn = Connection::open(path).expect("read open");
        conn.query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .expect("count")
    }

    /// Spin until the persisted counter reaches `want` or a deadline
    /// passes; returns whether it was reached.
    fn wait_persisted(counters: &Arc<UsageCounters>, want: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while counters.persisted() < want {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[tokio::test]
    async fn try_send_round_trips_a_representative_row() {
        // Arrange
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
        let mut rec = record("rt-1");
        rec.outcome = Outcome::Timeout;
        rec.stream = true;
        rec.input_tokens = Some(42);
        rec.quota_extras = Some(json!({"plan": "pro"}));
        rec.reduction_strategy = Some("applied".into());
        rec.selection_decision = Some("sticky_stay".into());

        // Act
        handle.try_send(rec);
        assert!(wait_persisted(handle.counters(), 1), "row not persisted");
        writer.shutdown();

        // Assert: exact bound values for the representative row.
        let conn = Connection::open(&path).expect("read");
        let (outcome, stream, input, extras, reduction, selection): (
            String,
            i64,
            i64,
            String,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT outcome, stream, input_tokens, quota_extras, reduction_strategy, selection_decision FROM requests WHERE request_id='rt-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .expect("row");
        assert_eq!(outcome, "timeout");
        assert_eq!(stream, 1);
        assert_eq!(input, 42);
        assert_eq!(extras, "{\"plan\":\"pro\"}");
        assert_eq!(reduction, "applied");
        assert_eq!(selection, "sticky_stay");
    }

    #[tokio::test]
    async fn try_send_round_trips_every_v8_attribution_column() {
        // Arrange: every v8 column carries a non-NULL value.
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
        let mut rec = record("rt-v8");
        rec.would_trim_dedup_tokens = Some(1_200);
        rec.would_trim_supersession_tokens = Some(800);
        rec.would_trim_path_units = Some(10);
        rec.would_trim_path_extractable = Some(7);
        rec.would_trim_recorder_version = Some(1);
        rec.would_trim_raw_marks = Some(json!([{"kind": "dedup", "index": 0}]));
        rec.would_trim_context_fraction = Some(0.25);

        // Act
        handle.try_send(rec);
        assert!(wait_persisted(handle.counters(), 1), "row not persisted");
        writer.shutdown();

        // Assert: every new column reads back exactly what was sent.
        let conn = Connection::open(&path).expect("read");
        let (
            dedup,
            supersession,
            path_units,
            path_extractable,
            recorder_version,
            raw_marks,
            context_fraction,
        ): (i64, i64, i64, i64, i64, String, f64) = conn
            .query_row(
                "SELECT would_trim_dedup_tokens, would_trim_supersession_tokens, \
                 would_trim_path_units, would_trim_path_extractable, \
                 would_trim_recorder_version, would_trim_raw_marks, \
                 would_trim_context_fraction \
                 FROM requests WHERE request_id='rt-v8'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .expect("row");
        assert_eq!(dedup, 1_200);
        assert_eq!(supersession, 800);
        assert_eq!(path_units, 10);
        assert_eq!(path_extractable, 7);
        assert_eq!(recorder_version, 1);
        assert_eq!(raw_marks, "[{\"index\":0,\"kind\":\"dedup\"}]");
        assert_eq!(context_fraction, 0.25);
    }

    #[test]
    fn capped_raw_marks_text_stores_small_blob_verbatim() {
        // Arrange
        let value = Some(json!([{"kind": "dedup", "index": 0}]));

        // Act
        let text = capped_raw_marks_text(&value);

        // Assert
        assert_eq!(text, Some("[{\"index\":0,\"kind\":\"dedup\"}]".to_string()));
    }

    #[test]
    fn capped_raw_marks_text_bounds_an_over_cap_array() {
        // Arrange: an array whose full serialization exceeds the cap.
        let big_item = "x".repeat(MAX_RAW_MARKS_BYTES / 4);
        let marks: Vec<serde_json::Value> =
            (0..8).map(|i| json!({"i": i, "pad": big_item})).collect();
        let value = Some(serde_json::Value::Array(marks));

        // Act
        let text = capped_raw_marks_text(&value).expect("bounded blob present");

        // Assert: bounded, and still valid, parseable JSON.
        assert!(
            text.len() <= MAX_RAW_MARKS_BYTES,
            "capped blob must not exceed the byte cap, got {} bytes",
            text.len()
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("capped blob is valid json");
        let kept = parsed.as_array().expect("capped blob is a json array");
        assert!(
            kept.len() < 8,
            "over-cap array must drop trailing elements, kept {}",
            kept.len()
        );
    }

    #[test]
    fn capped_raw_marks_text_returns_none_for_missing_value() {
        // Act + Assert
        assert_eq!(capped_raw_marks_text(&None), None);
    }

    #[test]
    fn capped_raw_marks_text_returns_none_for_over_cap_non_array() {
        // Arrange: a non-array value (no element boundary to trim) whose
        // full serialization exceeds the cap.
        let value = Some(serde_json::Value::String("x".repeat(MAX_RAW_MARKS_BYTES)));

        // Act
        let text = capped_raw_marks_text(&value);

        // Assert: degrades to None rather than storing invalid JSON.
        assert_eq!(text, None);
    }

    #[tokio::test]
    async fn duplicate_request_id_persists_one_row() {
        // Arrange
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

        // Act
        handle.try_send(record("dup"));
        handle.try_send(record("dup"));
        assert!(wait_persisted(handle.counters(), 1), "first not persisted");
        writer.shutdown();

        // Assert: INSERT OR IGNORE collapsed the duplicate, and the
        // persisted counter reflects the single real insert (not the
        // ignored duplicate).
        assert_eq!(row_count(&path), 1);
        assert_eq!(handle.counters().persisted(), 1);
    }

    #[tokio::test]
    async fn disabled_gate_drops_without_overflow_count() {
        // Arrange
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, false);

        // Act: disabled -> dropped at the gate.
        handle.try_send(record("gated"));
        // Flip on at runtime; subsequent records write with no restart.
        handle.set_enabled(true);
        handle.try_send(record("live"));
        assert!(wait_persisted(handle.counters(), 1), "live not persisted");
        writer.shutdown();

        // Assert
        assert_eq!(handle.counters().dropped_disabled(), 1);
        assert_eq!(handle.counters().dropped_full(), 0);
        assert_eq!(row_count(&path), 1);
    }

    #[tokio::test]
    async fn unopenable_db_degrades_but_handle_still_accepts() {
        // Arrange: point at a path whose parent is a file, so open fails.
        let dir = TempDir::new().expect("tempdir");
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write blocker");
        let bad_path = blocker.join("usage.db");
        let (handle, writer) = UsageWriter::start(bad_path, CHANNEL_CAPACITY, 0, true);

        // Act: try_send must not panic or error even with no DB.
        handle.try_send(record("degraded"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handle.counters().write_errors() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "no write error counted"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        writer.shutdown();

        // Assert: open failure + dropped row both counted; nothing persisted.
        assert!(handle.counters().write_errors() >= 1);
        assert_eq!(handle.counters().persisted(), 0);
    }

    #[tokio::test]
    async fn shutdown_drains_queued_rows() {
        // Arrange
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

        // Act: enqueue many, then shut down and let the bounded drain run.
        let n = 200;
        for i in 0..n {
            handle.try_send(record(&format!("drain-{i}")));
        }
        writer.shutdown();

        // Assert: a healthy DB drains every queued row before stopping.
        assert_eq!(row_count(&path), n as i64);
    }

    #[tokio::test]
    async fn full_channel_drops_and_counts_without_blocking() {
        // Arrange: a handle over a channel whose receiver is never polled,
        // so the queue fills and stays full.
        let capacity = 2usize;
        let (tx, _rx) = mpsc::channel::<WriterMessage>(capacity);
        let counters = Arc::new(UsageCounters::default());
        let enabled = Arc::new(AtomicBool::new(true));
        let handle = UsageHandle::new(tx, enabled, Arc::clone(&counters));

        // Act: send well past capacity, timing the loop to prove try_send
        // never blocks (a blocking send against a full channel would dwarf
        // this bound).
        let sends = 50usize;
        let start = std::time::Instant::now();
        for i in 0..sends {
            handle.try_send(record(&format!("full-{i}")));
        }
        let elapsed = start.elapsed();

        // Assert: completed far under any blocking threshold; exact split
        // of enqueued (capacity) vs overflow drops (the rest).
        assert!(
            elapsed < Duration::from_millis(100),
            "send loop blocked: took {elapsed:?} for {sends} sends"
        );
        assert_eq!(counters.dropped_disabled(), 0);
        assert_eq!(counters.enqueued(), capacity as u64);
        assert_eq!(counters.dropped_full(), (sends - capacity) as u64);
    }

    #[tokio::test]
    async fn drop_without_shutdown_is_non_blocking() {
        // Arrange: a live writer with rows queued behind it.
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path, CHANNEL_CAPACITY, 0, true);
        for i in 0..100 {
            handle.try_send(record(&format!("drop-{i}")));
        }

        // Act: drop the writer instead of calling shutdown. Drop must
        // signal-and-detach, never run the 5s drain.
        let start = std::time::Instant::now();
        drop(writer);
        let elapsed = start.elapsed();

        // Assert: returns effectively instantly (well under the 5s drain
        // deadline); at-most-once durability means queued rows may be lost.
        assert!(
            elapsed < Duration::from_millis(500),
            "Drop blocked: took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_then_drop_does_not_double_join() {
        // Arrange
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path, CHANNEL_CAPACITY, 0, true);
        handle.try_send(record("once"));
        assert!(wait_persisted(handle.counters(), 1), "row not persisted");

        // Act + Assert: explicit shutdown takes the join handle and
        // sender; the implicit Drop that runs as `shutdown` returns must
        // not panic or re-join (join/sender already taken).
        writer.shutdown();
    }

    #[test]
    fn degraded_writer_shutdown_is_noop() {
        // Arrange: a degraded pair as produced on thread-spawn failure.
        // The receiver is dropped to mirror the real path, where `rx` was
        // moved into the spawn closure that never ran -- so the channel is
        // closed and sends accept-and-drop.
        let (tx, rx) = mpsc::channel::<WriterMessage>(2);
        drop(rx);
        let counters = Arc::new(UsageCounters::default());
        let enabled = Arc::new(AtomicBool::new(true));
        let (handle, writer) = UsageWriter::degraded(tx, enabled, counters);

        // Act: try_send accepts-and-drops (channel closed -> overflow);
        // shutdown returns immediately with no thread to join.
        handle.try_send(record("degraded"));
        let start = std::time::Instant::now();
        writer.shutdown();

        // Assert
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(handle.counters().dropped_full(), 1);
    }

    #[tokio::test]
    async fn startup_prune_deletes_old_keeps_new() {
        // Arrange: seed an old and a new row directly, then start the
        // writer with a retention window that should drop the old one.
        let (_dir, path) = temp_path();
        {
            let db = crate::db::open(&path).expect("seed open");
            let now = now_epoch_ms();
            let day = 86_400_000i64;
            for (id, ts) in [("old", now - 40 * day), ("new", now - day)] {
                db.conn()
                    .execute(
                        "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                         requested_model, alias, stream, outcome, latency_ms, tool_count, \
                         msg_count, attempt_count, fallback_count) \
                         VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0)",
                        rusqlite::params![ts, id],
                    )
                    .expect("seed insert");
            }
        }

        // Act: 30-day retention prunes "old" at startup.
        let (_handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 30, true);
        writer.shutdown();

        // Assert
        let conn = Connection::open(&path).expect("read");
        let survivor: String = conn
            .query_row("SELECT request_id FROM requests", [], |r| r.get(0))
            .expect("survivor");
        assert_eq!(survivor, "new");
        assert_eq!(row_count(&path), 1);
    }

    #[tokio::test]
    async fn retention_zero_keeps_everything_at_startup() {
        // Arrange
        let (_dir, path) = temp_path();
        {
            let db = crate::db::open(&path).expect("seed open");
            db.conn()
                .execute(
                    "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                     requested_model, alias, stream, outcome, latency_ms, tool_count, \
                     msg_count, attempt_count, fallback_count) \
                     VALUES (0, 0, 'ancient', 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0)",
                    [],
                )
                .expect("seed insert");
        }

        // Act
        let (_handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
        writer.shutdown();

        // Assert: retention=0 means no prune.
        assert_eq!(row_count(&path), 1);
    }

    /// Build a representative learn event.
    fn learn_event(capability_key: &str) -> CapabilityLearnEvent {
        CapabilityLearnEvent {
            ts: 123,
            state_key: "gpt-nick".to_string(),
            capability_key: capability_key.to_string(),
            provider_kind: "anthropic-api".to_string(),
            signal_tier: "inferred".to_string(),
            observations: 2,
            upstream_status: 400,
            remapped: false,
            request_features: vec!["thinking".to_string(), capability_key.to_string()],
        }
    }

    fn learn_event_row_count(path: &PathBuf) -> i64 {
        let conn = Connection::open(path).expect("read open");
        conn.query_row("SELECT COUNT(*) FROM capability_learn_events", [], |r| {
            r.get(0)
        })
        .expect("count")
    }

    /// Spin until the learn-event persisted counter reaches `want` or a
    /// deadline passes; returns whether it was reached.
    fn wait_learn_events_persisted(counters: &Arc<UsageCounters>, want: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while counters.learn_events_persisted() < want {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[tokio::test]
    async fn learn_event_round_trips_through_the_shared_writer() {
        // Arrange
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

        // Act
        handle.try_send_learn_event(learn_event("web_search"));
        assert!(
            wait_learn_events_persisted(handle.counters(), 1),
            "learn event not persisted"
        );
        // Drop the handle's sender clone before draining, or shutdown blocks
        // on the deadline waiting for a channel that never closes.
        drop(handle);
        writer.shutdown();

        // Assert: the row round-trips through the one actor / one connection,
        // and request rows are untouched.
        assert_eq!(learn_event_row_count(&path), 1);
        assert_eq!(row_count(&path), 0);
        let conn = Connection::open(&path).expect("read");
        let (state_key, capability_key, tier, observations, status, remapped, features): (
            String,
            String,
            String,
            i64,
            i64,
            i64,
            String,
        ) = conn
            .query_row(
                "SELECT state_key, capability_key, signal_tier, observations, upstream_status, \
                 remapped, request_features FROM capability_learn_events",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .expect("row");
        assert_eq!(state_key, "gpt-nick");
        assert_eq!(capability_key, "web_search");
        assert_eq!(tier, "inferred");
        assert_eq!(observations, 2);
        assert_eq!(status, 400);
        assert_eq!(remapped, 0);
        assert_eq!(features, "[\"thinking\",\"web_search\"]");
    }

    #[tokio::test]
    async fn full_channel_drops_learn_events_without_blocking() {
        // Arrange: a handle over a channel whose receiver is never polled.
        let capacity = 2usize;
        let (tx, _rx) = mpsc::channel::<WriterMessage>(capacity);
        let counters = Arc::new(UsageCounters::default());
        let enabled = Arc::new(AtomicBool::new(true));
        let handle = UsageHandle::new(tx, enabled, Arc::clone(&counters));

        // Act: send well past capacity, timing the loop to prove the enqueue
        // never blocks.
        let sends = 50usize;
        let start = std::time::Instant::now();
        for i in 0..sends {
            handle.try_send_learn_event(learn_event(&format!("cap-{i}")));
        }
        let elapsed = start.elapsed();

        // Assert: no blocking; exact split of enqueued (capacity) vs overflow
        // drops (the rest), tracked on the learn-event counters.
        assert!(
            elapsed < Duration::from_millis(100),
            "learn-event send loop blocked: took {elapsed:?} for {sends} sends"
        );
        assert_eq!(counters.learn_events_enqueued(), capacity as u64);
        assert_eq!(
            counters.learn_events_dropped_full(),
            (sends - capacity) as u64
        );
    }

    #[tokio::test]
    async fn disabled_gate_drops_learn_events() {
        // Arrange
        let (_dir, path) = temp_path();
        let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, false);

        // Act: disabled -> dropped at the gate (counted as a disabled-drop,
        // not a learn-event overflow).
        handle.try_send_learn_event(learn_event("gated"));
        // Snapshot the counters and drop the handle's sender clone before
        // draining, or shutdown blocks on the deadline.
        let counters = Arc::clone(handle.counters());
        drop(handle);
        writer.shutdown();

        // Assert
        assert_eq!(counters.dropped_disabled(), 1);
        assert_eq!(counters.learn_events_dropped_full(), 0);
        assert_eq!(counters.learn_events_persisted(), 0);
        assert_eq!(learn_event_row_count(&path), 0);
    }
}
