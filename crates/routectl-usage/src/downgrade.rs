//! Offline schema downgrade: v18 -> v17 -> v16.
//!
//! A binary refuses a file stamped with a newer schema version than it
//! understands (`VersionTooNew`), so rolling a host back to an earlier binary
//! needs the version stamp lowered first. Each step in [`STEPS`] undoes one
//! strictly additive forward step, so every downgrade is a re-stamp, never a
//! rewrite:
//!
//! - 18 -> 17: v18 appended eight nullable `requests` columns and created
//!   `ledger_gaps` with its covering `idx_ledger_gaps_ts_ms` index. A v17
//!   binary names every `requests` column it writes and never touches
//!   `ledger_gaps`, so the columns, the table and its index all stay in
//!   place. The shape check covers columns only: like every other index,
//!   the gap index is tolerated, not verified, because no older binary
//!   reads or depends on it.
//! - 17 -> 16: v17 appended two nullable `capability_events` columns, which a
//!   v16 binary never names.
//!
//! Each step first checks that the tables it covers are exactly the earlier
//! shape plus the additive set (read from `PRAGMA table_xinfo` alone, so the
//! check costs the same on any table size), then re-stamps
//! `PRAGMA user_version` and `meta.schema_version`. No column, row or table is
//! dropped, and a later open by the newer binary re-stamps the file without
//! re-adding anything (each forward step's `ADD COLUMN`s are guarded and its
//! tables use `IF NOT EXISTS`).
//!
//! # The daemon must be stopped
//!
//! The daemon's usage writer holds one connection open for its whole lifetime.
//! A plain `BEGIN EXCLUSIVE` does not see it: in WAL mode an exclusive
//! transaction only excludes other WRITERS, and an idle connection is not one.
//! This connection instead switches to `locking_mode = EXCLUSIVE` before its
//! first read. In WAL mode that makes SQLite keep the wal-index in private
//! memory, which it may only do when no other connection is attached, so the
//! first transaction fails busy while ANY other connection -- the daemon's
//! writer, a status poll, a `routectl usage` viewer -- has the file open. That
//! busy is the refusal. The lock is held until the version change commits, so a
//! daemon starting mid-downgrade waits rather than racing it.
//!
//! The file is never opened with `SQLITE_OPEN_CREATE`, and every step runs in
//! one transaction, so a refusal in any step or a crash leaves the file exactly
//! as it was.

use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};

use crate::migrate::V18_REQUESTS_COLUMNS;
use crate::schema::META_SCHEMA_VERSION;

/// One downgrade step: the version it accepts and the shape check that must
/// pass before the file is re-stamped one version lower.
struct Step {
    from: i64,
    verify_shape: fn(&Connection) -> Result<(), DowngradeError>,
}

/// Every downgrade step, newest first. A downgrade runs a contiguous run of
/// them, starting at the file's version, until it reaches the target.
const STEPS: &[Step] = &[
    Step {
        from: 18,
        verify_shape: verify_additive_v18,
    },
    Step {
        from: 17,
        verify_shape: verify_additive_v17,
    },
];

/// The newest schema version any downgrade accepts as input.
const NEWEST_INPUT: i64 = STEPS[0].from;

/// The schema versions a file can be downgraded to: one below each step's
/// input version.
pub const SUPPORTED_TARGETS: RangeInclusive<i64> =
    (STEPS[STEPS.len() - 1].from - 1)..=(NEWEST_INPUT - 1);

/// How long to wait for the exclusive lock before concluding another
/// connection holds the file. Short: an attached daemon never lets go, so a
/// long wait only delays the refusal.
const LOCK_TIMEOUT_MS: u64 = 250;

/// One column as `PRAGMA table_xinfo` describes it: name, declared type,
/// `NOT NULL`, default expression, primary-key position, and the hidden flag
/// (non-zero for a generated column).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnShape {
    name: String,
    declared_type: String,
    not_null: bool,
    /// The default expression's text, or `None` when there is no default.
    default: Option<String>,
    /// 1-based position in the primary key, or 0 when not part of it.
    primary_key: i64,
    /// `table_xinfo`'s hidden flag: 0 for an ordinary column.
    hidden: i64,
}

/// Which part of a step's additive-shape check a table failed. A stable
/// reason, not the columns themselves: those carry file-supplied text (a
/// default expression) that has no business in an error a caller may log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotAdditiveReason {
    /// The table has a different number of columns than the earlier version
    /// plus the additive set (an absent table has none).
    ColumnCount,
    /// The count matches, but a column differs from its frozen record by
    /// name, type, nullability, default, key position or hidden flag.
    ColumnShape,
}

impl NotAdditiveReason {
    /// The fixed description of the failed check.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::ColumnCount => "the column count differs from the expected additive shape",
            Self::ColumnShape => "a column's shape differs from the expected additive shape",
        }
    }
}

/// A frozen column record: `(name, declared type, not null, primary-key
/// position)`. Every frozen column has no default and is not hidden.
type FrozenColumn = (&'static str, &'static str, bool, i64);

/// The `requests` columns as v17 created them, in physical order. A frozen
/// record of the historical shape -- it must not follow later DDL edits. The
/// table has no declared primary key (rowid), so every position is 0.
const V17_REQUESTS_COLUMNS: &[FrozenColumn] = &[
    ("ts_start", "INTEGER", true, 0),
    ("ts_end", "INTEGER", true, 0),
    ("request_id", "TEXT", true, 0),
    ("ingress_dialect", "TEXT", true, 0),
    ("requested_model", "TEXT", true, 0),
    ("alias", "TEXT", true, 0),
    ("model", "TEXT", false, 0),
    ("upstream", "TEXT", false, 0),
    ("provider", "TEXT", false, 0),
    ("provider_kind", "TEXT", false, 0),
    ("seat", "TEXT", false, 0),
    ("session_id", "TEXT", false, 0),
    ("stream", "INTEGER", true, 0),
    ("max_tokens_req", "INTEGER", false, 0),
    ("tool_count", "INTEGER", true, 0),
    ("thinking_req", "INTEGER", false, 0),
    ("thinking_req_kind", "TEXT", false, 0),
    ("msg_count", "INTEGER", true, 0),
    ("service_tier", "TEXT", false, 0),
    ("outcome", "TEXT", true, 0),
    ("http_status", "INTEGER", false, 0),
    ("error_class", "TEXT", false, 0),
    ("finish_reason", "TEXT", false, 0),
    ("attempt_count", "INTEGER", true, 0),
    ("fallback_count", "INTEGER", true, 0),
    ("latency_ms", "INTEGER", true, 0),
    ("ttfb_ms", "INTEGER", false, 0),
    ("input_tokens", "INTEGER", false, 0),
    ("output_tokens", "INTEGER", false, 0),
    ("reasoning_tokens", "INTEGER", false, 0),
    ("cache_read", "INTEGER", false, 0),
    ("cache_write_5m", "INTEGER", false, 0),
    ("cache_write_1h", "INTEGER", false, 0),
    ("server_tool_use", "TEXT", false, 0),
    ("quota_claim", "TEXT", false, 0),
    ("quota_status", "TEXT", false, 0),
    ("quota_overage_status", "TEXT", false, 0),
    ("quota_utilization", "REAL", false, 0),
    ("quota_overage_utilization", "REAL", false, 0),
    ("quota_reset", "INTEGER", false, 0),
    ("quota_extras", "TEXT", false, 0),
    ("extra", "TEXT", false, 0),
    ("strategy", "TEXT", false, 0),
    ("reduction_strategy", "TEXT", false, 0),
    ("selection_decision", "TEXT", false, 0),
    ("would_trim_tokens", "INTEGER", false, 0),
    ("would_trim_break_even_k", "REAL", false, 0),
    ("would_trim_k_floor", "REAL", false, 0),
    ("would_trim_shadow_misfire", "INTEGER", false, 0),
    ("would_trim_dedup_tokens", "INTEGER", false, 0),
    ("would_trim_supersession_tokens", "INTEGER", false, 0),
    ("would_trim_path_units", "INTEGER", false, 0),
    ("would_trim_path_extractable", "INTEGER", false, 0),
    ("would_trim_recorder_version", "INTEGER", false, 0),
    ("would_trim_raw_marks", "TEXT", false, 0),
    ("would_trim_context_fraction", "REAL", false, 0),
    ("resolved_class", "TEXT", false, 0),
    ("calib_estimated_tokens", "INTEGER", false, 0),
    ("calib_prompt_tokens", "INTEGER", false, 0),
    ("reduction_decision", "TEXT", false, 0),
    ("reduction_strings_compressed", "INTEGER", false, 0),
    ("reduction_strings_skipped", "INTEGER", false, 0),
    ("reduction_strings_rejected", "INTEGER", false, 0),
    ("reduction_bytes_saved", "INTEGER", false, 0),
    ("cache_front_decision", "TEXT", false, 0),
    ("cache_terminal_decision", "TEXT", false, 0),
    ("prefix_epoch_event", "INTEGER", false, 0),
];

/// The `ledger_gaps` columns as v18 created them, in physical order.
const V18_LEDGER_GAPS_COLUMNS: &[FrozenColumn] = &[
    ("ts_ms", "INTEGER", true, 0),
    ("cause", "TEXT", true, 0),
    ("dropped", "INTEGER", true, 0),
];

/// The `capability_events` columns as v16 created them, in physical order. A
/// frozen record of the historical shape -- it must not follow later DDL
/// edits.
const V16_CAPABILITY_EVENTS_COLUMNS: &[FrozenColumn] = &[
    ("id", "INTEGER", false, 1),
    ("ts", "INTEGER", true, 0),
    ("lane_key", "TEXT", false, 0),
    ("capability", "TEXT", false, 0),
    ("verdict", "TEXT", false, 0),
    ("phase", "TEXT", false, 0),
    ("source", "TEXT", false, 0),
    ("tier", "TEXT", false, 0),
    ("evidence_class", "TEXT", false, 0),
    ("upstream_token", "TEXT", false, 0),
    ("catalog_version", "INTEGER", false, 0),
    ("overlay_revision", "INTEGER", false, 0),
];

/// The columns v17 appended after the v16 `capability_events` set, in order.
const V17_ADDITIVE_COLUMNS: &[FrozenColumn] = &[
    ("provider_kind", "TEXT", false, 0),
    ("vocab_version", "INTEGER", false, 0),
];

/// Why a downgrade was refused or failed. Every variant leaves the file
/// unmodified.
#[derive(Debug, thiserror::Error)]
pub enum DowngradeError {
    /// The requested target is not one this build can downgrade to.
    #[error(
        "cannot downgrade to schema version {target}; supported targets are {} through {}",
        SUPPORTED_TARGETS.start(),
        SUPPORTED_TARGETS.end()
    )]
    UnsupportedTarget {
        /// The requested target version.
        target: i64,
    },

    /// No file exists at the path; nothing is created.
    #[error("no usage db at {path}")]
    NoData {
        /// The path that held no file.
        path: String,
    },

    /// The file could not be opened read-write.
    #[error("failed to open usage db {path}: {source}")]
    Open {
        /// The path that could not be opened.
        path: String,
        /// The underlying SQLite error.
        #[source]
        source: rusqlite::Error,
    },

    /// Another connection has the file open -- normally the running daemon.
    #[error(
        "the usage db is in use by another process; stop the routectl daemon (and any \
         reader of the db) and retry"
    )]
    InUse,

    /// The file's `PRAGMA user_version` is not one a downgrade to `target`
    /// accepts: it must be above `target` and no newer than the newest step.
    #[error(
        "usage db schema version is {found}; a downgrade to {target} accepts versions {} \
         through {NEWEST_INPUT}",
        .target + 1
    )]
    UnexpectedVersion {
        /// The on-disk version.
        found: i64,
        /// The requested target version.
        target: i64,
    },

    /// `meta.schema_version` disagrees with `PRAGMA user_version`.
    #[error("usage db meta schema_version is {found:?}; expected {expected:?}")]
    MetaMismatch {
        /// The stored value, or `None` when the row is absent.
        found: Option<String>,
        /// The value a well-formed file carries.
        expected: String,
    },

    /// A table a step covers is not exactly the earlier version's columns
    /// plus that step's additive set -- by name, type, nullability, default,
    /// key, or hidden flag -- so re-stamping it could hand an older binary a
    /// layout it does not understand. The message names only the table and
    /// which check failed.
    #[error("{table} is not the additive shape this downgrade expects: {}", .reason.description())]
    NotAdditive {
        /// The table that failed the check.
        table: &'static str,
        /// Which check failed.
        reason: NotAdditiveReason,
    },

    /// Any other SQLite failure.
    #[error("usage db downgrade failed: {0}")]
    Sqlite(#[source] rusqlite::Error),
}

/// Stamp the usage DB at `path` down to schema version `target`, so a binary
/// built for that version opens it.
///
/// Runs every step from the file's version down to `target` -- 18 -> 17,
/// then 17 -> 16 when `target` is 16 -- in one transaction. Each step checks
/// its tables are exactly the earlier shape plus the additive set the forward
/// step appended, then re-stamps `PRAGMA user_version` and
/// `meta.schema_version` one version lower; no column, row or table is
/// touched.
///
/// Refuses, leaving the file untouched, when `target` is outside
/// [`SUPPORTED_TARGETS`], when the file is missing, when any other connection
/// has it open ([`DowngradeError::InUse`]), when its version is not above
/// `target` or is newer than the newest step accepts, when its meta mirror
/// disagrees, or when any step's shape check fails.
pub fn downgrade_to(path: impl AsRef<Path>, target: i64) -> Result<(), DowngradeError> {
    if !SUPPORTED_TARGETS.contains(&target) {
        return Err(DowngradeError::UnsupportedTarget { target });
    }
    let path = path.as_ref();
    if !path.exists() {
        return Err(DowngradeError::NoData {
            path: path.display().to_string(),
        });
    }
    let mut conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(|source| {
            DowngradeError::Open {
                path: path.display().to_string(),
                source,
            }
        })?;
    conn.busy_timeout(Duration::from_millis(LOCK_TIMEOUT_MS))
        .map_err(DowngradeError::Sqlite)?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")
        .map_err(classify)?;

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .map_err(classify)?;
    let found = read_user_version(&tx)?;
    if found <= target || found > NEWEST_INPUT {
        return Err(DowngradeError::UnexpectedVersion { found, target });
    }
    for step in STEPS.iter().filter(|s| s.from <= found && s.from > target) {
        run_step(&tx, step)?;
    }
    tx.commit().map_err(classify)
}

/// Check the file is at `step.from` with a matching meta mirror, run the
/// step's shape check, then re-stamp both version values one lower.
fn run_step(conn: &Connection, step: &Step) -> Result<(), DowngradeError> {
    verify_stamps(conn, step.from)?;
    (step.verify_shape)(conn)?;
    let to = step.from - 1;
    conn.execute_batch(&format!("PRAGMA user_version = {to}"))
        .map_err(classify)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, to.to_string()],
    )
    .map_err(classify)?;
    Ok(())
}

fn read_user_version(conn: &Connection) -> Result<i64, DowngradeError> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(classify)
}

/// Check both version values read `expected`: the `PRAGMA user_version`
/// stamp and its human-readable mirror in `meta`.
fn verify_stamps(conn: &Connection, expected: i64) -> Result<(), DowngradeError> {
    let found = read_user_version(conn)?;
    if found != expected {
        return Err(DowngradeError::UnexpectedVersion {
            found,
            target: expected - 1,
        });
    }
    let meta: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [META_SCHEMA_VERSION],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(classify)?;
    let expected_meta = expected.to_string();
    if meta.as_deref() != Some(expected_meta.as_str()) {
        return Err(DowngradeError::MetaMismatch {
            found: meta,
            expected: expected_meta,
        });
    }
    Ok(())
}

/// The 18 -> 17 shape check: `requests` is the frozen v17 column list
/// followed by exactly the eight v18 columns (plain nullable, no default),
/// and `ledger_gaps` is exactly its v18 shape.
fn verify_additive_v18(conn: &Connection) -> Result<(), DowngradeError> {
    let requests: Vec<FrozenColumn> = V17_REQUESTS_COLUMNS
        .iter()
        .copied()
        .chain(
            V18_REQUESTS_COLUMNS
                .iter()
                .map(|&(name, declared_type)| (name, declared_type, false, 0)),
        )
        .collect();
    verify_table(conn, "requests", &requests)?;
    verify_table(conn, "ledger_gaps", V18_LEDGER_GAPS_COLUMNS)
}

/// The 17 -> 16 shape check: `capability_events` is the frozen v16 column
/// list followed by exactly `provider_kind TEXT` and `vocab_version INTEGER`.
fn verify_additive_v17(conn: &Connection) -> Result<(), DowngradeError> {
    let capability_events: Vec<FrozenColumn> = V16_CAPABILITY_EVENTS_COLUMNS
        .iter()
        .chain(V17_ADDITIVE_COLUMNS)
        .copied()
        .collect();
    verify_table(conn, "capability_events", &capability_events)
}

/// Check `table` is an ordinary table whose columns are exactly `expected`.
fn verify_table(
    conn: &Connection,
    table: &'static str,
    expected: &[FrozenColumn],
) -> Result<(), DowngradeError> {
    let columns = table_columns(conn, table)?;
    match not_additive_reason(&columns, expected) {
        Some(reason) => Err(DowngradeError::NotAdditive { table, reason }),
        None => Ok(()),
    }
}

/// Why `columns` is not exactly `expected`, or `None` when it is: every
/// column matching its frozen record in name, declared type, `NOT NULL`,
/// default, primary-key position and hidden flag, in order, nothing missing
/// or extra.
fn not_additive_reason(
    columns: &[ColumnShape],
    expected: &[FrozenColumn],
) -> Option<NotAdditiveReason> {
    if columns.len() != expected.len() {
        return Some(NotAdditiveReason::ColumnCount);
    }
    if columns.iter().zip(expected).all(matches_frozen) {
        None
    } else {
        Some(NotAdditiveReason::ColumnShape)
    }
}

/// Whether one live column matches its frozen record exactly.
fn matches_frozen((column, frozen): (&ColumnShape, &FrozenColumn)) -> bool {
    let &(name, declared_type, not_null, primary_key) = frozen;
    column.name == name
        && column.declared_type == declared_type
        && column.not_null == not_null
        && column.default.is_none()
        && column.primary_key == primary_key
        && column.hidden == 0
}

/// Every column of the ordinary table `table`, hidden ones included, in
/// physical order. A missing table, or a view by that name, yields an empty
/// list, which every shape check rejects. Reads only the
/// schema, never a row.
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnShape>, DowngradeError> {
    let mut stmt = conn
        .prepare(
            "SELECT name, type, \"notnull\", dflt_value, pk, hidden \
             FROM pragma_table_xinfo(?1) \
             WHERE EXISTS (SELECT 1 FROM sqlite_master \
                           WHERE type = 'table' AND name = ?1) \
             ORDER BY cid",
        )
        .map_err(classify)?;
    let rows = stmt
        .query_map([table], |row| {
            Ok(ColumnShape {
                name: row.get(0)?,
                declared_type: row.get(1)?,
                not_null: row.get(2)?,
                default: row.get(3)?,
                primary_key: row.get(4)?,
                hidden: row.get(5)?,
            })
        })
        .map_err(classify)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(classify)?;
    Ok(rows)
}

/// Map a SQLite error to the downgrade's vocabulary: a busy or locked file
/// means another connection holds it.
fn classify(err: rusqlite::Error) -> DowngradeError {
    match err.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => DowngradeError::InUse,
        _ => DowngradeError::Sqlite(err),
    }
}

#[cfg(test)]
#[path = "downgrade_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "downgrade_v18_tests.rs"]
mod v18_tests;
