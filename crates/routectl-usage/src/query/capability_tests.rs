//! Tests for the capability-ledger read queries.

use super::*;
use crate::capability_event::{CapabilityEvent, insert_capability_event};
use crate::db::open;
use tempfile::TempDir;

fn open_db() -> (TempDir, crate::db::UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = open(&path).expect("open");
    (dir, db)
}

fn event(ts: i64, capability: &str) -> CapabilityEvent {
    CapabilityEvent {
        ts,
        lane_key: "gpt-nick".to_string(),
        capability: capability.to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "inferred".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: 1,
        overlay_revision: 1,
    }
}

/// Append order -- rowid -- is authoritative, NOT capture time.
///
/// This test previously asserted `ts ASC` ordering; that expectation was
/// wrong. `ts` is a wall-clock stamp, so an NTP correction or a DST-adjacent
/// system-clock rollback can make a LATER-appended event carry an EARLIER
/// `ts`. Replay applies state transitions (a negative, then its clear) in the
/// order it receives them, so ordering by `ts` can invert a settled pair and
/// resurrect a cleared negative. `rowid` is monotonic per append and cannot
/// invert. Timestamps remain decay-age input; they never decide precedence.
#[test]
fn read_after_orders_by_rowid_not_by_capture_time() {
    // Arrange: insert with DECREASING ts, so ts order and append order
    // disagree on every pair.
    let (_dir, db) = open_db();
    insert_capability_event(db.conn(), &event(30, "first")).expect("first");
    insert_capability_event(db.conn(), &event(20, "second")).expect("second");
    insert_capability_event(db.conn(), &event(10, "third")).expect("third");

    // Act
    let rows = read_capability_events_after(db.conn(), 0, 100).expect("read");

    // Assert: append order, which here is the exact REVERSE of ts order --
    // so a ts-ordered implementation cannot also pass this.
    let caps: Vec<String> = rows.iter().map(|r| r.capability.clone().unwrap()).collect();
    assert_eq!(caps, vec!["first", "second", "third"]);
    let rowids: Vec<i64> = rows.iter().map(|r| r.rowid).collect();
    assert!(
        rowids.windows(2).all(|w| w[0] < w[1]),
        "rowids must ascend: {rowids:?}",
    );
}

/// A system-clock rollback must not invert a settled negative-then-cleared
/// pair. Ordering by `ts` would replay the clear FIRST (it carries the
/// earlier stamp), leaving the negative resident -- resurrecting a verdict
/// the clear had settled.
#[test]
fn a_clock_rollback_between_two_appends_does_not_invert_them() {
    // Arrange: the negative is appended first at ts=5000; the clear is
    // appended second but stamped ts=1000 (the clock went backwards).
    let (_dir, db) = open_db();
    insert_capability_event(db.conn(), &event(5_000, "web_search")).expect("negative");
    let mut clear = event(1_000, "web_search");
    clear.verdict = "cleared".to_string();
    insert_capability_event(db.conn(), &clear).expect("clear");

    // Act
    let rows = read_capability_events_after(db.conn(), 0, 100).expect("read");

    // Assert: the clear is replayed LAST, as appended.
    let verdicts: Vec<String> = rows.iter().map(|r| r.verdict.clone().unwrap()).collect();
    assert_eq!(verdicts, vec!["broken", "cleared"]);
}

/// Over the row cap the window must be the NEWEST rows, not the oldest.
///
/// The cap exists to bound the read, and the newest rows are the ones that
/// describe current state: taking the oldest window under a dense ledger
/// would replay ancient history and silently omit every recent verdict --
/// including the survivor restatements a boundary batch just appended.
#[test]
fn over_the_cap_the_newest_rows_are_returned_ascending() {
    // Arrange: 10 appends, cap of 3.
    let (_dir, db) = open_db();
    for i in 1..=10 {
        insert_capability_event(db.conn(), &event(i, &format!("c{i}"))).expect("insert");
    }

    // Act
    let rows = read_capability_events_after(db.conn(), 0, 3).expect("read");

    // Assert: the last three appended, still oldest-first among themselves.
    let caps: Vec<String> = rows.iter().map(|r| r.capability.clone().unwrap()).collect();
    assert_eq!(caps, vec!["c8", "c9", "c10"]);
}

/// The positive control for the cap test: under the cap, every row is
/// returned -- so the newest-window behavior above is the cap acting, not the
/// query dropping rows unconditionally.
#[test]
fn under_the_cap_every_row_is_returned_ascending() {
    let (_dir, db) = open_db();
    for i in 1..=3 {
        insert_capability_event(db.conn(), &event(i, &format!("c{i}"))).expect("insert");
    }

    let rows = read_capability_events_after(db.conn(), 0, 10).expect("read");

    let caps: Vec<String> = rows.iter().map(|r| r.capability.clone().unwrap()).collect();
    assert_eq!(caps, vec!["c1", "c2", "c3"]);
}

/// The newest window is taken from rows after the boundary, never across it:
/// the cap must not pull in pre-boundary rows to fill its quota.
#[test]
fn the_newest_window_never_reaches_back_across_the_boundary() {
    // Arrange: 6 rows; the boundary sits at the 4th rowid, so only 2 rows
    // are eligible even though the cap asks for 5.
    let (_dir, db) = open_db();
    for i in 1..=6 {
        insert_capability_event(db.conn(), &event(i, &format!("c{i}"))).expect("insert");
    }
    let boundary: i64 = db
        .conn()
        .query_row(
            "SELECT rowid FROM capability_events ORDER BY rowid LIMIT 1 OFFSET 3",
            [],
            |r| r.get(0),
        )
        .expect("boundary rowid");

    // Act
    let rows = read_capability_events_after(db.conn(), boundary, 5).expect("read");

    // Assert: exactly the two post-boundary rows.
    let caps: Vec<String> = rows.iter().map(|r| r.capability.clone().unwrap()).collect();
    assert_eq!(caps, vec!["c5", "c6"]);
}

#[test]
fn read_after_filters_by_rowid() {
    // Arrange
    let (_dir, db) = open_db();
    for (i, cap) in ["a", "b", "c"].iter().enumerate() {
        insert_capability_event(db.conn(), &event(10 + i as i64, cap)).expect("insert");
    }

    // Act: skip the first row (rowid 1).
    let rows = read_capability_events_after(db.conn(), 1, 100).expect("read");

    // Assert: only rows after rowid 1 come back.
    let caps: Vec<String> = rows.iter().map(|r| r.capability.clone().unwrap()).collect();
    assert_eq!(caps, vec!["b", "c"]);
    assert!(rows.iter().all(|r| r.rowid > 1));
}

#[test]
fn read_after_caps_at_limit() {
    // Arrange
    let (_dir, db) = open_db();
    for i in 0..10 {
        insert_capability_event(db.conn(), &event(i, &format!("c{i}"))).expect("insert");
    }

    // Act
    let rows = read_capability_events_after(db.conn(), 0, 3).expect("read");

    // Assert: capped, and the cap keeps the NEWEST rows -- the ones that
    // describe current state. (This assertion previously expected the oldest
    // window; that expectation was wrong, and the dedicated cap tests above
    // carry the reasoning.)
    assert_eq!(rows.len(), 3);
    let caps: Vec<String> = rows.iter().map(|r| r.capability.clone().unwrap()).collect();
    assert_eq!(caps, vec!["c7", "c8", "c9"]);
}

#[test]
fn read_after_maps_all_columns() {
    // Arrange
    let (_dir, db) = open_db();
    let mut e = event(42, "web_search");
    e.evidence_class = Some("param_rejected".to_string());
    e.upstream_token = Some("thinking".to_string());
    e.catalog_version = 7;
    e.overlay_revision = 3;
    insert_capability_event(db.conn(), &e).expect("insert");

    // Act
    let rows = read_capability_events_after(db.conn(), 0, 100).expect("read");

    // Assert
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.ts, 42);
    assert_eq!(row.lane_key.as_deref(), Some("gpt-nick"));
    assert_eq!(row.capability.as_deref(), Some("web_search"));
    assert_eq!(row.verdict.as_deref(), Some("broken"));
    assert_eq!(row.phase.as_deref(), Some("f1"));
    assert_eq!(row.source.as_deref(), Some("live"));
    assert_eq!(row.tier.as_deref(), Some("inferred"));
    assert_eq!(row.evidence_class.as_deref(), Some("param_rejected"));
    assert_eq!(row.upstream_token.as_deref(), Some("thinking"));
    assert_eq!(row.catalog_version, Some(7));
    assert_eq!(row.overlay_revision, Some(3));
}

#[test]
fn latest_tombstone_none_on_empty_ledger() {
    // Arrange
    let (_dir, db) = open_db();

    // Act + Assert
    assert_eq!(latest_tombstone(db.conn()).expect("query"), None);
}

#[test]
fn latest_tombstone_none_when_only_events() {
    // Arrange: events but no tombstone.
    let (_dir, db) = open_db();
    insert_capability_event(db.conn(), &event(10, "a")).expect("insert");

    // Act + Assert
    assert_eq!(latest_tombstone(db.conn()).expect("query"), None);
}

#[test]
fn latest_tombstone_returns_highest_rowid() {
    // Arrange: two tombstones stamped at different revisions, plus an event
    // inserted between them.
    let (_dir, db) = open_db();
    insert_capability_event(db.conn(), &CapabilityEvent::tombstone(1, 5, 1)).expect("first");
    insert_capability_event(db.conn(), &event(2, "a")).expect("event");
    insert_capability_event(db.conn(), &CapabilityEvent::tombstone(3, 9, 2)).expect("second");

    // Act
    let stone = latest_tombstone(db.conn()).expect("query").expect("some");

    // Assert: the highest-rowid tombstone, with its stamped revision.
    assert_eq!(stone.rowid, 3);
    assert_eq!(stone.catalog_version, Some(9));
    assert_eq!(stone.overlay_revision, Some(2));
}
