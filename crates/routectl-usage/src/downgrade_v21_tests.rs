//! Tests for the 21 -> 20 step and chaining it into the older steps.

use rusqlite::Connection;

use super::tests::{meta_version, rewrite_once, temp_db_path, user_version};
use super::v18_tests::{
    FileState, count, file_state, fresh_with_rows, insert_through, requests_column_names,
};
use super::v19_tests::file_from;
use super::v20_tests::{
    V20_WALK_COLUMNS, insert_capability_event, v19_insert_columns, walk_counts,
};
use super::*;
use crate::db::open;
use crate::record::UsageRecord;
use crate::schema::{CREATE_REQUESTS_TABLE, SCHEMA_VERSION};
use crate::writer::UsageWriter;
use std::collections::BTreeMap;

/// The two v21 cache-write columns, frozen as text in physical order.
const V21_CACHE_WRITE_COLUMNS: [&str; 2] = ["cache_write_tokens", "cache_write_lifetimes"];

/// The column list a v20 writer's `INSERT` names: the v19 list followed by
/// the five walk counts. A v20 binary handed a downgraded file writes through
/// exactly this list.
fn insert_as_v20_writer(conn: &Connection, request_id: &str) {
    let columns = format!("{},\n{}", v19_insert_columns(), V20_WALK_COLUMNS.join(", "));
    insert_through(conn, &columns, request_id);
}

/// `(cache_write_5m, cache_write_tokens, cache_write_lifetimes)` of one row.
type CacheWrites = (Option<i64>, Option<i64>, Option<String>);

fn cache_writes(conn: &Connection, request_id: &str) -> CacheWrites {
    conn.query_row(
        "SELECT cache_write_5m, cache_write_tokens, cache_write_lifetimes \
         FROM requests WHERE request_id = ?1",
        [request_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .expect("cache writes")
}

/// The current `requests` DDL cut after its v20 set, so the v21 tail can be
/// appended by `ALTER TABLE` in a chosen shape.
fn requests_ddl_through_v20() -> String {
    const LAST_V20_COLUMN: &str = "    excluded_probe_run INTEGER,";
    let (head, tail) = CREATE_REQUESTS_TABLE
        .split_once(LAST_V20_COLUMN)
        .expect("the DDL still ends its v20 set with excluded_probe_run");
    for column in V21_CACHE_WRITE_COLUMNS {
        assert!(tail.contains(column), "{column} must be in the v21 tail");
    }
    format!("{head}    excluded_probe_run INTEGER\n)")
}

/// The two v21 columns as `ALTER TABLE ... ADD COLUMN` definitions.
const V21_TAIL: [&str; 2] = ["cache_write_tokens INTEGER", "cache_write_lifetimes TEXT"];

/// A fresh current file whose one row the current writer wrote with a
/// cache-write total and split.
fn fresh_with_a_cache_write(path: &std::path::Path) {
    fresh_with_rows(path);
    let (handle, writer) = UsageWriter::start(path.to_path_buf(), 4, 0, true);
    handle.try_send(UsageRecord {
        request_id: "v21-row".to_string(),
        cache_write_tokens: Some(2523),
        cache_write_lifetimes: Some(BTreeMap::from([(300, 2523), (3600, 0)])),
        ..UsageRecord::default()
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while writer.counters().persisted() < 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(writer.counters().persisted(), 1, "rows persisted");
    drop(handle);
    writer.shutdown();
}

const V21_ROW: (Option<i64>, Option<i64>, &str) = (None, Some(2523), r#"{"300":2523,"3600":0}"#);

fn v21_row_values() -> CacheWrites {
    (V21_ROW.0, V21_ROW.1, Some(V21_ROW.2.to_string()))
}

#[test]
fn downgrade_21_to_20_restamps_only_and_a_frozen_v20_insert_then_v21_reopen_reads_the_writes() {
    // Arrange
    let (_dir, path) = temp_db_path();
    fresh_with_a_cache_write(&path);
    insert_capability_event(&Connection::open(&path).expect("reopen"));
    let before = file_state(&path);
    assert_eq!(before.requests, 2, "the seeded row and the v21 row");
    let fresh_columns = requests_column_names(&Connection::open(&path).expect("reopen"));

    // Act: step down, then a v20 binary writes one row.
    downgrade_to(&path, 20).expect("downgrade");
    let after = file_state(&path);
    insert_as_v20_writer(&Connection::open(&path).expect("reopen"), "v20-row");

    // Assert: the downgrade changed only the two version values.
    assert_eq!(
        after,
        FileState {
            user_version: 20,
            meta_version: "20".to_string(),
            ..before
        }
    );

    // Act: the current binary opens it again.
    let db = open(&path).expect("re-upgrade");

    // Assert: current again with the same column list (nothing re-added),
    // the pre-downgrade cache writes read back unchanged, and the
    // v20-written row NULL in both v21 columns.
    assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
    assert_eq!(meta_version(db.conn()), SCHEMA_VERSION.to_string());
    assert_eq!(requests_column_names(db.conn()), fresh_columns);
    assert_eq!(count(db.conn(), "requests"), Some(3));
    assert_eq!(cache_writes(db.conn(), "v21-row"), v21_row_values());
    assert_eq!(cache_writes(db.conn(), "v20-row"), (None, None, None));
    assert_eq!(walk_counts(db.conn(), "v20-row"), vec![None; 5]);
    drop(db);
    downgrade_to(&path, 20).expect("the re-stamped file is still exactly additive");
}

#[test]
fn downgrades_a_v21_file_through_every_step() {
    for target in [20, 19, 18, 17, 16] {
        // Arrange
        let (_dir, path) = temp_db_path();
        fresh_with_a_cache_write(&path);
        let before = file_state(&path);

        // Act
        downgrade_to(&path, target).expect("downgrade");

        // Assert: the stamps read the target, nothing else changed, and the
        // v21 values are still on disk.
        assert_eq!(
            file_state(&path),
            FileState {
                user_version: target,
                meta_version: target.to_string(),
                ..before
            },
            "--to {target}"
        );
        assert_eq!(
            cache_writes(&Connection::open(&path).expect("reopen"), "v21-row"),
            v21_row_values(),
            "--to {target}"
        );
    }
}

/// One refusal row: the version the file is stamped at, the `requests` DDL,
/// the columns appended to it, and the reason the refusal must name.
type ShapeCase<'a> = (&'a str, i64, &'a str, &'a [&'a str], NotAdditiveReason);

#[test]
fn refuses_a_file_whose_requests_is_not_exactly_the_additive_v21_shape() {
    use NotAdditiveReason::{ColumnCount, ColumnShape};
    let through_v20 = requests_ddl_through_v20();
    let with_extra: Vec<&str> = V21_TAIL
        .iter()
        .copied()
        .chain(["extra_col INTEGER"])
        .collect();
    let swapped = [V21_TAIL[1], V21_TAIL[0]];
    let retyped_tail = ["cache_write_tokens INTEGER", "cache_write_lifetimes BLOB"];
    let tokens = "    cache_write_tokens INTEGER,";
    let retyped = rewrite_once(
        CREATE_REQUESTS_TABLE,
        tokens,
        "    cache_write_tokens TEXT,",
    );
    let defaulted = rewrite_once(
        CREATE_REQUESTS_TABLE,
        tokens,
        "    cache_write_tokens INTEGER DEFAULT 0,",
    );
    let cases: &[ShapeCase<'_>] = &[
        (
            "an extra unknown column",
            21,
            &through_v20,
            &with_extra,
            ColumnCount,
        ),
        (
            "a missing cache-write column",
            21,
            &through_v20,
            &V21_TAIL[..1],
            ColumnCount,
        ),
        ("a retyped total column", 21, &retyped, &[], ColumnShape),
        (
            "a retyped split column",
            21,
            &through_v20,
            &retyped_tail,
            ColumnShape,
        ),
        (
            "a cache-write column with a default",
            21,
            &defaulted,
            &[],
            ColumnShape,
        ),
        (
            "the cache-write columns out of order",
            21,
            &through_v20,
            &swapped,
            ColumnShape,
        ),
        (
            "stamped 20, an extra unknown column",
            20,
            &through_v20,
            &with_extra,
            ColumnCount,
        ),
        (
            "stamped 20, a retyped total column",
            20,
            &retyped,
            &[],
            ColumnShape,
        ),
        (
            "stamped 19, a retyped total column",
            19,
            &retyped,
            &[],
            ColumnShape,
        ),
        (
            "stamped 18, a retyped total column",
            18,
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
fn the_exact_additive_v21_shape_is_accepted_by_the_shape_fixture() {
    // Positive controls for the refusal table: the same builder with the real
    // DDL, and with the v20 DDL plus the exact tail, downgrade all the way;
    // files stamped 20, 19 and 18 carrying the tail do too.
    let through_v20 = requests_ddl_through_v20();
    let cases: &[(&str, i64, &str, &[&str])] = &[
        ("the real v21 DDL", 21, CREATE_REQUESTS_TABLE, &[]),
        (
            "v20 DDL plus the appended tail",
            21,
            &through_v20,
            &V21_TAIL,
        ),
        (
            "stamped 20 with the appended tail",
            20,
            &through_v20,
            &V21_TAIL,
        ),
        ("stamped 20 without the tail", 20, &through_v20, &[]),
        (
            "stamped 19 with the v21 tail",
            19,
            CREATE_REQUESTS_TABLE,
            &[],
        ),
        (
            "stamped 18 with the v21 tail",
            18,
            CREATE_REQUESTS_TABLE,
            &[],
        ),
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
