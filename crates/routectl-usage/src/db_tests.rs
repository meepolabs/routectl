use super::*;
use crate::migrate::migrate_to_current;
use crate::record::UsageRecord;
use crate::schema::{
    CREATE_CAPABILITY_LEARN_EVENTS_TABLE, CREATE_TS_START_INDEX, META_CREATED_AT_MS,
    META_SCHEMA_VERSION, SCHEMA_VERSION,
};
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
    assert!(table_exists(db.conn(), "capability_learn_events"));
    assert!(table_exists(db.conn(), "capability_events"));
    assert!(index_exists(db.conn(), "idx_requests_ts_start"));
    assert!(index_exists(db.conn(), "idx_capability_events_ts"));
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
    let (strategy, reduction, selection): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
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
    let (strategy, reduction, selection): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
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

/// A v5 DB (created before the per-session K-floor column existed) must
/// migrate to v6: `would_trim_k_floor` is added, `user_version` becomes 6,
/// and any pre-existing row survives with the new column NULL (and its
/// prior would-trim columns intact). Builds a genuine v5 `requests` table
/// so the ALTER path -- not the fresh-schema path -- is exercised.
#[test]
fn old_v5_db_migrates_to_v6_preserving_rows() {
    // Arrange: a v5-shaped DB carrying the two would-trim advisory columns.
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
            selection_decision TEXT,
            would_trim_tokens INTEGER,
            would_trim_break_even_k REAL
        );
        CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
        INSERT INTO meta (key, value) VALUES ('schema_version', '5');
        INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
            requested_model, alias, stream, outcome, latency_ms, tool_count, \
            msg_count, attempt_count, fallback_count, strategy, reduction_strategy, \
            selection_decision, would_trim_tokens, would_trim_break_even_k) \
            VALUES (1, 2, 'v5-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0, \
            'auto_emitted', 'cost_gate:would_trim', 'sticky_stay', 40000, 50.0);
        PRAGMA user_version = 5;",
    )
    .expect("build v5 db");
    assert_eq!(user_version(&conn), 5);

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate v5->v6");

    // Assert: landed at v6, the column exists, the old row survived with a
    // NULL k-floor (and its prior would-trim values intact), and meta
    // tracks the new version.
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(user_version(&conn), SCHEMA_VERSION);
    let present: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name=?1")
        .expect("prepare")
        .exists(["would_trim_k_floor"])
        .expect("query");
    assert!(present, "v6 DB must carry the would_trim_k_floor column");
    // The prior would-trim values survive intact.
    let (wt_tokens, wt_k): (Option<i64>, Option<f64>) = conn
        .query_row(
            "SELECT would_trim_tokens, would_trim_break_even_k \
             FROM requests WHERE request_id='v5-row'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("old row survives");
    assert_eq!(wt_tokens, Some(40_000));
    assert_eq!(wt_k, Some(50.0));
    // The new column reads NULL on the migrated old row.
    let k_floor: Option<f64> = conn
        .query_row(
            "SELECT would_trim_k_floor FROM requests WHERE request_id='v5-row'",
            [],
            |r| r.get(0),
        )
        .expect("old row survives");
    assert!(
        k_floor.is_none(),
        "migrated v5 row must have NULL would_trim_k_floor"
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

/// A v7 DB (created before the near-lossless attribution columns
/// existed) must migrate to v8: every new column
/// (`would_trim_dedup_tokens`, `would_trim_supersession_tokens`,
/// `would_trim_path_units`, `would_trim_path_extractable`,
/// `would_trim_recorder_version`, `would_trim_raw_marks`,
/// `would_trim_context_fraction`) is added, `user_version` becomes 8,
/// and any pre-existing row survives with every new column NULL (and
/// its prior would-trim / shadow-misfire columns intact). Builds a
/// genuine v7 `requests` table so the ALTER path -- not the
/// fresh-schema path -- is exercised.
#[test]
fn old_v7_db_migrates_to_v8_preserving_rows() {
    // Arrange: a v7-shaped DB carrying the shadow-misfire column.
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
            selection_decision TEXT,
            would_trim_tokens INTEGER,
            would_trim_break_even_k REAL,
            would_trim_k_floor REAL,
            would_trim_shadow_misfire INTEGER
        );
        CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
        INSERT INTO meta (key, value) VALUES ('schema_version', '7');
        INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
            requested_model, alias, stream, outcome, latency_ms, tool_count, \
            msg_count, attempt_count, fallback_count, strategy, reduction_strategy, \
            selection_decision, would_trim_tokens, would_trim_break_even_k, \
            would_trim_k_floor, would_trim_shadow_misfire) \
            VALUES (1, 2, 'v7-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0, \
            'auto_emitted', 'applied', 'sticky_stay', 40000, 50.0, 60.0, 0);
        PRAGMA user_version = 7;",
    )
    .expect("build v7 db");
    assert_eq!(user_version(&conn), 7);

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate v7->v8");

    // Assert: landed at v8, every new column exists, the old row survived
    // with its prior would-trim / shadow-misfire values intact, and meta
    // tracks the new version.
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(user_version(&conn), SCHEMA_VERSION);
    for col in [
        "would_trim_dedup_tokens",
        "would_trim_supersession_tokens",
        "would_trim_path_units",
        "would_trim_path_extractable",
        "would_trim_recorder_version",
        "would_trim_raw_marks",
        "would_trim_context_fraction",
    ] {
        let present: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name=?1")
            .expect("prepare")
            .exists([col])
            .expect("query");
        assert!(present, "v8 DB must carry the {col} column");
    }
    // The prior would-trim / shadow-misfire values survive intact.
    let (wt_tokens, wt_k, k_floor, shadow_misfire): (
        Option<i64>,
        Option<f64>,
        Option<f64>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT would_trim_tokens, would_trim_break_even_k, would_trim_k_floor, \
             would_trim_shadow_misfire FROM requests WHERE request_id='v7-row'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("old row survives");
    assert_eq!(wt_tokens, Some(40_000));
    assert_eq!(wt_k, Some(50.0));
    assert_eq!(k_floor, Some(60.0));
    assert_eq!(shadow_misfire, Some(0));
    // Every new column reads NULL on the migrated old row.
    type NewAttributionCols = (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<f64>,
    );
    let (
        dedup,
        supersession,
        path_units,
        path_extractable,
        recorder_version,
        raw_marks,
        context_fraction,
    ): NewAttributionCols = conn
        .query_row(
            "SELECT would_trim_dedup_tokens, would_trim_supersession_tokens, \
             would_trim_path_units, would_trim_path_extractable, \
             would_trim_recorder_version, would_trim_raw_marks, \
             would_trim_context_fraction FROM requests WHERE request_id='v7-row'",
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
        .expect("old row survives");
    assert!(
        dedup.is_none(),
        "migrated v7 row must have NULL would_trim_dedup_tokens"
    );
    assert!(
        supersession.is_none(),
        "migrated v7 row must have NULL would_trim_supersession_tokens"
    );
    assert!(
        path_units.is_none(),
        "migrated v7 row must have NULL would_trim_path_units"
    );
    assert!(
        path_extractable.is_none(),
        "migrated v7 row must have NULL would_trim_path_extractable"
    );
    assert!(
        recorder_version.is_none(),
        "migrated v7 row must have NULL would_trim_recorder_version"
    );
    assert!(
        raw_marks.is_none(),
        "migrated v7 row must have NULL would_trim_raw_marks"
    );
    assert!(
        context_fraction.is_none(),
        "migrated v7 row must have NULL would_trim_context_fraction"
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

/// An older v8-shaped DB (no `capability_learn_events` table) migrates
/// to v9: the table is created, `user_version` becomes 9, and any
/// pre-existing `requests` row survives untouched. Builds a genuine v8
/// DB so the create-table migration path -- not the fresh-schema path --
/// is exercised.
#[test]
fn old_v8_db_migrates_to_v9_creating_learn_events_table() {
    // Arrange: a v8-shaped DB with a request row but no learn-events table.
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
        INSERT INTO meta (key, value) VALUES ('schema_version', '8');
        INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
            requested_model, alias, stream, outcome, latency_ms, tool_count, \
            msg_count, attempt_count, fallback_count) \
            VALUES (1, 2, 'v8-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0);
        PRAGMA user_version = 8;",
    )
    .expect("build v8 db");
    assert_eq!(user_version(&conn), 8);
    assert!(!table_exists(&conn, "capability_learn_events"));

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate v8->v9");

    // Assert: landed at v9, the new table exists, the old request row
    // survived, and meta tracks the new version.
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(user_version(&conn), SCHEMA_VERSION);
    assert!(table_exists(&conn, "capability_learn_events"));
    let survivor: String = conn
        .query_row("SELECT request_id FROM requests", [], |r| r.get(0))
        .expect("request row survives");
    assert_eq!(survivor, "v8-row");
    let meta_version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .expect("meta schema_version");
    assert_eq!(meta_version, SCHEMA_VERSION.to_string());
}

/// The v9 -> v10 keyspace invalidation: a v9 DB carrying a pre-change,
/// token-keyed learn row is truncated so it cannot resurface under the new
/// canonical keyspace. Proves a warm-rebuild after the bump surfaces no
/// stale pre-change keys, and that the step is forward-only + idempotent.
#[test]
fn v9_to_v10_truncates_stale_pre_change_learn_rows() {
    // Arrange: a genuine v9 DB with a stale token-keyed learn row (the
    // pre-change openai keyspace keyed on the `error.code` token). The
    // table is created from its HISTORICAL v9 shape (column `feature_key`,
    // renamed to `capability_key` only at v10 -> v11), not the current DDL.
    let (_dir, path) = temp_db_path();
    let conn = Connection::open(&path).expect("raw open");
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
         INSERT INTO meta (key, value) VALUES ('schema_version', '9');",
    )
    .expect("seed meta");
    // A genuine v9 DB also carries a `requests` table; the later ladder
    // steps (v11 -> v12 ALTER) touch it, so a minimal shape must exist.
    conn.execute_batch(
        "CREATE TABLE requests (
             ts_start        INTEGER NOT NULL,
             ts_end          INTEGER NOT NULL,
             request_id      TEXT    NOT NULL UNIQUE,
             ingress_dialect TEXT    NOT NULL,
             requested_model TEXT    NOT NULL,
             alias           TEXT    NOT NULL,
             stream          INTEGER NOT NULL,
             outcome         TEXT    NOT NULL,
             latency_ms      INTEGER NOT NULL,
             tool_count      INTEGER NOT NULL,
             msg_count       INTEGER NOT NULL,
             attempt_count   INTEGER NOT NULL,
             fallback_count  INTEGER NOT NULL
         )",
    )
    .expect("create v9 requests table");
    conn.execute_batch(
        "CREATE TABLE capability_learn_events (
             ts               INTEGER NOT NULL,
             state_key        TEXT    NOT NULL,
             feature_key      TEXT    NOT NULL,
             provider_kind    TEXT    NOT NULL,
             signal_tier      TEXT    NOT NULL,
             observations     INTEGER NOT NULL,
             upstream_status  INTEGER NOT NULL,
             remapped         INTEGER NOT NULL,
             request_features TEXT    NOT NULL
         )",
    )
    .expect("create v9 learn table");
    conn.execute(
        "INSERT INTO capability_learn_events (ts, state_key, feature_key, \
         provider_kind, signal_tier, observations, upstream_status, remapped, \
         request_features) VALUES (1, 'nick', 'unsupported_parameter', \
         'openai-compat', 'self-identifying', 1, 400, 0, '[]')",
        [],
    )
    .expect("seed stale row");
    conn.execute_batch("PRAGMA user_version = 9")
        .expect("stamp v9");
    assert_eq!(user_version(&conn), 9);

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate v9->v10");

    // Assert: landed at v10 and the stale row is gone (the table survives).
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(user_version(&conn), SCHEMA_VERSION);
    assert!(table_exists(&conn, "capability_learn_events"));
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM capability_learn_events", [], |r| {
            r.get(0)
        })
        .expect("count learn rows");
    assert_eq!(
        remaining, 0,
        "pre-change token-keyed row must not resurface"
    );

    // Idempotent: re-running the ladder is a no-op.
    let again = migrate_to_current(&conn, 0).expect("re-run migrate");
    assert_eq!(again, SCHEMA_VERSION);
}

/// The v10 -> v11 column rename: a v10 DB whose `capability_learn_events`
/// table still carries the legacy `feature_key` column is migrated to
/// `capability_key`, closing the persisted-vocabulary split. Proves the
/// column is renamed (not dropped/re-added), the table survives, and the
/// step is idempotent.
#[test]
fn v10_to_v11_renames_feature_key_to_capability_key() {
    // Arrange: a genuine v10 DB with the pre-rename column name.
    let (_dir, path) = temp_db_path();
    let conn = Connection::open(&path).expect("raw open");
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
         INSERT INTO meta (key, value) VALUES ('schema_version', '10');",
    )
    .expect("seed meta");
    // A genuine v10 DB also carries a `requests` table; the later ladder
    // steps (v11 -> v12 ALTER) touch it, so a minimal shape must exist.
    conn.execute_batch(
        "CREATE TABLE requests (
             ts_start        INTEGER NOT NULL,
             ts_end          INTEGER NOT NULL,
             request_id      TEXT    NOT NULL UNIQUE,
             ingress_dialect TEXT    NOT NULL,
             requested_model TEXT    NOT NULL,
             alias           TEXT    NOT NULL,
             stream          INTEGER NOT NULL,
             outcome         TEXT    NOT NULL,
             latency_ms      INTEGER NOT NULL,
             tool_count      INTEGER NOT NULL,
             msg_count       INTEGER NOT NULL,
             attempt_count   INTEGER NOT NULL,
             fallback_count  INTEGER NOT NULL
         )",
    )
    .expect("create v10 requests table");
    conn.execute_batch(
        "CREATE TABLE capability_learn_events (
             ts               INTEGER NOT NULL,
             state_key        TEXT    NOT NULL,
             feature_key      TEXT    NOT NULL,
             provider_kind    TEXT    NOT NULL,
             signal_tier      TEXT    NOT NULL,
             observations     INTEGER NOT NULL,
             upstream_status  INTEGER NOT NULL,
             remapped         INTEGER NOT NULL,
             request_features TEXT    NOT NULL
         )",
    )
    .expect("create v10 learn table");
    conn.execute_batch("PRAGMA user_version = 10")
        .expect("stamp v10");
    assert_eq!(user_version(&conn), 10);

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate v10->v11");

    // Assert: landed at the current version with the column renamed.
    assert_eq!(version, SCHEMA_VERSION);
    assert!(table_exists(&conn, "capability_learn_events"));
    let has_column = |name: &str| -> bool {
        conn.prepare("SELECT 1 FROM pragma_table_info('capability_learn_events') WHERE name=?1")
            .expect("prepare")
            .exists([name])
            .expect("exists")
    };
    assert!(has_column("capability_key"), "column must be renamed");
    assert!(
        !has_column("feature_key"),
        "legacy column name must be gone"
    );

    // Idempotent: re-running the ladder is a no-op.
    let again = migrate_to_current(&conn, 0).expect("re-run migrate");
    assert_eq!(again, SCHEMA_VERSION);
}

/// A v11 DB (created before the `resolved_class` column existed) must
/// migrate to v12: the column is added via ALTER, `user_version` reaches
/// the current version, any pre-existing row survives with
/// `resolved_class` NULL (no backfill), and a second migrate pass is a
/// no-op. Builds a genuine minimal v11 `requests` table so the ALTER path
/// -- not the fresh-schema path -- is exercised.
#[test]
fn v11_to_v12_adds_resolved_class_idempotently_preserving_rows() {
    // Arrange: a v11-shaped DB with a pre-migration row. The minimal
    // column subset is enough to prove the ALTER + row-survival contract.
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
        INSERT INTO meta (key, value) VALUES ('schema_version', '11');
        INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
            requested_model, alias, stream, outcome, latency_ms, tool_count, \
            msg_count, attempt_count, fallback_count) \
            VALUES (1, 2, 'v11-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0);
        PRAGMA user_version = 11;",
    )
    .expect("build v11 db");
    assert_eq!(user_version(&conn), 11);

    let has_resolved_class = |c: &Connection| -> bool {
        c.prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name='resolved_class'")
            .expect("prepare")
            .exists([])
            .expect("query")
    };
    assert!(
        !has_resolved_class(&conn),
        "sanity: v11 table has no resolved_class yet"
    );

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate v11->v12");

    // Assert: landed at the current version, the column was added, and the
    // old row survives reading back NULL (no backfill).
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(user_version(&conn), SCHEMA_VERSION);
    assert!(has_resolved_class(&conn), "v12 must add resolved_class");
    let resolved_class: Option<String> = conn
        .query_row(
            "SELECT resolved_class FROM requests WHERE request_id='v11-row'",
            [],
            |r| r.get(0),
        )
        .expect("old row survives");
    assert!(
        resolved_class.is_none(),
        "migrated v11 row must have NULL resolved_class"
    );
    let meta_version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .expect("meta schema_version");
    assert_eq!(meta_version, SCHEMA_VERSION.to_string());

    // Idempotent: a second pass over an already-current DB is a no-op and
    // does not error on the pre-existing column.
    let again = migrate_to_current(&conn, 0).expect("re-run migrate");
    assert_eq!(again, SCHEMA_VERSION);
    assert!(has_resolved_class(&conn));
}

/// A concurrent read-only viewer opening an un-migrated v11 DB must fail
/// closed with `VersionTooOld` rather than reading a mixed schema (a
/// `requests` table lacking the v12 `resolved_class` column). This is the
/// status-surface poller's fail-closed guard against a live pre-migration
/// file.
#[test]
fn open_readonly_on_unmigrated_v11_db_fails_closed() {
    // Arrange: a genuine v11 DB (user_version 11, no resolved_class).
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
        PRAGMA user_version = 11;",
    )
    .expect("build v11 db");
    drop(conn);

    // Act
    let result = open_readonly(&path);

    // Assert: fails closed on the version check, before any query that
    // would hit the missing column.
    assert!(
        matches!(result, Err(OpenError::VersionTooOld { found: 11, supported }) if supported == SCHEMA_VERSION),
        "un-migrated v11 DB must fail closed as VersionTooOld"
    );
}

/// An older v12-shaped DB (no `capability_events` table) migrates to v13:
/// the table + its `ts` index are created, `user_version` becomes 13, any
/// pre-existing `requests` row survives untouched, the legacy
/// `capability_learn_events` landing pad and its row are left intact, and a
/// second migrate pass is a no-op. Seeds the FULL v12 object set so the
/// create-table migration path runs against a real v12 shape.
#[test]
fn old_v12_db_migrates_to_v13_creating_capability_events_table() {
    // Arrange: a v12-shaped DB carrying the full object set -- requests (+ its
    // ts index), the legacy capability_learn_events landing pad with a row,
    // and meta -- but no capability_events.
    let (_dir, path) = temp_db_path();
    let conn = Connection::open(&path).expect("raw open");
    conn.execute_batch(&format!(
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
            resolved_class TEXT
        );
        {create_ts_index};
        {create_learn_events};
        CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
        INSERT INTO meta (key, value) VALUES ('schema_version', '12');
        INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
            requested_model, alias, stream, outcome, latency_ms, tool_count, \
            msg_count, attempt_count, fallback_count) \
            VALUES (1, 2, 'v12-row', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0);
        INSERT INTO capability_learn_events (ts, state_key, capability_key, \
            provider_kind, signal_tier, observations, upstream_status, \
            remapped, request_features) \
            VALUES (3, 'nn', 'web_search', 'openai-compat', 'inferred', 2, 400, 0, '[\"web_search\"]');
        PRAGMA user_version = 12;",
        create_ts_index = CREATE_TS_START_INDEX,
        create_learn_events = CREATE_CAPABILITY_LEARN_EVENTS_TABLE,
    ))
    .expect("build v12 db");
    assert_eq!(user_version(&conn), 12);
    assert!(!table_exists(&conn, "capability_events"));
    assert!(table_exists(&conn, "capability_learn_events"));

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate v12->v13");

    // Assert: landed at v13, the new table + index exist, the old request
    // row survived, and meta tracks the new version.
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(user_version(&conn), SCHEMA_VERSION);
    assert!(table_exists(&conn, "capability_events"));
    assert!(index_exists(&conn, "idx_capability_events_ts"));
    let survivor: String = conn
        .query_row("SELECT request_id FROM requests", [], |r| r.get(0))
        .expect("request row survives");
    assert_eq!(survivor, "v12-row");
    // The legacy landing pad and its row are left untouched by the v13 step.
    assert!(table_exists(&conn, "capability_learn_events"));
    let legacy_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM capability_learn_events", [], |r| {
            r.get(0)
        })
        .expect("legacy row count");
    assert_eq!(legacy_rows, 1);
    let meta_version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .expect("meta schema_version");
    assert_eq!(meta_version, SCHEMA_VERSION.to_string());

    // Idempotent: re-running the ladder is a no-op.
    let again = migrate_to_current(&conn, 0).expect("re-run migrate");
    assert_eq!(again, SCHEMA_VERSION);
    assert!(table_exists(&conn, "capability_events"));
}

/// Pins the exact `capability_events` column set on a fresh DB. A full-bind
/// INSERT naming every writable column fails to compile-at-SQL-level if a
/// column is added, removed, or renamed, and `pragma_table_info` pins the
/// order, names, and null-ability (only `ts` is NOT NULL; the `id` primary
/// key is auto-assigned and reads back as nullable). This is the
/// forever-contract guard the warm-rebuild replayer relies on.
#[test]
fn capability_events_column_set_is_pinned() {
    // Arrange
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");

    // Act: a full-bind INSERT naming every column in order. A drifted
    // column set makes this statement fail to prepare.
    let inserted = db
        .conn()
        .execute(
            "INSERT INTO capability_events (ts, lane_key, capability, verdict, \
             phase, source, tier, evidence_class, upstream_token, \
             catalog_version, overlay_revision) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                1_i64,
                "lane",
                "web_search",
                "broken",
                "f1",
                "live",
                "inferred",
                Option::<String>::None,
                Option::<String>::None,
                7_i64,
                3_i64,
            ],
        )
        .expect("full-bind insert pins the column set");
    assert_eq!(inserted, 1);

    // Assert: pragma_table_info pins the exact order, names, and
    // null-ability. `id` is the auto-assigned primary key (reads back
    // nullable); only `ts` is NOT NULL.
    let expected: &[(&str, bool)] = &[
        ("id", false),
        ("ts", true),
        ("lane_key", false),
        ("capability", false),
        ("verdict", false),
        ("phase", false),
        ("source", false),
        ("tier", false),
        ("evidence_class", false),
        ("upstream_token", false),
        ("catalog_version", false),
        ("overlay_revision", false),
    ];
    let mut stmt = db
        .conn()
        .prepare("SELECT name, \"notnull\" FROM pragma_table_info('capability_events')")
        .expect("prepare table_info");
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(
        rows.len(),
        expected.len(),
        "capability_events column count drifted"
    );
    for ((actual_name, actual_notnull), (exp_name, exp_notnull)) in rows.iter().zip(expected.iter())
    {
        assert_eq!(actual_name, exp_name, "column name/order mismatch");
        assert_eq!(
            *actual_notnull == 1,
            *exp_notnull,
            "null-ability mismatch for column {actual_name}"
        );
    }
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
        ("would_trim_k_floor", false),
        ("would_trim_shadow_misfire", false),
        ("would_trim_dedup_tokens", false),
        ("would_trim_supersession_tokens", false),
        ("would_trim_path_units", false),
        ("would_trim_path_extractable", false),
        ("would_trim_recorder_version", false),
        ("would_trim_raw_marks", false),
        ("would_trim_context_fraction", false),
        ("resolved_class", false),
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
    for ((actual_name, actual_notnull), (exp_name, exp_notnull)) in rows.iter().zip(expected.iter())
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
fn open_readonly_on_fresh_unwritten_db_returns_version_too_old() {
    // Arrange: a file exists but has no `requests` table and user_version = 0
    // (never migrated). VersionTooOld is now returned before the table check,
    // which prevents a raw "no such column" SQLite error on first query.
    let (_dir, path) = temp_db_path();
    let _conn = Connection::open(&path).expect("create empty sqlite file");

    // Act
    let result = open_readonly(&path);

    // Assert
    assert!(matches!(result, Err(OpenError::VersionTooOld { .. })));
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
fn open_readonly_rejects_older_version() {
    // Arrange: seed a fully-migrated DB, then roll user_version back to
    // simulate a DB that predates this binary (e.g. a pre-migration file
    // from an older install). The requests table exists so the version
    // check is the only thing standing between the caller and a raw
    // "no such column" SQLite error on first query.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("seed db");
    db.conn()
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION - 1))
        .expect("roll version back");
    drop(db);

    // Act
    let result = open_readonly(&path);

    // Assert
    assert!(matches!(result, Err(OpenError::VersionTooOld { .. })));
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

#[test]
fn open_readonly_fastfail_on_nonexistent_path_returns_no_data() {
    // Arrange: a path that does not exist.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("absent.db");

    // Act
    let result = open_readonly_fastfail(&path);

    // Assert: NoData, and the file was NOT created.
    assert!(matches!(result, Err(OpenError::NoData { .. })));
    assert!(
        !path.exists(),
        "open_readonly_fastfail must not create the file"
    );
}

#[test]
fn open_readonly_fastfail_missing_requests_table_returns_no_data() {
    // Arrange: a file at the current schema version but with no
    // `requests` table, so the version check passes and the table
    // probe is what classifies it as NoData.
    let (_dir, path) = temp_db_path();
    let conn = Connection::open(&path).expect("create empty sqlite file");
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
        .expect("stamp current version");
    drop(conn);

    // Act
    let result = open_readonly_fastfail(&path);

    // Assert
    assert!(matches!(result, Err(OpenError::NoData { .. })));
}

#[test]
fn open_readonly_fastfail_on_fresh_unwritten_db_returns_version_too_old() {
    // Arrange: a file exists but has no `requests` table and
    // user_version = 0 (never migrated). VersionTooOld is returned
    // before the table check, mirroring open_readonly.
    let (_dir, path) = temp_db_path();
    let _conn = Connection::open(&path).expect("create empty sqlite file");

    // Act
    let result = open_readonly_fastfail(&path);

    // Assert
    assert!(matches!(result, Err(OpenError::VersionTooOld { .. })));
}

#[test]
fn open_readonly_fastfail_rejects_newer_version() {
    // Arrange: seed a normal DB, then bump user_version above support.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("seed db");
    db.conn()
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
        .expect("bump version");
    drop(db);

    // Act
    let result = open_readonly_fastfail(&path);

    // Assert
    assert!(matches!(result, Err(OpenError::VersionTooNew { .. })));
}

#[test]
fn open_readonly_fastfail_rejects_older_version() {
    // Arrange: seed a fully-migrated DB, then roll user_version back to
    // simulate a DB that predates this binary.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("seed db");
    db.conn()
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION - 1))
        .expect("roll version back");
    drop(db);

    // Act
    let result = open_readonly_fastfail(&path);

    // Assert
    assert!(matches!(result, Err(OpenError::VersionTooOld { .. })));
}

#[test]
fn open_readonly_fastfail_reads_a_healthy_wal_db() {
    // Arrange: seed via the read-write open path, which sets WAL. WAL is
    // a persistent property of the file, so it survives the reopen.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("seed db");
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count) \
             VALUES (10, 20, 'r-ff', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0)",
            [],
        )
        .expect("seed row");
    drop(db);

    // Act
    let ro = open_readonly_fastfail(&path).expect("open readonly fastfail");

    // Assert: a trivial read query answers.
    let count: i64 = ro
        .conn()
        .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 1);
}

#[test]
fn open_readonly_fastfail_reads_wal_db_with_live_writer() {
    // Arrange: open the read-write writer and KEEP it live (do not drop),
    // inserting a row so the WAL sidecars materialize. This mirrors the
    // production scenario: the daemon's usage writer holds the WAL DB open
    // while a status poll opens the same file read-only.
    let (_dir, path) = temp_db_path();
    let writer = open(&path).expect("seed db (live writer)");
    writer
        .conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count) \
             VALUES (10, 20, 'r-live', 'openai', 'm', 'a', 0, 'ok', 5, 0, 0, 1, 0)",
            [],
        )
        .expect("seed row");

    // Act: open read-only WITHOUT dropping the live writer.
    let ro = open_readonly_fastfail(&path).expect("open readonly fastfail under live writer");

    // Assert: a real read query answers against the live-writer WAL DB.
    let count: i64 = ro
        .conn()
        .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
        .expect("count under live writer");
    assert_eq!(count, 1);

    // Keep the writer alive until the read completes.
    drop(writer);
}

#[test]
fn open_readonly_fastfail_rejects_non_wal_db() {
    // Arrange: a fully-migrated DB that was never switched to WAL. A raw
    // connection defaults to the rollback-journal ("delete") mode, so
    // this exercises the fail-closed path without mutating anything.
    let (_dir, path) = temp_db_path();
    let conn = Connection::open(&path).expect("raw open");
    migrate_to_current(&conn, 0).expect("migrate to current schema");
    assert_ne!(
        journal_mode(&conn).to_lowercase(),
        "wal",
        "fixture must genuinely not be in WAL mode"
    );
    drop(conn);

    // Act
    let result = open_readonly_fastfail(&path);

    // Assert: fails closed rather than flipping the journal mode.
    assert!(matches!(result, Err(OpenError::NotWal { .. })));

    // And the journal mode was NOT mutated by the failed open.
    let check = Connection::open(&path).expect("reopen");
    assert_ne!(
        journal_mode(&check).to_lowercase(),
        "wal",
        "open_readonly_fastfail must not flip the journal mode"
    );
}

#[test]
fn open_readonly_fastfail_sets_small_busy_timeout() {
    // Arrange
    let (_dir, path) = temp_db_path();
    drop(open(&path).expect("seed db"));

    // Act
    let ro = open_readonly_fastfail(&path).expect("open readonly fastfail");
    let timeout: i64 = ro
        .conn()
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .expect("read busy_timeout");

    // Assert: the fast-fail timeout is distinct and well under the 5s
    // CLI-viewer timeout (the const relationship is guarded at compile
    // time; here we prove the runtime PRAGMA took the small value).
    assert_eq!(timeout, FASTFAIL_BUSY_TIMEOUT_MS as i64);
}
