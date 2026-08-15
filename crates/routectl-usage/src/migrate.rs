//! PRAGMA user_version migrate-on-open ladder.
//!
//! On open, read `PRAGMA user_version` and apply forward migrations one
//! step at a time until the DB reaches `SCHEMA_VERSION`. Each step is
//! idempotent (DDL uses `IF NOT EXISTS`), so opening an already-current
//! DB is a no-op. Add a new step by extending the ladder and bumping
//! `SCHEMA_VERSION` in `schema.rs`.

use rusqlite::Connection;

use crate::schema::{
    CREATE_CAPABILITY_EVENTS_TABLE, CREATE_CAPABILITY_EVENTS_TS_INDEX,
    CREATE_CAPABILITY_LEARN_EVENTS_TABLE, CREATE_META_TABLE, CREATE_REQUESTS_TABLE,
    CREATE_TS_START_INDEX, META_CREATED_AT_MS, META_SCHEMA_VERSION, SCHEMA_VERSION,
};

/// Errors raised while migrating the usage DB. The caller can degrade
/// gracefully on any of these rather than crashing the proxy.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// A SQLite operation failed during migration.
    #[error("sqlite migration failed: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The DB reports a version newer than this build understands. We do
    /// not downgrade; the caller should refuse to write rather than
    /// corrupt a forward-versioned file.
    #[error("db schema version {found} is newer than supported {supported}")]
    VersionTooNew {
        /// On-disk schema version.
        found: i64,
        /// Highest version this binary understands.
        supported: i64,
    },
}

/// Read the current `PRAGMA user_version`.
fn read_user_version(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// Apply the v0 -> v1 step atomically: create the tables + index, seed
/// `meta`, and bump `PRAGMA user_version`, all in one transaction. A
/// crash mid-step rolls back, so the DB never lands in a
/// tables-without-version state. Each statement is still idempotent
/// (`IF NOT EXISTS` / `INSERT OR IGNORE`) so a re-run is safe.
///
/// The version literals here are the LITERAL target of this step (1), not
/// `SCHEMA_VERSION`: a later schema bump must not make a v0 DB skip the
/// intervening forward steps. `CREATE_REQUESTS_TABLE` already carries the
/// current (v3) column set, so a fresh DB lands fully-shaped; the
/// `migrate_v1_to_v2` / `migrate_v2_to_v3` `ADD COLUMN` steps are
/// no-op-equivalents on a fresh DB because the loop only reaches them on a
/// DB that started below their target version.
fn migrate_v0_to_v1(conn: &Connection, now_ms: i64) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(CREATE_REQUESTS_TABLE)?;
    tx.execute_batch(CREATE_TS_START_INDEX)?;
    tx.execute_batch(CREATE_META_TABLE)?;
    seed_meta(&tx, now_ms)?;
    tx.execute_batch("PRAGMA user_version = 1")?;
    tx.commit()
}

/// Apply the v1 -> v2 step atomically: add the nullable `strategy` column
/// (the per-request auto-cache decision token), bump `PRAGMA
/// user_version` to 2, and update the human-readable `meta.schema_version`
/// row. All in one transaction so a crash mid-step rolls back rather than
/// landing a column-without-version state. Existing rows survive with
/// `strategy` NULL.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current
/// schema, which already carries `strategy`. The loop still enters this
/// arm (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so guard the
/// `ADD COLUMN` against a pre-existing column to keep the fresh path safe.
fn migrate_v1_to_v2(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "strategy")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN strategy TEXT")?;
    }
    tx.execute_batch("PRAGMA user_version = 2")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "2"],
    )?;
    tx.commit()
}

/// Apply the v2 -> v3 step atomically: add the nullable
/// `reduction_strategy` column (the per-request context-reduction decision
/// token), bump `PRAGMA user_version` to 3, and update the human-readable
/// `meta.schema_version` row. All in one transaction so a crash mid-step
/// rolls back rather than landing a column-without-version state. Existing
/// rows survive with `reduction_strategy` NULL.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current
/// schema, which already carries `reduction_strategy`. The loop still
/// enters this arm (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so
/// guard the `ADD COLUMN` against a pre-existing column to keep the fresh
/// path safe.
fn migrate_v2_to_v3(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "reduction_strategy")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN reduction_strategy TEXT")?;
    }
    tx.execute_batch("PRAGMA user_version = 3")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "3"],
    )?;
    tx.commit()
}

/// Apply the v3 -> v4 step atomically: add the nullable
/// `selection_decision` column (the per-request seat-selection decision
/// token), bump `PRAGMA user_version` to 4, and update the human-readable
/// `meta.schema_version` row. All in one transaction so a crash mid-step
/// rolls back rather than landing a column-without-version state. Existing
/// rows survive with `selection_decision` NULL.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current
/// schema, which already carries `selection_decision`. The loop still
/// enters this arm (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so
/// guard the `ADD COLUMN` against a pre-existing column to keep the fresh
/// path safe.
fn migrate_v3_to_v4(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "selection_decision")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN selection_decision TEXT")?;
    }
    tx.execute_batch("PRAGMA user_version = 4")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "4"],
    )?;
    tx.commit()
}

/// Apply the v4 -> v5 step atomically: add the two nullable steady-state
/// would-trim advisory columns (`would_trim_tokens`, the candidate freed-token
/// count `d`, and `would_trim_break_even_k`, the break-even reuse count K*),
/// bump `PRAGMA user_version` to 5, and update the human-readable
/// `meta.schema_version` row. All in one transaction so a crash mid-step rolls
/// back rather than landing a column-without-version state. Existing rows
/// survive with both new columns NULL.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current schema,
/// which already carries both columns. The loop still enters this arm
/// (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so guard each
/// `ADD COLUMN` against a pre-existing column to keep the fresh path safe.
fn migrate_v4_to_v5(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "would_trim_tokens")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_tokens INTEGER")?;
    }
    if !column_exists(&tx, "requests", "would_trim_break_even_k")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_break_even_k REAL")?;
    }
    tx.execute_batch("PRAGMA user_version = 5")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "5"],
    )?;
    tx.commit()
}

/// Apply the v5 -> v6 step atomically: add the nullable `would_trim_k_floor`
/// column (the per-session K estimator's lower confidence bound, recorded only
/// for a `Calibrated` estimate), bump `PRAGMA user_version` to 6, and update
/// the human-readable `meta.schema_version` row. All in one transaction so a
/// crash mid-step rolls back rather than landing a column-without-version
/// state. Existing rows survive with the new column NULL.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current schema,
/// which already carries the column. The loop still enters this arm
/// (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so guard the
/// `ADD COLUMN` against a pre-existing column to keep the fresh path safe.
fn migrate_v5_to_v6(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "would_trim_k_floor")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_k_floor REAL")?;
    }
    tx.execute_batch("PRAGMA user_version = 6")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "6"],
    )?;
    tx.commit()
}

/// Apply the v6 -> v7 step atomically: add the nullable
/// `would_trim_shadow_misfire` column (the shadow misfire monitor advisory:
/// 0 = Stable, 1 = Misfire, NULL = FirstSeen or no session key), bump
/// `PRAGMA user_version` to 7, and update the human-readable
/// `meta.schema_version` row. All in one transaction so a crash mid-step
/// rolls back rather than landing a column-without-version state. Existing
/// rows survive with the new column NULL.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current schema,
/// which already carries the column. The loop still enters this arm
/// (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so guard the
/// `ADD COLUMN` against a pre-existing column to keep the fresh path safe.
fn migrate_v6_to_v7(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "would_trim_shadow_misfire")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_shadow_misfire INTEGER")?;
    }
    tx.execute_batch("PRAGMA user_version = 7")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "7"],
    )?;
    tx.commit()
}

/// Apply the v7 -> v8 step atomically: add the near-lossless attribution
/// columns (`would_trim_dedup_tokens`, `would_trim_supersession_tokens`, the
/// path-extractability count-pair `would_trim_path_units` /
/// `would_trim_path_extractable`, the recorder-version marker
/// `would_trim_recorder_version`, the capped raw-marks blob
/// `would_trim_raw_marks`, and `would_trim_context_fraction`), bump `PRAGMA
/// user_version` to 8, and update the human-readable `meta.schema_version`
/// row. All in one transaction so a crash mid-step rolls back rather than
/// landing a column-without-version state. Existing rows survive with every
/// new column NULL. This step only plumbs the columns -- the near-lossless
/// recorder pass computes and stamps their values.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current schema,
/// which already carries every column. The loop still enters this arm
/// (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so guard each
/// `ADD COLUMN` against a pre-existing column to keep the fresh path safe.
fn migrate_v7_to_v8(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "would_trim_dedup_tokens")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_dedup_tokens INTEGER")?;
    }
    if !column_exists(&tx, "requests", "would_trim_supersession_tokens")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_supersession_tokens INTEGER")?;
    }
    if !column_exists(&tx, "requests", "would_trim_path_units")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_path_units INTEGER")?;
    }
    if !column_exists(&tx, "requests", "would_trim_path_extractable")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_path_extractable INTEGER")?;
    }
    if !column_exists(&tx, "requests", "would_trim_recorder_version")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_recorder_version INTEGER")?;
    }
    if !column_exists(&tx, "requests", "would_trim_raw_marks")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_raw_marks TEXT")?;
    }
    if !column_exists(&tx, "requests", "would_trim_context_fraction")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN would_trim_context_fraction REAL")?;
    }
    tx.execute_batch("PRAGMA user_version = 8")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "8"],
    )?;
    tx.commit()
}

/// Apply the v8 -> v9 step atomically: create the `capability_learn_events`
/// table, bump `PRAGMA user_version` to 9, and update the human-readable
/// `meta.schema_version` row. All in one transaction so a crash mid-step
/// rolls back rather than landing a table-without-version state. The DDL is
/// `CREATE TABLE IF NOT EXISTS`, so a re-run is a no-op.
///
/// Unlike the column-adding steps, this creates a WHOLE NEW TABLE. A fresh
/// DB reaches this arm too (v0->v1 stamps user_version=1, and the loop runs
/// every step up to `SCHEMA_VERSION`), so `IF NOT EXISTS` covers both the
/// fresh-create and the migrated-open paths.
fn migrate_v8_to_v9(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(CREATE_CAPABILITY_LEARN_EVENTS_TABLE)?;
    tx.execute_batch("PRAGMA user_version = 9")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "9"],
    )?;
    tx.commit()
}

/// Apply the v9 -> v10 step atomically: truncate `capability_learn_events`,
/// bump `PRAGMA user_version` to 10, and update `meta.schema_version`. All in
/// one transaction so a crash mid-step rolls back.
///
/// The learned-capability keyspace changed in this cycle: openai-compat
/// negatives were previously keyed by the `error.code` TOKEN
/// (`unsupported_parameter`, ...) but are now keyed by the CANONICAL
/// capability the request carried (`/error/param`, e.g. `web_search`). Any
/// pre-change row would replay under a keyspace that no longer exists, so the
/// forward-only truncate invalidates them. Safe because nothing reads this
/// table yet (it is the warm-rebuild landing pad); `DELETE FROM` is
/// idempotent, so a re-run is a no-op.
fn migrate_v9_to_v10(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("DELETE FROM capability_learn_events")?;
    tx.execute_batch("PRAGMA user_version = 10")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "10"],
    )?;
    tx.commit()
}

/// Apply the v10 -> v11 step atomically: rename the `capability_learn_events`
/// column `feature_key` to `capability_key`, bump `PRAGMA user_version` to 11,
/// and update `meta.schema_version`. All in one transaction so a crash
/// mid-step rolls back.
///
/// This closes a persisted-vocabulary split: the event tracing, the router
/// carrier, and the writer struct all name this field `capability_key`, but
/// the column lagged at `feature_key`. The v9 -> v10 truncate already emptied
/// the table (the warm-rebuild landing pad nothing reads yet), so the rename
/// moves no data. Guarded so it is idempotent AND fresh-DB-safe: a fresh DB
/// created the table from the current DDL (already `capability_key`), so the
/// rename is skipped and only the version bumps.
fn migrate_v10_to_v11(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if column_exists(&tx, "capability_learn_events", "feature_key")?
        && !column_exists(&tx, "capability_learn_events", "capability_key")?
    {
        tx.execute_batch(
            "ALTER TABLE capability_learn_events RENAME COLUMN feature_key TO capability_key",
        )?;
    }
    tx.execute_batch("PRAGMA user_version = 11")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "11"],
    )?;
    tx.commit()
}

/// Apply the v11 -> v12 step atomically: add the nullable `resolved_class`
/// column (the canonical kebab failure-class token for a dispatch-reached
/// failure, stamped by the CLI capture), bump `PRAGMA user_version` to 12,
/// and update the human-readable `meta.schema_version` row. All in one
/// transaction so a crash mid-step rolls back rather than landing a
/// column-without-version state. Existing rows survive with `resolved_class`
/// NULL -- no backfill.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current schema,
/// which already carries the column. The loop still enters this arm
/// (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so guard the
/// `ADD COLUMN` against a pre-existing column to keep the fresh path safe.
fn migrate_v11_to_v12(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "resolved_class")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN resolved_class TEXT")?;
    }
    tx.execute_batch("PRAGMA user_version = 12")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "12"],
    )?;
    tx.commit()
}

/// Apply the v12 -> v13 step atomically: create the `capability_events`
/// table + its `ts` index, bump `PRAGMA user_version` to 13, and update
/// the human-readable `meta.schema_version` row. All in one transaction so
/// a crash mid-step rolls back rather than landing a table-without-version
/// state. The DDL is `CREATE TABLE / INDEX IF NOT EXISTS`, so a re-run is a
/// no-op.
///
/// Like the v8 -> v9 step this creates a WHOLE NEW TABLE (the unified
/// capability-event ledger the warm rebuild replays). A fresh DB reaches
/// this arm too (v0->v1 stamps user_version=1, and the loop runs every step
/// up to `SCHEMA_VERSION`), so `IF NOT EXISTS` covers both the fresh-create
/// and the migrated-open paths.
fn migrate_v12_to_v13(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(CREATE_CAPABILITY_EVENTS_TABLE)?;
    tx.execute_batch(CREATE_CAPABILITY_EVENTS_TS_INDEX)?;
    tx.execute_batch("PRAGMA user_version = 13")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "13"],
    )?;
    tx.commit()
}

/// Apply the v13 -> v14 step atomically: add the two nullable
/// token-estimate calibration columns (`calib_estimated_tokens`, routectl's
/// own byte-heuristic estimate of the dispatched payload, and
/// `calib_prompt_tokens`, the upstream's cache-INCLUSIVE prompt total), bump
/// `PRAGMA user_version` to 14, and update the human-readable
/// `meta.schema_version` row. All in one transaction so a crash mid-step
/// rolls back rather than landing a column-without-version state. Existing
/// rows survive with both new columns NULL -- no backfill.
///
/// On a FRESH DB the v0 -> v1 step created `requests` from the current schema,
/// which already carries both columns. The loop still enters this arm
/// (v0->v1 stamps user_version=1, not SCHEMA_VERSION), so guard each
/// `ADD COLUMN` against a pre-existing column to keep the fresh path safe.
fn migrate_v13_to_v14(conn: &Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    if !column_exists(&tx, "requests", "calib_estimated_tokens")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN calib_estimated_tokens INTEGER")?;
    }
    if !column_exists(&tx, "requests", "calib_prompt_tokens")? {
        tx.execute_batch("ALTER TABLE requests ADD COLUMN calib_prompt_tokens INTEGER")?;
    }
    tx.execute_batch("PRAGMA user_version = 14")?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![META_SCHEMA_VERSION, "14"],
    )?;
    tx.commit()
}

/// True if `table` already has a column named `column`. Used so the
/// v1 -> v2 `ADD COLUMN` is safe on a fresh DB (whose `requests` was
/// created from the current schema and already carries the column).
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?;
    let present = stmt.exists(rusqlite::params![table, column])?;
    Ok(present)
}

/// Insert the creation timestamp and schema version into `meta`. Uses
/// `INSERT OR IGNORE` so a re-run never overwrites the original
/// creation time. Seeds `schema_version` to the v1 literal (the version
/// this step lands); later migration steps update it in lockstep with
/// their own `PRAGMA user_version` bump.
fn seed_meta(conn: &Connection, now_ms: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![META_CREATED_AT_MS, now_ms.to_string()],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![META_SCHEMA_VERSION, "1"],
    )?;
    Ok(())
}

/// Advance the DB to `SCHEMA_VERSION`, applying each forward step in
/// turn. Idempotent: an already-current DB performs no DDL and returns
/// the unchanged version. Each step is responsible for bumping
/// `PRAGMA user_version` atomically with its own DDL, so this loop does
/// not set the version itself. `now_ms` seeds the `meta` creation
/// timestamp when the v0 -> v1 step runs.
pub fn migrate_to_current(conn: &Connection, now_ms: i64) -> Result<i64, MigrateError> {
    let mut version = read_user_version(conn)?;

    if version > SCHEMA_VERSION {
        return Err(MigrateError::VersionTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }

    while version < SCHEMA_VERSION {
        match version {
            0 => migrate_v0_to_v1(conn, now_ms)?,
            1 => migrate_v1_to_v2(conn)?,
            2 => migrate_v2_to_v3(conn)?,
            3 => migrate_v3_to_v4(conn)?,
            4 => migrate_v4_to_v5(conn)?,
            5 => migrate_v5_to_v6(conn)?,
            6 => migrate_v6_to_v7(conn)?,
            7 => migrate_v7_to_v8(conn)?,
            8 => migrate_v8_to_v9(conn)?,
            9 => migrate_v9_to_v10(conn)?,
            10 => migrate_v10_to_v11(conn)?,
            11 => migrate_v11_to_v12(conn)?,
            12 => migrate_v12_to_v13(conn)?,
            13 => migrate_v13_to_v14(conn)?,
            other => unreachable!("no migration step from version {other}"),
        }
        version = read_user_version(conn)?;
        tracing::info!(target: "routectl_usage::migrate", to_version = version, "applied usage schema migration");
    }

    Ok(version)
}
