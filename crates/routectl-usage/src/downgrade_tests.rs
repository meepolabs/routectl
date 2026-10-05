use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::capability_event::{CapabilityEvent, insert_capability_event};
use crate::db::{open, open_readonly};
use crate::migrate::migrate_to_current;
use crate::schema::{CREATE_CAPABILITY_EVENTS_TABLE, CREATE_META_TABLE, CREATE_REQUESTS_TABLE};

fn temp_db_path() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

fn user_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version")
}

fn meta_version(conn: &Connection) -> String {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        [META_SCHEMA_VERSION],
        |r| r.get(0),
    )
    .expect("meta schema_version")
}

/// The `capability_events` DDL as v16 created it: the current DDL with the two
/// trailing v17 columns cut off. Derived from the live constant so it cannot
/// drift from the real prior shape.
fn v16_capability_events_ddl() -> String {
    const LAST_V16_COLUMN: &str = "    overlay_revision INTEGER,";
    let (head, tail) = CREATE_CAPABILITY_EVENTS_TABLE
        .split_once(LAST_V16_COLUMN)
        .expect("the DDL still ends its v16 set with overlay_revision");
    assert!(
        tail.contains("provider_kind") && tail.contains("vocab_version"),
        "the v17 pair must be the trailing columns"
    );
    format!("{head}    overlay_revision INTEGER\n)")
}

/// A WAL file stamped v17 whose `capability_events` is the v16 DDL followed
/// by `added_columns`, each appended with `ALTER TABLE ... ADD COLUMN`.
fn v17_file_with(path: &std::path::Path, added_columns: &[&str]) -> Connection {
    v17_file_from_ddl(path, &v16_capability_events_ddl(), added_columns)
}

/// A WAL file stamped v17 whose `capability_events` is created by `ddl` and
/// then extended by `added_columns`.
fn v17_file_from_ddl(path: &std::path::Path, ddl: &str, added_columns: &[&str]) -> Connection {
    let conn = Connection::open(path).expect("raw open");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    conn.execute_batch(CREATE_REQUESTS_TABLE).expect("requests");
    conn.execute_batch(CREATE_META_TABLE).expect("meta");
    conn.execute_batch(ddl).expect("capability_events");
    for column in added_columns {
        conn.execute_batch(&format!(
            "ALTER TABLE capability_events ADD COLUMN {column}"
        ))
        .expect("add column");
    }
    conn.execute_batch(
        "INSERT INTO meta (key, value) VALUES ('schema_version', '17');
         PRAGMA user_version = 17;",
    )
    .expect("stamp v17");
    conn
}

/// `ddl` with one column definition rewritten. Panics unless `from` occurs
/// exactly once, so a DDL edit cannot silently turn a refusal row into a copy
/// of the real shape.
fn rewrite_once(ddl: &str, from: &str, to: &str) -> String {
    assert_eq!(
        ddl.matches(from).count(),
        1,
        "{from:?} must occur exactly once in the DDL"
    );
    ddl.replacen(from, to, 1)
}

/// Assert `path` is refused on shape and is still stamped v17.
fn assert_refused_on_shape(path: &std::path::Path, name: &str) {
    let result = downgrade_to_v16(path);
    assert!(
        matches!(result, Err(DowngradeError::NotAdditive { .. })),
        "{name}: {result:?}"
    );
    let conn = Connection::open(path).expect("reopen");
    assert_eq!(user_version(&conn), 17, "{name}");
    assert_eq!(meta_version(&conn), "17", "{name}");
}

/// One legacy-shaped negative, as a v16 binary would write it.
fn negative() -> CapabilityEvent {
    CapabilityEvent {
        ts: 10,
        lane_key: "lane".to_string(),
        capability: "web_search".to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: 7,
        overlay_revision: 3,
        provider_kind: Some("openai-compat".to_string()),
        vocab_version: Some(2),
    }
}

/// The capability-event INSERT and read a v16 binary issues: every v16 column
/// named explicitly, so the two v17 columns are invisible to it.
const V16_INSERT: &str = "INSERT INTO capability_events (ts, lane_key, capability, verdict, \
     phase, source, tier, evidence_class, upstream_token, catalog_version, overlay_revision) \
     VALUES (20, 'lane', 'thinking', 'broken', 'f1', 'live', 'inferred', NULL, NULL, 7, 3)";
const V16_READ: &str = "SELECT rowid, ts, lane_key, capability, verdict, phase, source, tier, \
     evidence_class, upstream_token, catalog_version, overlay_revision \
     FROM capability_events ORDER BY rowid";

#[test]
fn downgrades_a_fresh_v17_db_and_a_v16_reader_and_writer_use_it() {
    // Arrange: a fresh v17 DB with one stamped row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open fresh");
    insert_capability_event(db.conn(), &negative()).expect("insert");
    drop(db);

    // Act
    downgrade_to_v16(&path).expect("downgrade");

    // Assert: both stamps read 16, the journal is still WAL, and the v16
    // statements work against the file with its rows intact.
    let conn = Connection::open(&path).expect("reopen");
    assert_eq!(user_version(&conn), 16);
    assert_eq!(meta_version(&conn), "16");
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("journal_mode");
    assert_eq!(mode, "wal");
    conn.execute(V16_INSERT, []).expect("v16 insert");
    let rows: Vec<(i64, String)> = conn
        .prepare(V16_READ)
        .expect("v16 read prepares")
        .query_map([], |r| Ok((r.get(0)?, r.get(3)?)))
        .expect("v16 read")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        rows,
        vec![(1, "web_search".to_string()), (2, "thinking".to_string())]
    );
    let stamped: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT provider_kind, vocab_version FROM capability_events WHERE rowid = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("v17 columns kept");
    assert_eq!(stamped, (Some("openai-compat".to_string()), Some(2)));
}

#[test]
fn a_downgraded_file_migrates_back_to_v17_with_v16_rows_reading_legacy() {
    // Arrange: a downgraded file a v16 binary then wrote to.
    let (_dir, path) = temp_db_path();
    drop(open(&path).expect("open fresh"));
    downgrade_to_v16(&path).expect("downgrade");
    Connection::open(&path)
        .expect("reopen")
        .execute(V16_INSERT, [])
        .expect("v16 insert");

    // Act: a v17 open migrates it forward again.
    let db = open(&path).expect("re-upgrade");

    // Assert: v17 again, same column shape, and the v16-written row carries a
    // NULL vocabulary (legacy).
    assert_eq!(user_version(db.conn()), crate::schema::SCHEMA_VERSION);
    let rows = crate::query::read_capability_events_after(db.conn(), 0, 10).expect("read");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].vocab_version, None);
    assert_eq!(rows[0].provider_kind, None);
    drop(db);
    downgrade_to_v16(&path).expect("the re-upgraded file is still exactly additive");
}

#[test]
fn downgrades_a_v16_db_that_migrated_to_v17() {
    // Arrange: a v16-shaped file migrated forward by the ladder.
    let (_dir, path) = temp_db_path();
    let conn = v17_file_with(&path, &[]);
    conn.execute_batch(
        "UPDATE meta SET value = '16' WHERE key = 'schema_version';
         PRAGMA user_version = 16;",
    )
    .expect("restamp v16");
    migrate_to_current(&conn, 0).expect("migrate v16 -> v17");
    drop(conn);

    // Act + Assert
    downgrade_to_v16(&path).expect("a migrated v17 file is additive");
    assert_eq!(user_version(&Connection::open(&path).expect("reopen")), 16);
}

#[test]
fn refuses_while_another_connection_has_the_file_open() {
    // The daemon's writer holds a read-write connection for its lifetime, and a
    // viewer holds a read-only one; either must block the downgrade, even idle.
    type Holder = fn(&std::path::Path) -> Box<dyn std::any::Any>;
    let holders: &[(&str, Holder)] = &[
        ("idle writer connection", |p| {
            Box::new(open(p).expect("writer"))
        }),
        ("idle read-only viewer", |p| {
            Box::new(open_readonly(p).expect("viewer"))
        }),
    ];
    for (name, hold) in holders {
        // Arrange
        let (_dir, path) = temp_db_path();
        drop(open(&path).expect("open fresh"));
        let holder = hold(&path);

        // Act
        let result = downgrade_to_v16(&path);

        // Assert: refused as in-use, and the file is still v17.
        assert!(
            matches!(result, Err(DowngradeError::InUse)),
            "{name}: {result:?}"
        );
        drop(holder);
        let conn = Connection::open(&path).expect("reopen");
        assert_eq!(user_version(&conn), 17, "{name}");
        assert_eq!(meta_version(&conn), "17", "{name}");
    }
}

#[test]
fn refuses_a_file_that_is_not_exactly_the_additive_v17_shape() {
    let cases: &[(&str, &[&str])] = &[
        ("only the v16 columns", &[]),
        ("missing vocab_version", &["provider_kind TEXT"]),
        (
            "an extra trailing column",
            &["provider_kind TEXT", "vocab_version INTEGER", "extra TEXT"],
        ),
        (
            "the pair in the wrong order",
            &["vocab_version INTEGER", "provider_kind TEXT"],
        ),
        (
            "provider_kind with the wrong type",
            &["provider_kind INTEGER", "vocab_version INTEGER"],
        ),
        (
            "vocab_version with the wrong type",
            &["provider_kind TEXT", "vocab_version TEXT"],
        ),
    ];
    for (name, added) in cases {
        // Arrange
        let (_dir, path) = temp_db_path();
        drop(v17_file_with(&path, added));

        // Act + Assert
        assert_refused_on_shape(&path, name);
    }
}

#[test]
fn refuses_an_added_column_carrying_a_default_or_a_generated_column() {
    let cases: &[(&str, &[&str])] = &[
        (
            "provider_kind with a DEFAULT",
            &[
                "provider_kind TEXT DEFAULT 'openai-compat'",
                "vocab_version INTEGER",
            ],
        ),
        (
            "vocab_version as a generated column",
            &[
                "provider_kind TEXT",
                "vocab_version INTEGER GENERATED ALWAYS AS (1) VIRTUAL",
            ],
        ),
        (
            "an extra trailing generated column",
            &[
                "provider_kind TEXT",
                "vocab_version INTEGER",
                "shadow INTEGER GENERATED ALWAYS AS (1) VIRTUAL",
            ],
        ),
    ];
    for (name, added) in cases {
        // Arrange
        let (_dir, path) = temp_db_path();
        drop(v17_file_with(&path, added));

        // Act + Assert
        assert_refused_on_shape(&path, name);
    }
}

#[test]
fn refuses_a_column_whose_constraints_differ_from_the_frozen_shape() {
    // Each row rewrites column definitions of the real v17 DDL; names and
    // declared types are unchanged, so only the constraint comparison can
    // refuse them.
    type Rewrite = (&'static str, &'static str);
    let cases: &[(&str, &[Rewrite])] = &[
        (
            "provider_kind TEXT NOT NULL",
            &[("provider_kind    TEXT,", "provider_kind    TEXT NOT NULL,")],
        ),
        (
            "v16 ts without NOT NULL",
            &[(
                "ts               INTEGER NOT NULL,",
                "ts               INTEGER,",
            )],
        ),
        (
            "v16 lane_key NOT NULL",
            &[("lane_key         TEXT,", "lane_key         TEXT NOT NULL,")],
        ),
        (
            "v16 id not the primary key",
            &[(
                "id               INTEGER PRIMARY KEY,",
                "id               INTEGER,",
            )],
        ),
        (
            "v16 primary key moved from id to ts",
            &[
                (
                    "id               INTEGER PRIMARY KEY,",
                    "id               INTEGER,",
                ),
                (
                    "ts               INTEGER NOT NULL,",
                    "ts               INTEGER NOT NULL PRIMARY KEY,",
                ),
            ],
        ),
    ];
    for (name, rewrites) in cases {
        // Arrange
        let (_dir, path) = temp_db_path();
        let ddl = rewrites.iter().fold(
            CREATE_CAPABILITY_EVENTS_TABLE.to_string(),
            |ddl, (from, to)| rewrite_once(&ddl, from, to),
        );
        drop(v17_file_from_ddl(&path, &ddl, &[]));

        // Act + Assert
        assert_refused_on_shape(&path, name);
    }
}

#[test]
fn the_exact_additive_shape_is_accepted_by_the_shape_fixture() {
    // Positive control for the refusal table: the same fixture with exactly the
    // additive pair downgrades, so each refusal above is caused by its column
    // difference and not by the fixture.
    let (_dir, path) = temp_db_path();
    drop(v17_file_with(
        &path,
        &["provider_kind TEXT", "vocab_version INTEGER"],
    ));

    downgrade_to_v16(&path).expect("exact additive shape downgrades");
}

#[test]
fn the_current_ddl_is_accepted_by_the_constraint_fixture() {
    // Positive control for the constraint table: the unmodified v17 DDL built
    // through the same fixture downgrades.
    let (_dir, path) = temp_db_path();
    drop(v17_file_from_ddl(
        &path,
        CREATE_CAPABILITY_EVENTS_TABLE,
        &[],
    ));

    downgrade_to_v16(&path).expect("the real v17 DDL downgrades");
}

#[test]
fn refuses_a_file_at_any_version_other_than_17() {
    for version in [0_i64, 15, 16, 18] {
        // Arrange
        let (_dir, path) = temp_db_path();
        let conn = v17_file_with(&path, &["provider_kind TEXT", "vocab_version INTEGER"]);
        conn.execute_batch(&format!("PRAGMA user_version = {version}"))
            .expect("restamp");
        drop(conn);

        // Act
        let result = downgrade_to_v16(&path);

        // Assert
        assert!(
            matches!(
                result,
                Err(DowngradeError::UnexpectedVersion { found, expected: 17 }) if found == version
            ),
            "v{version}: {result:?}"
        );
        assert_eq!(
            user_version(&Connection::open(&path).expect("reopen")),
            version
        );
    }
}

#[test]
fn refuses_when_the_meta_mirror_disagrees() {
    for (name, sql) in [
        (
            "meta says 16",
            "UPDATE meta SET value = '16' WHERE key = 'schema_version'",
        ),
        (
            "meta row absent",
            "DELETE FROM meta WHERE key = 'schema_version'",
        ),
    ] {
        // Arrange
        let (_dir, path) = temp_db_path();
        let conn = v17_file_with(&path, &["provider_kind TEXT", "vocab_version INTEGER"]);
        conn.execute_batch(sql).expect("tamper meta");
        drop(conn);

        // Act
        let result = downgrade_to_v16(&path);

        // Assert
        assert!(
            matches!(result, Err(DowngradeError::MetaMismatch { .. })),
            "{name}: {result:?}"
        );
        assert_eq!(
            user_version(&Connection::open(&path).expect("reopen")),
            17,
            "{name}"
        );
    }
}

#[test]
fn a_missing_file_is_refused_and_not_created() {
    let (_dir, path) = temp_db_path();

    let result = downgrade_to_v16(&path);

    assert!(matches!(result, Err(DowngradeError::NoData { .. })));
    assert!(!path.exists());
}
