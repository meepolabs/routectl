// A database written by an older build can carry `meta` rows under keys this
// build no longer defines. Such a row must not stop the database from opening,
// accepting writes, or answering reads, and nothing may rewrite or remove it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::{UsageRecord, UsageWriter, earliest_ts_start, open, open_readonly, open_rw};

/// `(key, value)` rows an older build could have left in `meta`. The second is
/// the exact shape the removed paid-probe reservation codec wrote:
/// `paid_probe_reservation:v1:<UTC day index>:<hex-encoded provider name>`,
/// here day 20348 for provider `anthropic`, holding a committed unit count.
const LEGACY_ROWS: [(&str, &str); 2] = [
    ("probe_reservation:v1:20260101:616e7468726f706963", "3"),
    ("paid_probe_reservation:v1:20348:616e7468726f706963", "2"),
];

fn plant_legacy_meta_rows(path: &Path) {
    let db = open(path).expect("create database");
    for row in LEGACY_ROWS {
        db.conn()
            .execute("INSERT INTO meta (key, value) VALUES (?1, ?2)", row)
            .expect("plant legacy meta row");
    }
}

fn legacy_values(path: &Path) -> Vec<Option<String>> {
    let db = open_readonly(path).expect("readonly open");
    LEGACY_ROWS
        .iter()
        .map(|(key, _)| {
            db.conn()
                .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                    row.get(0)
                })
                .ok()
        })
        .collect()
}

fn planted_values() -> Vec<Option<String>> {
    LEGACY_ROWS
        .iter()
        .map(|(_, value)| Some((*value).to_string()))
        .collect()
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
    plant_legacy_meta_rows(&path);
    assert_eq!(legacy_values(&path), planted_values());

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

    // Assert: the row was written and reads back, and the legacy rows are
    // untouched.
    assert!(persisted, "the writer must persist the row");
    assert_eq!(counters.write_errors(), 0);
    let db = open_readonly(&path).expect("readonly open");
    assert_eq!(earliest_ts_start(&db, 0).expect("read"), Some(1_000));
    assert_eq!(legacy_values(&path), planted_values());
}
