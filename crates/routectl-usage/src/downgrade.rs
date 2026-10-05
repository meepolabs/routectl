//! Offline schema downgrade from v17 back to v16.
//!
//! A v16 binary refuses a v17 file (`VersionTooNew`), so rolling a hop back to
//! the previous binary needs the version stamp lowered first. v17 is strictly
//! additive -- two nullable columns appended to `capability_events` -- and a
//! v16 binary names every `capability_events` column it reads or writes, so the
//! trailing pair is invisible to it. The downgrade therefore only re-stamps
//! `PRAGMA user_version` and `meta.schema_version`; it never drops a column or
//! rewrites a row, and a later v17 open re-stamps the file without re-adding
//! anything (the forward step's `ADD COLUMN`s are guarded).
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
//! The file is never opened with `SQLITE_OPEN_CREATE`, and the stamps change in
//! one transaction, so a refusal or a crash leaves the file exactly as it was.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};

use crate::schema::META_SCHEMA_VERSION;

/// The schema version this downgrade accepts as input.
const FROM_VERSION: i64 = 17;

/// The schema version this downgrade stamps.
const TO_VERSION: i64 = 16;

/// How long to wait for the exclusive lock before concluding another
/// connection holds the file. Short: an attached daemon never lets go, so a
/// long wait only delays the refusal.
const LOCK_TIMEOUT_MS: u64 = 250;

/// The `capability_events` columns as v16 created them, in physical order,
/// with their declared types. A frozen record of the historical shape -- it
/// must not follow later DDL edits.
const V16_CAPABILITY_EVENTS_COLUMNS: &[(&str, &str)] = &[
    ("id", "INTEGER"),
    ("ts", "INTEGER"),
    ("lane_key", "TEXT"),
    ("capability", "TEXT"),
    ("verdict", "TEXT"),
    ("phase", "TEXT"),
    ("source", "TEXT"),
    ("tier", "TEXT"),
    ("evidence_class", "TEXT"),
    ("upstream_token", "TEXT"),
    ("catalog_version", "INTEGER"),
    ("overlay_revision", "INTEGER"),
];

/// The columns v17 appended after the v16 set, in order.
const V17_ADDITIVE_COLUMNS: &[(&str, &str)] =
    &[("provider_kind", "TEXT"), ("vocab_version", "INTEGER")];

/// Why a downgrade was refused or failed. Every variant leaves the file
/// unmodified.
#[derive(Debug, thiserror::Error)]
pub enum DowngradeError {
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

    /// The file's `PRAGMA user_version` is not the version this downgrade
    /// accepts.
    #[error("usage db schema version is {found}; this downgrade only accepts {expected}")]
    UnexpectedVersion {
        /// The on-disk version.
        found: i64,
        /// The version this downgrade accepts.
        expected: i64,
    },

    /// `meta.schema_version` disagrees with `PRAGMA user_version`.
    #[error("usage db meta schema_version is {found:?}; expected {expected:?}")]
    MetaMismatch {
        /// The stored value, or `None` when the row is absent.
        found: Option<String>,
        /// The value a well-formed file carries.
        expected: String,
    },

    /// `capability_events` is not exactly the v16 columns plus the v17
    /// additive pair, so stamping it v16 could hand a v16 binary a layout it
    /// does not understand.
    #[error("capability_events columns are not the additive v17 shape: found {found:?}")]
    NotAdditive {
        /// The `(name, declared type)` columns actually present, in order.
        found: Vec<(String, String)>,
    },

    /// Any other SQLite failure.
    #[error("usage db downgrade failed: {0}")]
    Sqlite(#[source] rusqlite::Error),
}

/// Stamp an exactly-additive v17 usage DB at `path` as v16, so a v16 binary
/// opens it.
///
/// Refuses, leaving the file untouched, when the file is missing, when any
/// other connection has it open ([`DowngradeError::InUse`]), when it is not
/// at v17, or when its `capability_events` columns are anything other than the
/// v16 set followed by `provider_kind TEXT` and `vocab_version INTEGER`. On
/// success `PRAGMA user_version` and `meta.schema_version` both read 16,
/// committed in one transaction; no column or row is touched.
pub fn downgrade_to_v16(path: impl AsRef<Path>) -> Result<(), DowngradeError> {
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
    verify_additive_v17(&tx)?;
    tx.execute_batch(&format!("PRAGMA user_version = {TO_VERSION}"))
        .map_err(classify)?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, TO_VERSION.to_string()],
    )
    .map_err(classify)?;
    tx.commit().map_err(classify)
}

/// Check the file is exactly the additive v17 shape: the version stamp, its
/// human-readable mirror, and the `capability_events` column list.
fn verify_additive_v17(conn: &Connection) -> Result<(), DowngradeError> {
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(classify)?;
    if found != FROM_VERSION {
        return Err(DowngradeError::UnexpectedVersion {
            found,
            expected: FROM_VERSION,
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
    let expected_meta = FROM_VERSION.to_string();
    if meta.as_deref() != Some(expected_meta.as_str()) {
        return Err(DowngradeError::MetaMismatch {
            found: meta,
            expected: expected_meta,
        });
    }

    let columns = capability_events_columns(conn)?;
    if !is_additive_v17(&columns) {
        return Err(DowngradeError::NotAdditive { found: columns });
    }
    Ok(())
}

/// Whether `columns` is exactly the v16 set followed by the v17 additive pair:
/// same names, same declared types, same order, nothing missing or extra.
fn is_additive_v17(columns: &[(String, String)]) -> bool {
    let expected = V16_CAPABILITY_EVENTS_COLUMNS
        .iter()
        .chain(V17_ADDITIVE_COLUMNS);
    columns.len() == V16_CAPABILITY_EVENTS_COLUMNS.len() + V17_ADDITIVE_COLUMNS.len()
        && columns
            .iter()
            .zip(expected)
            .all(|((name, ty), (want_name, want_ty))| name == want_name && ty == want_ty)
}

/// The `(name, declared type)` of every `capability_events` column, in
/// physical order. An absent table yields an empty list, which the shape
/// check rejects.
fn capability_events_columns(conn: &Connection) -> Result<Vec<(String, String)>, DowngradeError> {
    let mut stmt = conn
        .prepare("SELECT name, type FROM pragma_table_info('capability_events')")
        .map_err(classify)?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
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
