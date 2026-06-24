//! PRAGMA user_version migrate-on-open ladder.
//!
//! On open, read `PRAGMA user_version` and apply forward migrations one
//! step at a time until the DB reaches `SCHEMA_VERSION`. Each step is
//! idempotent (DDL uses `IF NOT EXISTS`), so opening an already-current
//! DB is a no-op. Add a new step by extending the ladder and bumping
//! `SCHEMA_VERSION` in `schema.rs`.

use rusqlite::Connection;

use crate::schema::{
    CREATE_META_TABLE, CREATE_REQUESTS_TABLE, CREATE_TS_START_INDEX, META_CREATED_AT_MS,
    META_SCHEMA_VERSION, SCHEMA_VERSION,
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
    VersionTooNew { found: i64, supported: i64 },
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
            other => unreachable!("no migration step from version {other}"),
        }
        version = read_user_version(conn)?;
        tracing::info!(target: "routectl_usage::migrate", to_version = version, "applied usage schema migration");
    }

    Ok(version)
}
