//! Tests for the `capability_events` row, insert, and tombstone
//! constructor.

use super::*;
use crate::db::open;
use tempfile::TempDir;

fn open_db() -> (TempDir, crate::db::UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = open(&path).expect("open");
    (dir, db)
}

fn event(lane_key: &str, capability: &str) -> CapabilityEvent {
    CapabilityEvent {
        ts: 123,
        lane_key: lane_key.to_string(),
        capability: capability.to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "inferred".to_string(),
        evidence_class: Some("param_rejected".to_string()),
        upstream_token: Some("thinking".to_string()),
        catalog_version: 7,
        overlay_revision: 3,
    }
}

#[test]
fn insert_binds_every_column() {
    // Arrange
    let (_dir, db) = open_db();

    // Act
    let inserted = insert_capability_event(db.conn(), &event("gpt-nick", "web_search"))
        .expect("insert capability event");

    // Assert: one row, every column reads back exactly what was bound.
    assert_eq!(inserted, 1);
    let (ts, lane_key, capability, verdict, phase, source, tier): (
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = db
        .conn()
        .query_row(
            "SELECT ts, lane_key, capability, verdict, phase, source, tier \
             FROM capability_events",
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
        .expect("token columns");
    let (evidence_class, upstream_token, catalog_version, overlay_revision): (
        Option<String>,
        Option<String>,
        i64,
        i64,
    ) = db
        .conn()
        .query_row(
            "SELECT evidence_class, upstream_token, catalog_version, overlay_revision \
             FROM capability_events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("stamp columns");
    assert_eq!(ts, 123);
    assert_eq!(lane_key, "gpt-nick");
    assert_eq!(capability, "web_search");
    assert_eq!(verdict, "broken");
    assert_eq!(phase, "f1");
    assert_eq!(source, "live");
    assert_eq!(tier, "inferred");
    assert_eq!(evidence_class, Some("param_rejected".to_string()));
    assert_eq!(upstream_token, Some("thinking".to_string()));
    assert_eq!(catalog_version, 7);
    assert_eq!(overlay_revision, 3);
}

#[test]
fn insert_is_append_only() {
    // Arrange
    let (_dir, db) = open_db();

    // Act: the same logical event inserted twice yields two distinct rows.
    insert_capability_event(db.conn(), &event("gpt-nick", "web_search")).expect("first");
    insert_capability_event(db.conn(), &event("gpt-nick", "web_search")).expect("second");

    // Assert
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM capability_events", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 2);
}

#[test]
fn optional_columns_persist_null() {
    // Arrange
    let (_dir, db) = open_db();
    let mut e = event("gpt-nick", "web_search");
    e.evidence_class = None;
    e.upstream_token = None;

    // Act
    insert_capability_event(db.conn(), &e).expect("insert");

    // Assert: the two nullable columns round-trip as SQL NULL.
    let (evidence_class, upstream_token): (Option<String>, Option<String>) = db
        .conn()
        .query_row(
            "SELECT evidence_class, upstream_token FROM capability_events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row");
    assert_eq!(evidence_class, None);
    assert_eq!(upstream_token, None);
}

#[test]
fn tombstone_carries_boundary_revision_and_empty_keys() {
    // Arrange
    let (_dir, db) = open_db();
    let stone = CapabilityEvent::tombstone(999, 12, 4);

    // Act
    insert_capability_event(db.conn(), &stone).expect("insert tombstone");

    // Assert: tombstone verdict, empty lane / capability, stamped revision.
    let (ts, lane_key, capability, verdict, catalog_version, overlay_revision): (
        i64,
        String,
        String,
        String,
        i64,
        i64,
    ) = db
        .conn()
        .query_row(
            "SELECT ts, lane_key, capability, verdict, catalog_version, overlay_revision \
             FROM capability_events",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("row");
    assert_eq!(ts, 999);
    assert_eq!(lane_key, "");
    assert_eq!(capability, "");
    assert_eq!(verdict, TOMBSTONE_VERDICT);
    assert_eq!(catalog_version, 12);
    assert_eq!(overlay_revision, 4);
}

#[test]
fn batch_insert_commits_every_row_in_one_transaction() {
    // Arrange
    let (_dir, db) = open_db();
    let batch = vec![
        CapabilityEvent::tombstone(500, 8, 1),
        event(
            "nick",
            &format!("{}{}", "fie", "ld:thinking.enabled.display"),
        ),
    ];

    // Act
    let inserted =
        insert_capability_events_atomic(db.conn(), &batch).expect("atomic batch commits");

    // Assert: both rows landed, and the tombstone's rowid precedes the
    // restatement's -- append order is the boundary contract.
    assert_eq!(inserted, 2);
    let rowids: Vec<(i64, String)> = db
        .conn()
        .prepare("SELECT rowid, verdict FROM capability_events ORDER BY rowid")
        .expect("prepare")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");
    assert_eq!(rowids.len(), 2);
    assert_eq!(rowids[0].1, TOMBSTONE_VERDICT);
    assert_eq!(rowids[1].1, "broken");
    assert!(
        rowids[0].0 < rowids[1].0,
        "the tombstone must be appended before its survivors",
    );
}

/// A later row failing must leave NO row of the batch committed -- the
/// tombstone included. A partially-committed batch is the exact corruption
/// the atomicity exists to prevent: a tombstone with no survivors after it
/// silently evicts every verdict it was supposed to preserve.
#[test]
fn batch_insert_rolls_back_every_row_when_a_later_row_fails() {
    // Arrange: the second row violates a NOT NULL constraint the first does
    // not, so the failure lands after the tombstone already inserted inside
    // the transaction.
    let (_dir, db) = open_db();
    db.conn()
        .execute_batch(
            "CREATE TRIGGER reject_second BEFORE INSERT ON capability_events \
             WHEN NEW.capability = 'poison' \
             BEGIN SELECT RAISE(ABORT, 'forced row failure'); END",
        )
        .expect("install trigger");
    let batch = vec![
        CapabilityEvent::tombstone(500, 8, 1),
        event("nick", "poison"),
    ];

    // Act
    let result = insert_capability_events_atomic(db.conn(), &batch);

    // Assert: the call reports failure and the ledger is untouched.
    assert!(result.is_err(), "a failing row must fail the whole batch");
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM capability_events", [], |r| r.get(0))
        .expect("count");
    assert_eq!(
        count, 0,
        "neither the tombstone nor any survivor may remain committed",
    );
}

/// A positive control for the rollback test above: the same batch minus the
/// poison row commits, proving the trigger is what fails the batch and the
/// fixture is not simply unable to insert anything.
#[test]
fn batch_insert_commits_under_the_same_fixture_without_the_failing_row() {
    let (_dir, db) = open_db();
    db.conn()
        .execute_batch(
            "CREATE TRIGGER reject_second BEFORE INSERT ON capability_events \
             WHEN NEW.capability = 'poison' \
             BEGIN SELECT RAISE(ABORT, 'forced row failure'); END",
        )
        .expect("install trigger");

    let inserted = insert_capability_events_atomic(
        db.conn(),
        &[
            CapabilityEvent::tombstone(500, 8, 1),
            event("nick", "web_search"),
        ],
    )
    .expect("a batch with no poison row commits");

    assert_eq!(inserted, 2);
}

#[test]
fn batch_insert_of_no_rows_commits_nothing_and_reports_zero() {
    let (_dir, db) = open_db();

    let inserted = insert_capability_events_atomic(db.conn(), &[]).expect("empty batch is ok");

    assert_eq!(inserted, 0);
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM capability_events", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 0);
}
