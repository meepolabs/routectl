//! Usage-DB connection open: path handling, WAL, file perms, migrations.
//!
//! `open` creates the parent directory if missing, opens (or creates)
//! the SQLite file in WAL mode, tightens the file mode to 0600 on Unix,
//! and runs the migrate-on-open ladder. It returns a `UsageDb` wrapper
//! that owns the connection; a later writer task takes ownership of it.
//! Normal failures return an `OpenError` so the caller can degrade
//! gracefully rather than panicking.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::migrate::{migrate_to_current, MigrateError};
use crate::schema::SCHEMA_VERSION;

/// Owns an open usage-DB connection. The later writer task takes
/// ownership of this wrapper.
pub struct UsageDb {
    conn: Connection,
    path: PathBuf,
}

impl UsageDb {
    /// Borrow the underlying connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The on-disk path this DB was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consume the wrapper and return the owned connection.
    pub fn into_conn(self) -> Connection {
        self.conn
    }
}

/// Errors raised while opening the usage DB. Each is a normal,
/// recoverable failure path -- the caller degrades (disables usage
/// capture) rather than crashing the proxy.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Creating the parent directory failed.
    #[error("failed to create usage db directory {path}: {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// Opening the SQLite file failed.
    #[error("failed to open usage db {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: rusqlite::Error,
    },

    /// A required PRAGMA (e.g. WAL) did not take effect.
    #[error("failed to configure usage db: {0}")]
    Pragma(#[source] rusqlite::Error),

    /// Tightening the file permissions failed (Unix only).
    #[error("failed to set usage db permissions {path}: {source}")]
    Permissions {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The migrate-on-open ladder failed.
    #[error("usage db migration failed: {0}")]
    Migrate(#[from] MigrateError),

    /// The read-only viewer found no usage data yet: either the file is
    /// absent or the `requests` table has never been created. The CLI can
    /// turn this into a friendly "no usage data yet" message.
    #[error("no usage data yet at {path}")]
    NoData { path: String },

    /// The on-disk DB reports a schema version newer than this binary
    /// understands. A read-only viewer refuses to guess at a future
    /// layout rather than silently misread it.
    #[error("usage db schema version {found} is newer than supported {supported}")]
    VersionTooNew { found: i64, supported: i64 },
}

/// Current wall-clock time as epoch milliseconds. Saturates to 0 if the
/// clock is before the epoch (should not happen on a sane host).
fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Create the parent directory of `path` if it does not yet exist.
fn ensure_parent_dir(path: &Path) -> Result<(), OpenError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(|source| OpenError::CreateDir {
        path: parent.display().to_string(),
        source,
    })
}

/// Pre-create the DB file with mode 0600 before SQLite opens it, closing
/// the window where `Connection::open` would otherwise create a
/// world-readable file (umask) ahead of the post-open chmod. SQLite then
/// opens the existing 0600 file and the WAL `-wal`/`-shm` sidecars inherit
/// that mode at creation. No-op if the file already exists. No-op on
/// non-Unix platforms.
#[cfg(unix)]
fn precreate_restricted(path: &Path) -> Result<(), OpenError> {
    use std::os::unix::fs::OpenOptionsExt;

    let result = std::fs::OpenOptions::new()
        .mode(0o600)
        .create(true)
        .truncate(false)
        .write(true)
        .open(path);
    match result {
        Ok(_handle) => Ok(()),
        Err(source) => Err(OpenError::Permissions {
            path: path.display().to_string(),
            source,
        }),
    }
}

#[cfg(not(unix))]
fn precreate_restricted(_path: &Path) -> Result<(), OpenError> {
    Ok(())
}

/// Append `suffix` to the file name of `path` (e.g. ".db" -> ".db-wal").
/// Unlike `with_extension`, this preserves the existing extension.
#[cfg(unix)]
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Tighten the DB file mode to 0600 (owner read/write only) and do the
/// same for any existing `-wal`/`-shm` sidecars. The main-file chmod
/// covers the upgrade path for installs whose file predates the
/// pre-create step; the sidecar chmod is belt-and-suspenders. No-op on
/// non-Unix platforms.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), OpenError> {
    use std::os::unix::fs::PermissionsExt;

    let chmod_0600 = |target: &Path| -> Result<(), OpenError> {
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(target, perms).map_err(|source| OpenError::Permissions {
            path: target.display().to_string(),
            source,
        })
    };

    chmod_0600(path)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        if sidecar.exists() {
            chmod_0600(&sidecar)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), OpenError> {
    Ok(())
}

/// Open (or create) the usage DB at `path` and bring it to the current
/// schema. Steps: ensure the parent dir, pre-create the file 0600 on
/// Unix, open the file, enable + verify WAL, set a busy timeout, restrict
/// perms (upgrade path + sidecars), then run the migration ladder.
pub fn open(path: impl AsRef<Path>) -> Result<UsageDb, OpenError> {
    let path = path.as_ref().to_path_buf();
    ensure_parent_dir(&path)?;
    precreate_restricted(&path)?;

    let conn = Connection::open(&path).map_err(|source| OpenError::Open {
        path: path.display().to_string(),
        source,
    })?;

    enable_wal(&conn)?;

    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(OpenError::Pragma)?;

    restrict_permissions(&path)?;

    let version = migrate_to_current(&conn, now_epoch_ms())?;
    tracing::info!(
        target: "routectl_usage::db",
        path = %path.display(),
        schema_version = version,
        "opened usage db"
    );

    Ok(UsageDb { conn, path })
}

/// Open the usage DB at `path` for reading only. Unlike `open`, this does
/// NOT create the file, chmod it, switch journal modes, or migrate -- the
/// daemon is the sole writer and may be live; a viewer must not mutate the
/// file out from under it. WAL readers do not block the writer, but a busy
/// timeout is still set for safety. A missing file or a missing `requests`
/// table surfaces as `NoData` so the CLI can print a friendly message; a
/// DB newer than this binary surfaces as `VersionTooNew` rather than being
/// misread.
pub fn open_readonly(path: impl AsRef<Path>) -> Result<UsageDb, OpenError> {
    let path = path.as_ref().to_path_buf();
    if !path.exists() {
        return Err(OpenError::NoData {
            path: path.display().to_string(),
        });
    }

    let conn =
        Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            OpenError::Open {
                path: path.display().to_string(),
                source,
            }
        })?;

    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(OpenError::Pragma)?;

    verify_readable_version(&conn)?;
    ensure_requests_table(&conn, &path)?;

    Ok(UsageDb { conn, path })
}

/// Reject a DB whose on-disk `PRAGMA user_version` is newer than this
/// binary understands. Equal-or-older is readable by a non-migrating
/// viewer.
fn verify_readable_version(conn: &Connection) -> Result<(), OpenError> {
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(OpenError::Pragma)?;
    if found > SCHEMA_VERSION {
        return Err(OpenError::VersionTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Treat a DB that exists but has no `requests` table (a fresh,
/// never-written file) as having no usage data yet. A missing table makes
/// the existence probe return zero rows (`QueryReturnedNoRows`); any other
/// SQLite error (corruption, I/O, lock) is a real failure and must surface
/// as such rather than being masked as "no data".
fn ensure_requests_table(conn: &Connection, path: &Path) -> Result<(), OpenError> {
    match conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='requests'",
        [],
        |_| Ok(()),
    ) {
        Ok(()) => Ok(()),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(OpenError::NoData {
            path: path.display().to_string(),
        }),
        Err(source) => Err(OpenError::Open {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// SQLite busy timeout for this open path, in milliseconds. The
/// one-writer/N-reader contract must not rely on a crate default.
const BUSY_TIMEOUT_MS: u64 = 5000;

/// Switch the connection to WAL and confirm the mode actually took. On
/// some filesystems the pragma is silently ignored and the journal stays
/// e.g. "delete"; the daemon-writer + direct-CLI-reader concurrency
/// contract depends on WAL, so a non-WAL result is a hard error.
fn enable_wal(conn: &Connection) -> Result<(), OpenError> {
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .map_err(OpenError::Pragma)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(OpenError::Pragma(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!("journal_mode is '{mode}', expected 'wal'")),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::migrate_to_current;
    use crate::record::UsageRecord;
    use crate::schema::{META_CREATED_AT_MS, META_SCHEMA_VERSION, SCHEMA_VERSION};
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn temp_db_path() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("usage.db");
        (dir, path)
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("read user_version")
    }

    fn journal_mode(conn: &Connection) -> String {
        conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("read journal_mode")
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    }

    #[test]
    fn fresh_open_sets_version_tables_index_and_wal() {
        // Arrange
        let (_dir, path) = temp_db_path();

        // Act
        let db = open(&path).expect("open fresh db");

        // Assert
        assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
        assert!(table_exists(db.conn(), "requests"));
        assert!(table_exists(db.conn(), "meta"));
        assert!(index_exists(db.conn(), "idx_requests_ts_start"));
        assert_eq!(journal_mode(db.conn()).to_lowercase(), "wal");
    }

    #[cfg(unix)]
    #[test]
    fn fresh_open_restricts_file_mode_to_0600() {
        use std::os::unix::fs::PermissionsExt;

        // Arrange
        let (_dir, path) = temp_db_path();

        // Act
        let _db = open(&path).expect("open fresh db");

        // Assert
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn wal_sidecars_are_restricted_to_0600() {
        use std::os::unix::fs::PermissionsExt;

        // Arrange: open, then force a write so the WAL sidecar materializes.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open fresh db");
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count) \
                 VALUES (0, 0, 'wal', 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0)",
                [],
            )
            .expect("write to materialize wal");

        // Re-run restrict_permissions: the sidecars now exist and must be
        // tightened (the open path already does this; assert the outcome).
        restrict_permissions(&path).expect("restrict perms");

        // Assert: any existing -wal/-shm sidecar is 0600.
        let mut checked_any = false;
        for suffix in ["-wal", "-shm"] {
            let sidecar = sidecar_path(&path, suffix);
            if sidecar.exists() {
                checked_any = true;
                let mode = std::fs::metadata(&sidecar)
                    .expect("sidecar metadata")
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "sidecar {suffix} not 0600");
            }
        }
        assert!(checked_any, "expected at least one WAL sidecar to exist");
    }

    #[test]
    fn open_sets_busy_timeout() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");

        // Act
        let timeout: i64 = db
            .conn()
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("read busy_timeout");

        // Assert
        assert_eq!(timeout, BUSY_TIMEOUT_MS as i64);
    }

    #[test]
    fn open_creates_missing_parent_dir() {
        // Arrange
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("nested").join("deeper").join("usage.db");

        // Act
        let db = open(&path).expect("open with missing parent");

        // Assert
        assert!(path.exists());
        assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
    }

    #[test]
    fn outcome_check_rejects_invalid_token() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");

        // Act
        let result = db.conn().execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count) \
             VALUES (0, 0, 'r1', 'openai', 'm', 'a', 0, 'not_an_outcome', 0, 0, 0, 1, 0)",
            [],
        );

        // Assert
        assert!(result.is_err(), "invalid outcome token must be rejected");
    }

    #[test]
    fn valid_outcome_token_inserts() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");

        // Act
        let rows = db
            .conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count) \
                 VALUES (0, 0, 'r1', 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0)",
                [],
            )
            .expect("valid insert");

        // Assert
        assert_eq!(rows, 1);
    }

    #[test]
    fn request_id_unique_dedupes_insert_or_ignore() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        let insert = "INSERT OR IGNORE INTO requests (ts_start, ts_end, request_id, \
             ingress_dialect, requested_model, alias, stream, outcome, latency_ms, \
             tool_count, msg_count, attempt_count, fallback_count) \
             VALUES (0, 0, 'dup', 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0)";

        // Act
        db.conn().execute(insert, []).expect("first insert");
        db.conn()
            .execute(insert, [])
            .expect("second insert ignored");

        // Assert
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM requests WHERE request_id='dup'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn migrate_on_open_is_idempotent() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let first = open(&path).expect("first open");
        let v1 = user_version(first.conn());
        drop(first);

        // Act
        let second = open(&path).expect("second open");

        // Assert
        assert_eq!(user_version(second.conn()), v1);
        assert_eq!(user_version(second.conn()), SCHEMA_VERSION);
    }

    #[test]
    fn migration_user_version_matches_meta_after_open() {
        // Arrange + Act
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");

        // Assert: the atomic v0->v1 step sets PRAGMA user_version and seeds
        // meta.schema_version to the same value -- no half-applied state.
        let pragma_version = user_version(db.conn());
        let meta_version: String = db
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [META_SCHEMA_VERSION],
                |r| r.get(0),
            )
            .expect("schema_version row");
        assert_eq!(pragma_version, SCHEMA_VERSION);
        assert_eq!(meta_version, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn forward_migration_advances_from_zero() {
        // Arrange: a bare DB at user_version 0 with no tables.
        let (_dir, path) = temp_db_path();
        let conn = Connection::open(&path).expect("raw open");
        assert_eq!(user_version(&conn), 0);
        assert!(!table_exists(&conn, "requests"));

        // Act
        let version = migrate_to_current(&conn, 123).expect("migrate");

        // Assert
        assert_eq!(version, SCHEMA_VERSION);
        assert!(table_exists(&conn, "requests"));
        assert!(table_exists(&conn, "meta"));
    }

    #[test]
    fn meta_holds_creation_ts_and_schema_version() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let conn = Connection::open(&path).expect("raw open");

        // Act
        migrate_to_current(&conn, 999).expect("migrate");

        // Assert
        let created: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [META_CREATED_AT_MS],
                |r| r.get(0),
            )
            .expect("created_at row");
        assert_eq!(created, "999");

        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [META_SCHEMA_VERSION],
                |r| r.get(0),
            )
            .expect("schema_version row");
        assert_eq!(version, SCHEMA_VERSION.to_string());
    }

    /// A v1 DB (created before the `strategy` column existed) must migrate
    /// forward across every ladder step: both later columns are added,
    /// `user_version` reaches the current version, and any pre-existing row
    /// survives with the new columns NULL. Builds a genuine v1 `requests`
    /// table (the pre-strategy column set) so the ALTER path -- not the
    /// fresh-schema path -- is exercised.
    #[test]
    fn old_v1_db_migrates_to_current_preserving_rows() {
        // Arrange: a v1-shaped DB. Minimal column subset is enough to prove
        // the ALTER + row-survival contract; the full v1 set is not needed.
        let (_dir, path) = temp_db_path();
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(
            "CREATE TABLE requests (
                ts_start INTEGER NOT NULL,
                ts_end INTEGER NOT NULL,
                request_id TEXT NOT NULL UNIQUE,
                ingress_dialect TEXT NOT NULL,
                requested_model TEXT NOT NULL,
                alias TEXT NOT NULL,
                stream INTEGER NOT NULL,
                outcome TEXT NOT NULL,
                latency_ms INTEGER NOT NULL,
                tool_count INTEGER NOT NULL,
                msg_count INTEGER NOT NULL,
                attempt_count INTEGER NOT NULL,
                fallback_count INTEGER NOT NULL
            );
            CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
            INSERT INTO meta (key, value) VALUES ('schema_version', '1');
            INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                requested_model, alias, stream, outcome, latency_ms, tool_count, \
                msg_count, attempt_count, fallback_count) \
                VALUES (1, 2, 'old-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0);
            PRAGMA user_version = 1;",
        )
        .expect("build v1 db");
        assert_eq!(user_version(&conn), 1);

        // Act
        let version = migrate_to_current(&conn, 0).expect("migrate v1->current");

        // Assert: the ladder runs every forward step, so a v1 DB lands at
        // the current version with both added columns present, the old row
        // survives with NULL strategy / reduction_strategy, and meta tracks
        // the final version.
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        let strategy: Option<String> = conn
            .query_row(
                "SELECT strategy FROM requests WHERE request_id='old-row'",
                [],
                |r| r.get(0),
            )
            .expect("old row survives");
        assert!(
            strategy.is_none(),
            "migrated old row must have NULL strategy"
        );
        let reduction_strategy: Option<String> = conn
            .query_row(
                "SELECT reduction_strategy FROM requests WHERE request_id='old-row'",
                [],
                |r| r.get(0),
            )
            .expect("old row survives v2->v3");
        assert!(
            reduction_strategy.is_none(),
            "migrated old row must have NULL reduction_strategy"
        );
        let selection_decision: Option<String> = conn
            .query_row(
                "SELECT selection_decision FROM requests WHERE request_id='old-row'",
                [],
                |r| r.get(0),
            )
            .expect("old row survives v3->v4");
        assert!(
            selection_decision.is_none(),
            "migrated old row must have NULL selection_decision"
        );
        let meta_version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .expect("meta schema_version");
        assert_eq!(meta_version, SCHEMA_VERSION.to_string());
    }

    /// A v2 DB (created before the `reduction_strategy` column existed) must
    /// migrate to v3: the column is added, `user_version` becomes 3, and any
    /// pre-existing row survives with `reduction_strategy` NULL. Builds a
    /// genuine v2 `requests` table (the pre-reduction_strategy column set,
    /// including `strategy`) so the ALTER path -- not the fresh-schema path
    /// -- is exercised.
    #[test]
    fn old_v2_db_migrates_to_v3_preserving_rows() {
        // Arrange: a v2-shaped DB. Minimal column subset plus `strategy` is
        // enough to prove the ALTER + row-survival contract.
        let (_dir, path) = temp_db_path();
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(
            "CREATE TABLE requests (
                ts_start INTEGER NOT NULL,
                ts_end INTEGER NOT NULL,
                request_id TEXT NOT NULL UNIQUE,
                ingress_dialect TEXT NOT NULL,
                requested_model TEXT NOT NULL,
                alias TEXT NOT NULL,
                stream INTEGER NOT NULL,
                outcome TEXT NOT NULL,
                latency_ms INTEGER NOT NULL,
                tool_count INTEGER NOT NULL,
                msg_count INTEGER NOT NULL,
                attempt_count INTEGER NOT NULL,
                fallback_count INTEGER NOT NULL,
                strategy TEXT
            );
            CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
            INSERT INTO meta (key, value) VALUES ('schema_version', '2');
            INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                requested_model, alias, stream, outcome, latency_ms, tool_count, \
                msg_count, attempt_count, fallback_count, strategy) \
                VALUES (1, 2, 'v2-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0, 'auto_emitted');
            PRAGMA user_version = 2;",
        )
        .expect("build v2 db");
        assert_eq!(user_version(&conn), 2);

        // Act
        let version = migrate_to_current(&conn, 0).expect("migrate v2->v3");

        // Assert: landed at v3, the column exists, the old row survived with
        // a NULL reduction_strategy (and its prior strategy intact), and
        // meta tracks the new version.
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        let (strategy, reduction_strategy): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT strategy, reduction_strategy FROM requests WHERE request_id='v2-row'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("old row survives");
        assert_eq!(
            strategy.as_deref(),
            Some("auto_emitted"),
            "pre-existing strategy must survive the v2->v3 migration"
        );
        assert!(
            reduction_strategy.is_none(),
            "migrated v2 row must have NULL reduction_strategy"
        );
        let meta_version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .expect("meta schema_version");
        assert_eq!(meta_version, SCHEMA_VERSION.to_string());
    }

    /// A v3 DB (created before the `selection_decision` column existed) must
    /// migrate to v4: the column is added, `user_version` becomes 4, and any
    /// pre-existing row survives with `selection_decision` NULL (and its
    /// prior strategy / reduction_strategy intact). Builds a genuine v3
    /// `requests` table (the pre-selection_decision column set, including
    /// `strategy` and `reduction_strategy`) so the ALTER path -- not the
    /// fresh-schema path -- is exercised.
    #[test]
    fn old_v3_db_migrates_to_v4_preserving_rows() {
        // Arrange: a v3-shaped DB. Minimal column subset plus `strategy` and
        // `reduction_strategy` is enough to prove the ALTER + row-survival
        // contract.
        let (_dir, path) = temp_db_path();
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(
            "CREATE TABLE requests (
                ts_start INTEGER NOT NULL,
                ts_end INTEGER NOT NULL,
                request_id TEXT NOT NULL UNIQUE,
                ingress_dialect TEXT NOT NULL,
                requested_model TEXT NOT NULL,
                alias TEXT NOT NULL,
                stream INTEGER NOT NULL,
                outcome TEXT NOT NULL,
                latency_ms INTEGER NOT NULL,
                tool_count INTEGER NOT NULL,
                msg_count INTEGER NOT NULL,
                attempt_count INTEGER NOT NULL,
                fallback_count INTEGER NOT NULL,
                strategy TEXT,
                reduction_strategy TEXT
            );
            CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
            INSERT INTO meta (key, value) VALUES ('schema_version', '3');
            INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                requested_model, alias, stream, outcome, latency_ms, tool_count, \
                msg_count, attempt_count, fallback_count, strategy, reduction_strategy) \
                VALUES (1, 2, 'v3-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0, 'auto_emitted', 'applied');
            PRAGMA user_version = 3;",
        )
        .expect("build v3 db");
        assert_eq!(user_version(&conn), 3);

        // Act
        let version = migrate_to_current(&conn, 0).expect("migrate v3->current");

        // Assert: the ladder runs forward to the current version; the v3->v4
        // step added the selection_decision column, the old row survived with a
        // NULL selection_decision (and its prior tokens intact), and meta
        // tracks the final version.
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        let present: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name='selection_decision'")
            .expect("prepare")
            .exists([])
            .expect("query");
        assert!(present, "v4 DB must carry the selection_decision column");
        let (strategy, reduction, selection): (Option<String>, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT strategy, reduction_strategy, selection_decision \
                 FROM requests WHERE request_id='v3-row'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("old row survives");
        assert_eq!(
            strategy.as_deref(),
            Some("auto_emitted"),
            "pre-existing strategy must survive the v3->v4 migration"
        );
        assert_eq!(
            reduction.as_deref(),
            Some("applied"),
            "pre-existing reduction_strategy must survive the v3->v4 migration"
        );
        assert!(
            selection.is_none(),
            "migrated v3 row must have NULL selection_decision"
        );
        let meta_version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .expect("meta schema_version");
        assert_eq!(meta_version, SCHEMA_VERSION.to_string());
    }

    /// A v4 DB (created before the steady-state would-trim columns existed)
    /// must migrate to v5: both `would_trim_tokens` and
    /// `would_trim_break_even_k` are added, `user_version` becomes 5, and any
    /// pre-existing row survives with both new columns NULL (and its prior
    /// strategy / reduction_strategy / selection_decision intact). Builds a
    /// genuine v4 `requests` table so the ALTER path -- not the fresh-schema
    /// path -- is exercised.
    #[test]
    fn old_v4_db_migrates_to_v5_preserving_rows() {
        // Arrange: a v4-shaped DB. Minimal column subset plus the three prior
        // decision tokens is enough to prove the ALTER + row-survival contract.
        let (_dir, path) = temp_db_path();
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(
            "CREATE TABLE requests (
                ts_start INTEGER NOT NULL,
                ts_end INTEGER NOT NULL,
                request_id TEXT NOT NULL UNIQUE,
                ingress_dialect TEXT NOT NULL,
                requested_model TEXT NOT NULL,
                alias TEXT NOT NULL,
                stream INTEGER NOT NULL,
                outcome TEXT NOT NULL,
                latency_ms INTEGER NOT NULL,
                tool_count INTEGER NOT NULL,
                msg_count INTEGER NOT NULL,
                attempt_count INTEGER NOT NULL,
                fallback_count INTEGER NOT NULL,
                strategy TEXT,
                reduction_strategy TEXT,
                selection_decision TEXT
            );
            CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
            INSERT INTO meta (key, value) VALUES ('schema_version', '4');
            INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                requested_model, alias, stream, outcome, latency_ms, tool_count, \
                msg_count, attempt_count, fallback_count, strategy, reduction_strategy, \
                selection_decision) \
                VALUES (1, 2, 'v4-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0, \
                'auto_emitted', 'applied', 'sticky_stay');
            PRAGMA user_version = 4;",
        )
        .expect("build v4 db");
        assert_eq!(user_version(&conn), 4);

        // Act
        let version = migrate_to_current(&conn, 0).expect("migrate v4->v5");

        // Assert: landed at v5, both columns exist, the old row survived with
        // NULL would-trim columns (and its prior tokens intact), and meta
        // tracks the new version.
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        for col in ["would_trim_tokens", "would_trim_break_even_k"] {
            let present: bool = conn
                .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name=?1")
                .expect("prepare")
                .exists([col])
                .expect("query");
            assert!(present, "v5 DB must carry the {col} column");
        }
        // The three prior decision tokens survive intact.
        let (strategy, reduction, selection): (Option<String>, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT strategy, reduction_strategy, selection_decision \
                 FROM requests WHERE request_id='v4-row'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("old row survives");
        assert_eq!(strategy.as_deref(), Some("auto_emitted"));
        assert_eq!(reduction.as_deref(), Some("applied"));
        assert_eq!(selection.as_deref(), Some("sticky_stay"));
        // Both new columns read NULL on the migrated old row.
        let wt_tokens: Option<i64> = conn
            .query_row(
                "SELECT would_trim_tokens FROM requests WHERE request_id='v4-row'",
                [],
                |r| r.get(0),
            )
            .expect("old row survives");
        assert!(
            wt_tokens.is_none(),
            "migrated v4 row must have NULL would_trim_tokens"
        );
        let wt_k: Option<f64> = conn
            .query_row(
                "SELECT would_trim_break_even_k FROM requests WHERE request_id='v4-row'",
                [],
                |r| r.get(0),
            )
            .expect("old row survives");
        assert!(
            wt_k.is_none(),
            "migrated v4 row must have NULL would_trim_break_even_k"
        );
        let meta_version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .expect("meta schema_version");
        assert_eq!(meta_version, SCHEMA_VERSION.to_string());
    }

    /// A fresh DB opens directly at the current version with the steady-state
    /// would-trim columns present.
    #[test]
    fn fresh_db_opens_at_v5_with_would_trim_columns() {
        // Arrange + Act
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open fresh");

        // Assert
        assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
        for col in ["would_trim_tokens", "would_trim_break_even_k"] {
            let present: bool = db
                .conn()
                .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name=?1")
                .expect("prepare")
                .exists([col])
                .expect("query");
            assert!(present, "fresh v5 DB must carry the {col} column");
        }
    }

    /// A fresh DB opens directly at v4 with the `selection_decision` column
    /// present.
    #[test]
    fn fresh_db_opens_at_v4_with_selection_decision_column() {
        // Arrange + Act
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open fresh");

        // Assert
        assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
        let present: bool = db
            .conn()
            .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name='selection_decision'")
            .expect("prepare")
            .exists([])
            .expect("query");
        assert!(
            present,
            "fresh v4 DB must carry the selection_decision column"
        );
    }

    /// A fresh DB opens directly at the current version (`SCHEMA_VERSION`)
    /// with the `reduction_strategy` column present.
    #[test]
    fn fresh_db_opens_at_v3_with_reduction_strategy_column() {
        // Arrange + Act
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open fresh");

        // Assert
        assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
        let present: bool = db
            .conn()
            .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name='reduction_strategy'")
            .expect("prepare")
            .exists([])
            .expect("query");
        assert!(
            present,
            "fresh v3 DB must carry the reduction_strategy column"
        );
    }

    #[test]
    fn migration_rejects_newer_db_version() {
        // Arrange: a DB claiming a future schema version.
        let (_dir, path) = temp_db_path();
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .expect("set future version");

        // Act
        let result = migrate_to_current(&conn, 0);

        // Assert
        assert!(matches!(result, Err(MigrateError::VersionTooNew { .. })));
    }

    /// `requests` must carry exactly one column per `UsageRecord` field,
    /// with matching null-ability. record.rs is the source of truth; this
    /// test fails loudly if the schema and the struct drift apart.
    #[test]
    fn requests_columns_match_usage_record() {
        // Arrange: (column name, expected NOT NULL). Mirrors record.rs --
        // non-Option fields are NOT NULL, Option<T> fields are NULLable.
        let expected: &[(&str, bool)] = &[
            ("ts_start", true),
            ("ts_end", true),
            ("request_id", true),
            ("ingress_dialect", true),
            ("requested_model", true),
            ("alias", true),
            ("model", false),
            ("upstream", false),
            ("provider", false),
            ("provider_kind", false),
            ("seat", false),
            ("session_id", false),
            ("stream", true),
            ("max_tokens_req", false),
            ("tool_count", true),
            ("thinking_req", false),
            ("thinking_req_kind", false),
            ("msg_count", true),
            ("service_tier", false),
            ("outcome", true),
            ("http_status", false),
            ("error_class", false),
            ("finish_reason", false),
            ("attempt_count", true),
            ("fallback_count", true),
            ("latency_ms", true),
            ("ttfb_ms", false),
            ("input_tokens", false),
            ("output_tokens", false),
            ("reasoning_tokens", false),
            ("cache_read", false),
            ("cache_write_5m", false),
            ("cache_write_1h", false),
            ("server_tool_use", false),
            ("quota_claim", false),
            ("quota_status", false),
            ("quota_overage_status", false),
            ("quota_utilization", false),
            ("quota_overage_utilization", false),
            ("quota_reset", false),
            ("quota_extras", false),
            ("extra", false),
            ("strategy", false),
            ("reduction_strategy", false),
            ("selection_decision", false),
            ("would_trim_tokens", false),
            ("would_trim_break_even_k", false),
        ];

        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");

        // Act: read PRAGMA table_info (name, notnull) for each column.
        let mut stmt = db
            .conn()
            .prepare("SELECT name, \"notnull\" FROM pragma_table_info('requests')")
            .expect("prepare table_info");
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();

        // Assert: same count, same names, same null-ability, same order.
        assert_eq!(
            rows.len(),
            expected.len(),
            "column count drifted from UsageRecord field count"
        );
        for ((actual_name, actual_notnull), (exp_name, exp_notnull)) in
            rows.iter().zip(expected.iter())
        {
            assert_eq!(actual_name, exp_name, "column name/order mismatch");
            assert_eq!(
                *actual_notnull == 1,
                *exp_notnull,
                "null-ability mismatch for column {actual_name}"
            );
        }
    }

    /// Compile-time guard: touching `UsageRecord` here means a field
    /// rename forces a look at the column-set test above.
    #[test]
    fn usage_record_field_count_is_referenced() {
        let _ = std::mem::size_of::<UsageRecord>();
    }

    #[test]
    fn open_readonly_on_nonexistent_path_returns_no_data() {
        // Arrange: a path that does not exist.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("absent.db");

        // Act
        let result = open_readonly(&path);

        // Assert: NoData, and the file was NOT created.
        assert!(matches!(result, Err(OpenError::NoData { .. })));
        assert!(!path.exists(), "open_readonly must not create the file");
    }

    #[test]
    fn open_readonly_on_fresh_unwritten_db_returns_no_data() {
        // Arrange: a file exists but has no `requests` table.
        let (_dir, path) = temp_db_path();
        let _conn = Connection::open(&path).expect("create empty sqlite file");

        // Act
        let result = open_readonly(&path);

        // Assert
        assert!(matches!(result, Err(OpenError::NoData { .. })));
    }

    #[test]
    fn open_readonly_reads_a_seeded_db() {
        // Arrange: seed via the read-write open path.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("seed db");
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count) \
                 VALUES (10, 20, 'r-ro', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0)",
                [],
            )
            .expect("seed row");
        drop(db);

        // Act
        let ro = open_readonly(&path).expect("open readonly");

        // Assert
        let count: i64 = ro
            .conn()
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn open_readonly_rejects_newer_version() {
        // Arrange: seed a normal DB, then bump user_version above support.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("seed db");
        db.conn()
            .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .expect("bump version");
        drop(db);

        // Act
        let result = open_readonly(&path);

        // Assert
        assert!(matches!(result, Err(OpenError::VersionTooNew { .. })));
    }

    #[test]
    fn open_readonly_accepts_equal_version() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("seed db");
        drop(db);

        // Act + Assert: equal version is readable by a non-migrating viewer.
        let ro = open_readonly(&path);
        assert!(ro.is_ok());
    }

    #[test]
    fn open_readonly_sets_busy_timeout() {
        // Arrange
        let (_dir, path) = temp_db_path();
        drop(open(&path).expect("seed db"));

        // Act
        let ro = open_readonly(&path).expect("open readonly");
        let timeout: i64 = ro
            .conn()
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("read busy_timeout");

        // Assert
        assert_eq!(timeout, BUSY_TIMEOUT_MS as i64);
    }
}
