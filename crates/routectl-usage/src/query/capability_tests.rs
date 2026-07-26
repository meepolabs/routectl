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

#[test]
fn read_after_orders_by_ts_then_rowid() {
    // Arrange: insert out of ts order, plus a ts collision to exercise the
    // rowid tie-break.
    let (_dir, db) = open_db();
    insert_capability_event(db.conn(), &event(30, "c")).expect("c");
    insert_capability_event(db.conn(), &event(10, "a")).expect("a");
    insert_capability_event(db.conn(), &event(10, "b")).expect("b"); // same ts as a, later rowid
    insert_capability_event(db.conn(), &event(20, "d")).expect("d");

    // Act
    let rows = read_capability_events_after(db.conn(), 0, 100).expect("read");

    // Assert: ts ascending, and the ts=10 pair tie-breaks by insertion order.
    let caps: Vec<String> = rows.iter().map(|r| r.capability.clone().unwrap()).collect();
    assert_eq!(caps, vec!["a", "b", "d", "c"]);
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

    // Assert: capped, and the cap keeps the oldest (ts-ascending) rows.
    assert_eq!(rows.len(), 3);
    let caps: Vec<String> = rows.iter().map(|r| r.capability.clone().unwrap()).collect();
    assert_eq!(caps, vec!["c0", "c1", "c2"]);
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
