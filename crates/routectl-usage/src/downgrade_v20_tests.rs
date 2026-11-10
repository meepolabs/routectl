//! Tests for the v19 -> v20 migration, the 20 -> 19 step, and chaining it
//! into the older steps.

use rusqlite::Connection;

use super::tests::{meta_version, rewrite_once, temp_db_path, user_version};
use super::v18_tests::{
    FileState, V17_INSERT_COLUMNS, count, file_state, fresh_with_rows, insert_through,
    requests_column_names,
};
use super::v19_tests::{V18_INSERT_TAIL, V19_SKIP_COLUMNS, file_from};
use super::*;
use crate::db::open;
use crate::schema::{CREATE_REQUESTS_TABLE, SCHEMA_VERSION};

/// The five v20 walk-count columns, frozen as text in physical order.
const V20_WALK_COLUMNS: [&str; 5] = [
    "request_caused_failures",
    "excluded_endpoint",
    "excluded_model",
    "excluded_target",
    "excluded_probe_run",
];

/// The column list a v19 writer's `INSERT` names: the v17 and v18 lists
/// followed by the six skip counts. A v19 binary handed a downgraded file
/// writes through exactly this list.
fn v19_insert_columns() -> String {
    format!(
        "{V17_INSERT_COLUMNS},\n{V18_INSERT_TAIL},\n{}",
        V19_SKIP_COLUMNS.join(", ")
    )
}

fn insert_as_v19_writer(conn: &Connection, request_id: &str) {
    insert_through(conn, &v19_insert_columns(), request_id);
}

/// The five walk counts of one request row; `None` is a NULL column.
fn walk_counts(conn: &Connection, request_id: &str) -> Vec<Option<i64>> {
    conn.query_row(
        &format!(
            "SELECT {} FROM requests WHERE request_id = ?1",
            V20_WALK_COLUMNS.join(", ")
        ),
        [request_id],
        |r| (0..V20_WALK_COLUMNS.len()).map(|i| r.get(i)).collect(),
    )
    .expect("walk counts")
}

/// The current `requests` DDL cut after its v19 set, so the v20 tail can be
/// appended by `ALTER TABLE` in a chosen shape.
fn requests_ddl_through_v19() -> String {
    const LAST_V19_COLUMN: &str = "    skip_context_window INTEGER,";
    let (head, tail) = CREATE_REQUESTS_TABLE
        .split_once(LAST_V19_COLUMN)
        .expect("the DDL still ends its v19 set with skip_context_window");
    for column in V20_WALK_COLUMNS {
        assert!(tail.contains(column), "{column} must be in the v20 tail");
    }
    format!("{head}    skip_context_window INTEGER\n)")
}

/// The five v20 columns as `ALTER TABLE ... ADD COLUMN` definitions.
fn v20_tail() -> Vec<String> {
    V20_WALK_COLUMNS
        .iter()
        .map(|name| format!("{name} INTEGER"))
        .collect()
}

/// Block until `writer` has persisted `want` rows, or fail after 10 s.
fn wait_persisted(writer: &crate::writer::UsageWriter, want: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while writer.counters().persisted() < want && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(writer.counters().persisted(), want, "rows persisted");
}

#[test]
fn the_fresh_schema_is_v20_with_the_five_walk_columns_last() {
    // Arrange + Act
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open fresh");

    // Assert
    assert_eq!(SCHEMA_VERSION, 20);
    assert_eq!(user_version(db.conn()), 20);
    assert_eq!(meta_version(db.conn()), "20");
    let columns = requests_column_names(db.conn());
    assert_eq!(
        &columns[columns.len() - V20_WALK_COLUMNS.len()..],
        V20_WALK_COLUMNS
    );
}

#[test]
fn a_seeded_v19_ledger_migrates_to_v20_with_null_walk_columns_on_old_rows() {
    // Arrange: a genuine v19 file holding two rows a v19 writer wrote.
    let (_dir, path) = temp_db_path();
    file_from(&path, 19, &requests_ddl_through_v19(), &[]);
    {
        let conn = Connection::open(&path).expect("reopen");
        insert_as_v19_writer(&conn, "v19-row-a");
        insert_as_v19_writer(&conn, "v19-row-b");
    }
    let (_fresh_dir, fresh_path) = temp_db_path();
    let fresh_columns = requests_column_names(open(&fresh_path).expect("fresh").conn());

    // Act: the production writer opens (and migrates) the file, then writes.
    let (handle, writer) = crate::writer::UsageWriter::start(path.clone(), 4, 0, true);
    handle.try_send(crate::record::UsageRecord {
        request_id: "v20-row".to_string(),
        excluded_model: 3,
        ..crate::record::UsageRecord::default()
    });
    wait_persisted(&writer, 1);
    drop(handle);
    writer.shutdown();
    let db = open(&path).expect("reopen at v20");

    // Assert: stamped 20, the fresh column list, the old rows NULL in every
    // walk column, the new row 0 except where it counted.
    assert_eq!(user_version(db.conn()), 20);
    assert_eq!(meta_version(db.conn()), "20");
    assert_eq!(requests_column_names(db.conn()), fresh_columns);
    for old in ["v19-row-a", "v19-row-b"] {
        assert_eq!(walk_counts(db.conn(), old), vec![None; 5], "{old}");
    }
    assert_eq!(
        walk_counts(db.conn(), "v20-row"),
        vec![Some(0), Some(0), Some(3), Some(0), Some(0)]
    );
    drop(db);

    // Assert: reopening is idempotent.
    let again = open(&path).expect("reopen at v20");
    assert_eq!(user_version(again.conn()), 20);
    assert_eq!(requests_column_names(again.conn()), fresh_columns);
    assert_eq!(count(again.conn(), "requests"), Some(3));
}

#[test]
fn an_injected_failure_after_the_first_alter_rolls_back_columns_and_both_stamps() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Arrange: a v19 file whose connection allows the first `ALTER TABLE
    // requests` and denies every later one.
    let (_dir, path) = temp_db_path();
    file_from(&path, 19, &requests_ddl_through_v19(), &[]);
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
    assert_eq!(user_version(&conn), 19);
    assert_eq!(meta_version(&conn), "19");
    assert_eq!(requests_column_names(&conn), columns_before);

    // Assert: once the fault is gone the same file migrates normally.
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("clear authorizer");
    assert_eq!(
        crate::migrate::migrate_to_current(&conn, 0).expect("retry"),
        SCHEMA_VERSION
    );
    let columns = requests_column_names(&conn);
    assert_eq!(
        &columns[columns.len() - V20_WALK_COLUMNS.len()..],
        V20_WALK_COLUMNS
    );
}

#[test]
fn downgrade_20_to_19_restamps_only_and_a_frozen_v19_insert_then_v20_reopen_keeps_the_row() {
    // Arrange
    let (_dir, path) = temp_db_path();
    fresh_with_rows(&path);
    let before = file_state(&path);
    let fresh_columns = requests_column_names(&Connection::open(&path).expect("reopen"));

    // Act: step down, then a v19 binary writes one row.
    downgrade_to(&path, 19).expect("downgrade");
    let after = file_state(&path);
    insert_as_v19_writer(&Connection::open(&path).expect("reopen"), "v19-row");

    // Assert: the downgrade changed only the two version values.
    assert_eq!(
        after,
        FileState {
            user_version: 19,
            meta_version: "19".to_string(),
            ..before
        }
    );

    // Act: the current binary opens it again.
    let db = open(&path).expect("re-upgrade");

    // Assert: v20 again with the same column list (nothing re-added), both
    // rows present, and the v19-written row NULL in every walk column.
    assert_eq!(user_version(db.conn()), 20);
    assert_eq!(meta_version(db.conn()), "20");
    assert_eq!(requests_column_names(db.conn()), fresh_columns);
    assert_eq!(count(db.conn(), "requests"), Some(2));
    assert_eq!(walk_counts(db.conn(), "v19-row"), vec![None; 5]);
    drop(db);
    downgrade_to(&path, 19).expect("the re-stamped file is still exactly additive");
}

#[test]
fn downgrades_a_v20_file_through_every_step() {
    for target in [19, 18, 17, 16] {
        // Arrange
        let (_dir, path) = temp_db_path();
        fresh_with_rows(&path);
        let before = file_state(&path);

        // Act
        downgrade_to(&path, target).expect("downgrade");

        // Assert: the stamps read the target and nothing else changed.
        assert_eq!(
            file_state(&path),
            FileState {
                user_version: target,
                meta_version: target.to_string(),
                ..before
            },
            "--to {target}"
        );
    }
}

/// One refusal row: the version the file is stamped at, the `requests` DDL,
/// the columns appended to it, and the reason the refusal must name.
type ShapeCase<'a> = (&'a str, i64, &'a str, &'a [&'a str], NotAdditiveReason);

#[test]
fn refuses_a_file_whose_requests_is_not_exactly_the_additive_v20_shape() {
    use NotAdditiveReason::{ColumnCount, ColumnShape};
    let tail = v20_tail();
    let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
    let through_v19 = requests_ddl_through_v19();
    let with_extra: Vec<&str> = tail.iter().copied().chain(["extra_col INTEGER"]).collect();
    let mut swapped = tail.clone();
    swapped.swap(0, 1);
    let excluded_model = "    excluded_model INTEGER,";
    let retyped = rewrite_once(
        CREATE_REQUESTS_TABLE,
        excluded_model,
        "    excluded_model TEXT,",
    );
    let defaulted = rewrite_once(
        CREATE_REQUESTS_TABLE,
        excluded_model,
        "    excluded_model INTEGER DEFAULT 0,",
    );
    let cases: &[ShapeCase<'_>] = &[
        (
            "an extra unknown column",
            20,
            &through_v19,
            &with_extra,
            ColumnCount,
        ),
        (
            "a missing walk column",
            20,
            &through_v19,
            &tail[..4],
            ColumnCount,
        ),
        ("a retyped walk column", 20, &retyped, &[], ColumnShape),
        (
            "a walk column with a default",
            20,
            &defaulted,
            &[],
            ColumnShape,
        ),
        (
            "the walk columns out of order",
            20,
            &through_v19,
            &swapped,
            ColumnShape,
        ),
        (
            "v20 stamped without the walk columns",
            20,
            &through_v19,
            &[],
            ColumnCount,
        ),
        (
            "stamped 19, an extra unknown column",
            19,
            &through_v19,
            &with_extra,
            ColumnCount,
        ),
        (
            "stamped 19, a retyped walk column",
            19,
            &retyped,
            &[],
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
fn the_exact_additive_v20_shape_is_accepted_by_the_shape_fixture() {
    // Positive controls for the refusal table: the same builder with the real
    // DDL, and with the v19 DDL plus the exact tail, downgrade all the way; a
    // v19 file with or without the tail does too.
    let tail = v20_tail();
    let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
    let through_v19 = requests_ddl_through_v19();
    let cases: &[(&str, i64, &str, &[&str])] = &[
        ("the real v20 DDL", 20, CREATE_REQUESTS_TABLE, &[]),
        ("v19 DDL plus the appended tail", 20, &through_v19, &tail),
        ("stamped 19 with the appended tail", 19, &through_v19, &tail),
        ("stamped 19 without the tail", 19, &through_v19, &[]),
    ];
    for &(name, version, ddl, added) in cases {
        for target in (16..version).rev() {
            let (_dir, path) = temp_db_path();
            file_from(&path, version, ddl, added);

            downgrade_to(&path, target).unwrap_or_else(|e| panic!("{name} --to {target}: {e:?}"));
            assert_eq!(
                file_state(&path).user_version,
                target,
                "{name} --to {target}"
            );
        }
    }
}
