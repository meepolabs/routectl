//! Tests for the 19 -> 18 step and for chaining it into the older steps.

use rusqlite::Connection;

use super::tests::{meta_version, rewrite_once, temp_db_path, user_version};
use super::v18_tests::{
    FileState, V17_INSERT_COLUMNS, count, file_state, fresh_with_rows, insert_through,
    requests_column_names,
};
use super::*;
use crate::db::{open, open_readonly};
use crate::schema::{
    CREATE_CAPABILITY_EVENTS_TABLE, CREATE_LEDGER_GAPS_TABLE, CREATE_META_TABLE,
    CREATE_REQUESTS_TABLE, SCHEMA_VERSION,
};

/// The `requests` columns v18 appended, frozen as text: with
/// [`V17_INSERT_COLUMNS`] they are the column list a v18 writer's `INSERT`
/// names. A v18 binary handed a downgraded file writes through exactly that
/// list.
pub(super) const V18_INSERT_TAIL: &str = "\
    operation, probe, harness,
    wire_losses, translation_drops, local_refusals, field_repairs, missing_terminals";

/// The six v19 skip-count columns, frozen as text in physical order.
pub(super) const V19_SKIP_COLUMNS: [&str; 6] = [
    "skip_credential_unusable",
    "skip_account_parked",
    "skip_breaker_open",
    "skip_rate_limited",
    "skip_capability",
    "skip_context_window",
];

/// Insert one request row through the frozen v18 column list.
fn insert_as_v18_writer(conn: &Connection, request_id: &str) {
    insert_through(
        conn,
        &format!("{V17_INSERT_COLUMNS},\n{V18_INSERT_TAIL}"),
        request_id,
    );
}

/// The six skip counts of one request row; `None` is a NULL column.
fn skip_counts(conn: &Connection, request_id: &str) -> Vec<Option<i64>> {
    conn.query_row(
        &format!(
            "SELECT {} FROM requests WHERE request_id = ?1",
            V19_SKIP_COLUMNS.join(", ")
        ),
        [request_id],
        |r| (0..V19_SKIP_COLUMNS.len()).map(|i| r.get(i)).collect(),
    )
    .expect("skip counts")
}

/// The current `requests` DDL cut after its v18 set, so the v19 tail can be
/// appended by `ALTER TABLE` in a chosen shape.
fn requests_ddl_through_v18() -> String {
    const LAST_V18_COLUMN: &str = "    missing_terminals INTEGER,";
    let (head, tail) = CREATE_REQUESTS_TABLE
        .split_once(LAST_V18_COLUMN)
        .expect("the DDL still ends its v18 set with missing_terminals");
    for column in V19_SKIP_COLUMNS {
        assert!(tail.contains(column), "{column} must be in the v19 tail");
    }
    format!("{head}    missing_terminals INTEGER\n)")
}

/// The six v19 columns as `ALTER TABLE ... ADD COLUMN` definitions.
fn v19_tail() -> Vec<String> {
    V19_SKIP_COLUMNS
        .iter()
        .map(|name| format!("{name} INTEGER"))
        .collect()
}

/// A WAL file stamped `version` whose `requests` is created by
/// `requests_ddl` then extended by `added_columns`, with the current
/// `ledger_gaps` and `capability_events`.
pub(super) fn file_from(
    path: &std::path::Path,
    version: i64,
    requests_ddl: &str,
    added_columns: &[&str],
) {
    let conn = Connection::open(path).expect("raw open");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    conn.execute_batch(requests_ddl).expect("requests");
    for column in added_columns {
        conn.execute_batch(&format!("ALTER TABLE requests ADD COLUMN {column}"))
            .expect("add column");
    }
    conn.execute_batch(CREATE_LEDGER_GAPS_TABLE)
        .expect("ledger_gaps");
    conn.execute_batch(CREATE_META_TABLE).expect("meta");
    conn.execute_batch(CREATE_CAPABILITY_EVENTS_TABLE)
        .expect("capability_events");
    conn.execute_batch(&format!(
        "INSERT INTO meta (key, value) VALUES ('schema_version', '{version}');
         PRAGMA user_version = {version};"
    ))
    .expect("stamp");
}

/// A v19 file as the rollback path leaves it: a fresh current file with rows,
/// stepped down to 19, so `requests` still carries the trailing v20 columns.
pub(super) fn fresh_v19_with_rows(path: &std::path::Path) {
    fresh_with_rows(path);
    downgrade_to(path, 19).expect("step the fresh file down to 19");
}

#[test]
fn the_fresh_schema_carries_the_six_skip_columns_after_the_v18_set() {
    // Arrange + Act
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open fresh");

    // Assert
    let columns = requests_column_names(db.conn());
    let start = columns
        .iter()
        .position(|c| c == "missing_terminals")
        .expect("the v18 set ends with missing_terminals")
        + 1;
    assert_eq!(
        &columns[start..start + V19_SKIP_COLUMNS.len()],
        V19_SKIP_COLUMNS
    );
}

#[test]
fn a_v18_file_with_rows_migrates_to_v19_with_old_rows_null_and_new_rows_zero() {
    // Arrange: a genuine v18 file holding one row a v18 writer wrote.
    let (_dir, path) = temp_db_path();
    file_from(&path, 18, &requests_ddl_through_v18(), &[]);
    insert_as_v18_writer(&Connection::open(&path).expect("reopen"), "v18-row");
    let (_fresh_dir, fresh_path) = temp_db_path();
    let fresh_columns = requests_column_names(open(&fresh_path).expect("fresh").conn());

    // Act: the production writer opens (and migrates) the file, then writes.
    let (handle, writer) = crate::writer::UsageWriter::start(path.clone(), 4, 0, true);
    handle.try_send(crate::record::UsageRecord {
        request_id: "v19-row".to_string(),
        skip_breaker_open: 2,
        ..crate::record::UsageRecord::default()
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while writer.counters().persisted() < 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        writer.counters().persisted(),
        1,
        "the current row persisted"
    );
    drop(handle);
    writer.shutdown();
    let db = open(&path).expect("reopen at the current version");

    // Assert: stamped current, the fresh column list, the old row NULL in
    // every skip column, the new row 0 except where it counted.
    assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
    assert_eq!(meta_version(db.conn()), SCHEMA_VERSION.to_string());
    assert_eq!(requests_column_names(db.conn()), fresh_columns);
    assert_eq!(skip_counts(db.conn(), "v18-row"), vec![None; 6]);
    assert_eq!(
        skip_counts(db.conn(), "v19-row"),
        vec![Some(0), Some(0), Some(2), Some(0), Some(0), Some(0)]
    );
    drop(db);

    // Assert: reopening is idempotent.
    let again = open(&path).expect("reopen at the current version");
    assert_eq!(user_version(again.conn()), SCHEMA_VERSION);
    assert_eq!(requests_column_names(again.conn()), fresh_columns);
    assert_eq!(count(again.conn(), "requests"), Some(2));
}

#[test]
fn downgrades_a_v19_db_to_18_keeping_every_column_and_row() {
    // Arrange
    let (_dir, path) = temp_db_path();
    fresh_v19_with_rows(&path);
    let before = file_state(&path);

    // Act
    downgrade_to(&path, 18).expect("downgrade");

    // Assert: only the two version values changed.
    let after = file_state(&path);
    assert_eq!(after.user_version, 18);
    assert_eq!(after.meta_version, "18");
    assert_eq!(after.schema, before.schema);
    assert_eq!(after.requests, before.requests);
    assert_eq!(after.ledger_gaps, before.ledger_gaps);
}

#[test]
fn a_v18_writer_inserts_into_a_downgraded_file_and_reopening_re_stamps_it() {
    // Arrange: a downgraded file a v18 binary then wrote to.
    let (_dir, path) = temp_db_path();
    fresh_v19_with_rows(&path);
    let fresh_columns = requests_column_names(&Connection::open(&path).expect("reopen"));
    downgrade_to(&path, 18).expect("downgrade");
    insert_as_v18_writer(&Connection::open(&path).expect("reopen"), "v18-row");

    // Act: the current binary opens it again.
    let db = open(&path).expect("re-upgrade");

    // Assert: current again with the same column list (nothing re-added),
    // both rows present, and the v18-written row NULL in every skip column.
    assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
    assert_eq!(meta_version(db.conn()), SCHEMA_VERSION.to_string());
    assert_eq!(requests_column_names(db.conn()), fresh_columns);
    assert_eq!(count(db.conn(), "requests"), Some(2));
    assert_eq!(skip_counts(db.conn(), "v18-row"), vec![None; 6]);
    drop(db);
    downgrade_to(&path, 18).expect("the re-stamped file is still exactly additive");
}

#[test]
fn downgrades_a_v19_file_through_every_step() {
    for target in [18, 17, 16] {
        // Arrange
        let (_dir, path) = temp_db_path();
        fresh_v19_with_rows(&path);
        let before = file_state(&path);

        // Act
        downgrade_to(&path, target).expect("downgrade");

        // Assert: the stamps read the target and nothing else changed.
        let after = file_state(&path);
        assert_eq!(
            after,
            FileState {
                user_version: target,
                meta_version: target.to_string(),
                ..before
            },
            "--to {target}"
        );
    }
}

#[test]
fn a_file_stepped_down_to_18_and_written_by_a_v18_binary_still_steps_to_16() {
    // Arrange: the rollback path, then a later wish to go one release further.
    let (_dir, path) = temp_db_path();
    fresh_with_rows(&path);
    downgrade_to(&path, 18).expect("first rollback");
    insert_as_v18_writer(&Connection::open(&path).expect("reopen"), "v18-row");

    // Act
    downgrade_to(&path, 16).expect("a v18 file carrying the v19 columns steps on");

    // Assert
    let after = file_state(&path);
    assert_eq!(after.user_version, 16);
    assert_eq!(after.requests, 2);
}

/// One refusal row: the version the file is stamped at, the `requests` DDL,
/// the columns appended to it, and the reason the refusal must name.
type ShapeCase<'a> = (&'a str, i64, &'a str, &'a [&'a str], NotAdditiveReason);

#[test]
fn refuses_a_file_whose_requests_is_not_exactly_the_additive_v19_shape() {
    use NotAdditiveReason::{ColumnCount, ColumnShape};
    let tail = v19_tail();
    let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
    let through_v18 = requests_ddl_through_v18();
    let with_extra: Vec<&str> = tail.iter().copied().chain(["extra_col INTEGER"]).collect();
    let mut swapped = tail.clone();
    swapped.swap(0, 1);
    let skip_capability = "    skip_capability INTEGER,";
    let defaulted = rewrite_once(
        CREATE_REQUESTS_TABLE,
        skip_capability,
        "    skip_capability INTEGER DEFAULT 0,",
    );
    let wrong_type = rewrite_once(
        CREATE_REQUESTS_TABLE,
        skip_capability,
        "    skip_capability TEXT,",
    );
    let cases: &[ShapeCase<'_>] = &[
        (
            "an extra unknown column",
            19,
            &through_v18,
            &with_extra,
            ColumnCount,
        ),
        (
            "a missing skip column",
            19,
            &through_v18,
            &tail[..5],
            ColumnCount,
        ),
        (
            "the skip columns out of order",
            19,
            &through_v18,
            &swapped,
            ColumnShape,
        ),
        (
            "a skip column with a default",
            19,
            &defaulted,
            &[],
            ColumnShape,
        ),
        (
            "a skip column of the wrong type",
            19,
            &wrong_type,
            &[],
            ColumnShape,
        ),
        (
            "v19 stamped without the skip columns",
            19,
            &through_v18,
            &[],
            ColumnCount,
        ),
        (
            "stamped 18, an extra unknown column",
            18,
            &through_v18,
            &with_extra,
            ColumnCount,
        ),
        (
            "stamped 18, skip columns out of order",
            18,
            &through_v18,
            &swapped,
            ColumnShape,
        ),
    ];
    for &(name, version, ddl, added, reason) in cases {
        for target in (16..version).rev() {
            // Arrange
            let (_dir, path) = temp_db_path();
            file_from(&path, version, ddl, added);
            let before = file_state(&path);

            // Act
            let result = downgrade_to(&path, target);

            // Assert: refused on `requests`, and the file is exactly as it was.
            assert!(
                matches!(
                    result,
                    Err(DowngradeError::NotAdditive { table: "requests", reason: r })
                        if r == reason
                ),
                "{name} --to {target}: {result:?}"
            );
            assert_eq!(file_state(&path), before, "{name} --to {target}");
        }
    }
}

#[test]
fn the_exact_additive_v19_shape_is_accepted_by_the_shape_fixture() {
    // Positive controls for the refusal table: the same builder with the real
    // DDL, and with the v18 DDL plus the exact tail, both downgrade all the
    // way; a v18 file without the tail still downgrades too.
    let tail = v19_tail();
    let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
    let through_v18 = requests_ddl_through_v18();
    let cases: &[(&str, i64, &str, &[&str])] = &[
        (
            "the current DDL, trailing v20 columns included",
            19,
            CREATE_REQUESTS_TABLE,
            &[],
        ),
        ("v18 DDL plus the appended tail", 19, &through_v18, &tail),
        ("stamped 18 with the appended tail", 18, &through_v18, &tail),
        ("stamped 18 without the tail", 18, &through_v18, &[]),
    ];
    for &(name, version, ddl, added) in cases {
        let (_dir, path) = temp_db_path();
        file_from(&path, version, ddl, added);

        downgrade_to(&path, 16).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(file_state(&path).user_version, 16, "{name}");
    }
}

#[test]
fn refuses_while_another_connection_has_the_file_open_on_the_way_to_18() {
    // Arrange: an idle writer connection and an idle read-only viewer.
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
        let (_dir, path) = temp_db_path();
        fresh_with_rows(&path);
        let before = file_state(&path);
        let holder = hold(&path);

        // Act
        let result = downgrade_to(&path, 18);

        // Assert: refused busy, and the file is unchanged.
        assert!(
            matches!(result, Err(DowngradeError::InUse)),
            "{name}: {result:?}"
        );
        drop(holder);
        assert_eq!(file_state(&path), before, "{name}");
    }
}

#[test]
fn refuses_a_v19_file_whose_meta_mirror_disagrees() {
    // Arrange
    let (_dir, path) = temp_db_path();
    fresh_v19_with_rows(&path);
    Connection::open(&path)
        .expect("reopen")
        .execute_batch("UPDATE meta SET value = '18' WHERE key = 'schema_version'")
        .expect("tamper meta");

    // Act
    let result = downgrade_to(&path, 18);

    // Assert
    assert!(
        matches!(result, Err(DowngradeError::MetaMismatch { .. })),
        "{result:?}"
    );
    assert_eq!(file_state(&path).user_version, 19);
}

#[test]
fn v18_to_v19_failure_after_the_first_alter_leaves_the_file_at_v18() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Arrange: a v18 file whose connection allows the first `ALTER TABLE
    // requests` and denies every later one.
    let (_dir, path) = temp_db_path();
    file_from(&path, 18, &requests_ddl_through_v18(), &[]);
    let conn = Connection::open(&path).expect("reopen");
    let columns_before = requests_column_names(&conn);
    let alters = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&alters);
    conn.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
        AuthAction::AlterTable {
            table_name: "requests",
            ..
        } => {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Allow,
    }))
    .expect("install authorizer");

    // Act
    let result = crate::migrate::migrate_to_current(&conn, 0);

    // Assert: failed at the second ALTER, and nothing the step did survived.
    assert!(
        result.is_err(),
        "the injected failure must surface: {result:?}"
    );
    assert_eq!(
        alters.load(Ordering::SeqCst),
        2,
        "failed at the second ALTER"
    );
    assert_eq!(user_version(&conn), 18);
    assert_eq!(meta_version(&conn), "18");
    assert_eq!(requests_column_names(&conn), columns_before);

    // Assert: once the fault is gone the same file migrates normally.
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("clear authorizer");
    assert_eq!(
        crate::migrate::migrate_to_current(&conn, 0).expect("retry"),
        SCHEMA_VERSION
    );
    let (_fresh_dir, fresh_path) = temp_db_path();
    let fresh_columns = requests_column_names(open(&fresh_path).expect("fresh").conn());
    assert_eq!(requests_column_names(&conn), fresh_columns);
}
