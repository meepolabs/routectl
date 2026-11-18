//! Tests for the 22 -> 21 step and chaining it into the older steps.

use rusqlite::Connection;

use super::tests::{meta_version, temp_db_path, user_version};
use super::v18_tests::{FileState, count, file_state, fresh_with_rows, insert_as_v17_writer};
use super::*;
use crate::db::open;
use crate::schema::SCHEMA_VERSION;

/// The three read indexes v22 creates.
const V22_INDEX_NAMES: [&str; 3] = [
    "idx_requests_agg",
    "idx_requests_errors",
    "idx_requests_quota_seat",
];

/// A v22 file as the rollback path leaves it: a fresh current file with rows,
/// stepped down to 22.
fn v22_with_rows(path: &std::path::Path) {
    fresh_with_rows(path);
    downgrade_to(path, 22).expect("step the fresh file down to 22");
}

fn index_names(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND name IN (?1, ?2, ?3) \
         ORDER BY name",
    )
    .expect("prepare")
    .query_map(V22_INDEX_NAMES, |r| r.get(0))
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("collect")
}

#[test]
fn downgrade_22_to_21_restamps_only_and_a_reopen_climbs_back_to_current() {
    // Arrange: a v22 file with a row.
    let (_dir, path) = temp_db_path();
    v22_with_rows(&path);
    let before = file_state(&path);
    assert_eq!(before.user_version, 22, "the fixture starts at v22");

    // Act: step down, then a v21 binary writes one row.
    downgrade_to(&path, 21).expect("downgrade");
    let after = file_state(&path);
    insert_as_v17_writer(&Connection::open(&path).expect("reopen"), "v21-row");

    // Assert: only the two version values changed; the indexes and rows stay.
    assert_eq!(
        after,
        FileState {
            user_version: 21,
            meta_version: "21".to_string(),
            ..before
        }
    );

    // Act: the current binary opens it again.
    let db = open(&path).expect("re-upgrade");

    // Assert: current again, all three indexes, both rows.
    assert_eq!(user_version(db.conn()), SCHEMA_VERSION);
    assert_eq!(meta_version(db.conn()), SCHEMA_VERSION.to_string());
    assert_eq!(index_names(db.conn()), V22_INDEX_NAMES);
    assert_eq!(count(db.conn(), "requests"), Some(2));
    drop(db);
    downgrade_to(&path, 21).expect("the re-stamped file still downgrades");
}

#[test]
fn downgrades_a_v22_file_through_every_step() {
    for target in [21, 20, 19, 18, 17, 16] {
        // Arrange
        let (_dir, path) = temp_db_path();
        v22_with_rows(&path);
        let before = file_state(&path);

        // Act
        downgrade_to(&path, target).expect("downgrade");

        // Assert
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

#[test]
fn refuses_a_v22_file_whose_requests_is_not_the_v21_shape() {
    // Arrange: a v22 file with one extra column appended.
    let (_dir, path) = temp_db_path();
    v22_with_rows(&path);
    Connection::open(&path)
        .expect("reopen")
        .execute_batch("ALTER TABLE requests ADD COLUMN extra_col INTEGER")
        .expect("extra column");
    let before = file_state(&path);

    // Act
    let result = downgrade_to(&path, 21);

    // Assert: refused on `requests`, and the file is exactly as it was.
    assert!(
        matches!(
            result,
            Err(DowngradeError::NotAdditive {
                table: "requests",
                reason: NotAdditiveReason::ColumnCount
            })
        ),
        "{result:?}"
    );
    assert_eq!(file_state(&path), before);
}
