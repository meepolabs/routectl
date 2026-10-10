// A database written by an older build can carry `meta` rows under keys this
// build no longer defines. Such a row must not stop the database from opening,
// accepting writes, or answering reads, and nothing may rewrite or remove it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::{UsageRecord, UsageWriter, earliest_ts_start, open, open_readonly, open_rw};

const LEGACY_KEY: &str = "probe_reservation:v1:20260101:616e7468726f706963";
const LEGACY_VALUE: &str = "3";

fn plant_legacy_meta_row(path: &Path) {
    let db = open(path).expect("create database");
    db.conn()
        .execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)",
            [LEGACY_KEY, LEGACY_VALUE],
        )
        .expect("plant legacy meta row");
}

fn legacy_value(path: &Path) -> Option<String> {
    let db = open_readonly(path).expect("readonly open");
    db.conn()
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [LEGACY_KEY],
            |row| row.get(0),
        )
        .ok()
}

fn wait_persisted(counters: &Arc<crate::UsageCounters>, want: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while counters.persisted() < want {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

#[test]
fn a_legacy_meta_row_does_not_block_open_write_or_read() {
    // Arrange
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    plant_legacy_meta_row(&path);
    assert_eq!(legacy_value(&path).as_deref(), Some(LEGACY_VALUE));

    // Act: every open mode, then a write through the production writer.
    open(&path).expect("migrating open");
    open_rw(&path).expect("read-write open");
    let (handle, writer) = UsageWriter::start(path.clone(), 4, 0, true);
    handle.try_send(UsageRecord {
        ts_start: 1_000,
        request_id: "after-legacy-row".to_string(),
        ..UsageRecord::default()
    });
    let persisted = wait_persisted(writer.counters(), 1);
    let counters = Arc::clone(writer.counters());
    writer.shutdown();

    // Assert: the row was written and reads back, and the legacy row is
    // untouched.
    assert!(persisted, "the writer must persist the row");
    assert_eq!(counters.write_errors(), 0);
    let db = open_readonly(&path).expect("readonly open");
    assert_eq!(earliest_ts_start(&db, 0).expect("read"), Some(1_000));
    assert_eq!(legacy_value(&path).as_deref(), Some(LEGACY_VALUE));
}
