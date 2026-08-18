//! Tests for the shared migrating open the bootstrap warms perform.

use routectl_usage::{SCHEMA_VERSION, open_readonly};
use tempfile::TempDir;

use super::*;

const WARM: &str = "test_warm";
const CONSEQUENCE: &str = "nothing happens";

/// A ledger left at an OLDER schema version is exactly the silently-zero-rows
/// case: the read-only open a warm's query needs rejects it outright. The
/// migrating open must bring it forward so the read can proceed.
#[test]
fn an_older_schema_is_migrated_forward_so_a_read_only_open_succeeds() {
    // Arrange: a real ledger, rolled back to one version behind current.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = routectl_usage::open(&path).expect("open");
    db.conn()
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION - 1))
        .expect("roll the schema version back");
    drop(db);
    assert!(
        open_readonly(&path).is_err(),
        "sanity: a read-only open must refuse the older schema"
    );

    // Act
    let proceed = migrate_before_warm(&path, WARM, CONSEQUENCE);

    // Assert
    assert!(proceed, "a migratable ledger must let the warm proceed");
    assert!(
        open_readonly(&path).is_ok(),
        "the migrating open must leave a ledger the warm's read-only open accepts"
    );
}

/// An absent ledger is the cold start, not a failure: the migrating open
/// creates it at the current schema and the warm proceeds to read nothing.
#[test]
fn an_absent_ledger_is_created_and_the_warm_proceeds() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("absent.db");

    let proceed = migrate_before_warm(&path, WARM, CONSEQUENCE);

    assert!(proceed);
    assert!(open_readonly(&path).is_ok());
}

/// An unmigratable file skips the warm at `debug` and never fails bootstrap;
/// the writer reports the same failure at `error` from the surface that owns
/// the ledger's health.
#[test]
fn an_unmigratable_ledger_skips_the_warm_without_warning() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    std::fs::write(&path, b"this is not a sqlite database").expect("write junk");

    let mut proceed = true;
    let events = routectl_testkit::capture_events(|| {
        proceed = migrate_before_warm(&path, WARM, CONSEQUENCE);
    });

    assert!(!proceed, "an unmigratable ledger must skip the warm");
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "the writer owns the loud report; this site must not double-report it"
    );
    let debug = events
        .iter()
        .find(|e| e.level == tracing::Level::DEBUG)
        .expect("the skip must be observable at debug");
    assert_eq!(debug.field("warm"), Some(WARM));
    assert_eq!(debug.field("consequence"), Some(CONSEQUENCE));
}
