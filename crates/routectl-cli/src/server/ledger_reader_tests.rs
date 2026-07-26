use std::time::{Duration, Instant};

use routectl_usage::open;
use rusqlite::params;
use tempfile::TempDir;

use super::*;

const CAT: u32 = 1;
const OV: u64 = 0;

/// Insert a tombstone boundary row stamped with a specific revision.
fn seed_tombstone(
    conn: &rusqlite::Connection,
    ts: i64,
    catalog_version: i64,
    overlay_revision: i64,
) {
    conn.execute(
        "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
         tier, evidence_class, upstream_token, catalog_version, overlay_revision) \
         VALUES (?1, '', '', 'tombstone', '', '', '', NULL, NULL, ?2, ?3)",
        params![ts, catalog_version, overlay_revision],
    )
    .expect("seed tombstone");
}

/// Insert one non-tombstone capability event stamped with a revision.
#[allow(clippy::too_many_arguments)]
fn seed_event(
    conn: &rusqlite::Connection,
    ts: i64,
    lane_key: &str,
    capability: &str,
    verdict: &str,
    phase: &str,
    source: &str,
    tier: &str,
    catalog_version: i64,
    overlay_revision: i64,
) {
    conn.execute(
        "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
         tier, evidence_class, upstream_token, catalog_version, overlay_revision) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, ?9)",
        params![
            ts,
            lane_key,
            capability,
            verdict,
            phase,
            source,
            tier,
            catalog_version,
            overlay_revision
        ],
    )
    .expect("seed capability event");
}

#[test]
fn map_instant_clamps_future_and_ancient_and_maps_recent_past() {
    let now = Instant::now();
    let now_ms = 1_000_000i64;

    // A future-dated row clamps to now (age saturates to zero, never
    // underflowing the u64 duration).
    assert_eq!(map_instant(now, now_ms, now_ms + 5_000), now);
    // An extreme future timestamp cannot underflow the age computation either.
    assert_eq!(map_instant(now, now_ms, i64::MAX), now);

    // A recent past event maps to exactly now - age.
    assert_eq!(
        now.duration_since(map_instant(now, now_ms, now_ms - 3_000)),
        Duration::from_secs(3),
    );

    // An ancient event (far older than any decay window) maps to a far-past
    // instant -- already expired. The saturating age never panics the
    // instant subtraction.
    let ancient = map_instant(now, now_ms, i64::MIN);
    assert!(
        ancient < now,
        "an ancient event maps to a past (already-expired) instant"
    );
}

#[test]
fn classify_boundary_cold_on_absent_ledger() {
    let tmp = TempDir::new().expect("tempdir");
    let absent = tmp.path().join("absent.db");

    assert!(
        matches!(classify_boundary(&absent, CAT, OV), BoundaryOutcome::Cold),
        "an absent ledger is the legitimately-cold case, not unreadable"
    );
}

#[test]
fn classify_boundary_unreadable_on_junk_file() {
    let tmp = TempDir::new().expect("tempdir");
    let ledger = tmp.path().join("usage.db");
    // A non-DB file exists, so the read-only open clears the existence probe
    // and fails on a PRAGMA -- a genuine read failure, not an empty ledger.
    std::fs::write(&ledger, b"this is not a sqlite database").expect("write junk");

    assert!(
        matches!(
            classify_boundary(&ledger, CAT, OV),
            BoundaryOutcome::Unreadable(_)
        ),
        "an unreadable ledger classifies as Unreadable with a path-free class"
    );
}

#[test]
fn classify_boundary_no_tombstone_on_empty_ledger() {
    let tmp = TempDir::new().expect("tempdir");
    let ledger = tmp.path().join("usage.db");
    // Open (creates the schema) but leave it empty -- readable, no tombstone.
    let _db = open(&ledger).expect("open ledger");
    drop(_db);

    assert!(
        matches!(
            classify_boundary(&ledger, CAT, OV),
            BoundaryOutcome::NoTombstone
        ),
        "a readable ledger with no tombstone classifies as NoTombstone"
    );
}

#[test]
fn classify_boundary_revision_mismatch_on_foreign_tombstone() {
    let tmp = TempDir::new().expect("tempdir");
    let ledger = tmp.path().join("usage.db");
    let db = open(&ledger).expect("open ledger");
    // A tombstone stamped a DIFFERENT overlay revision than the target.
    seed_tombstone(db.conn(), 100, i64::from(CAT), 99);
    drop(db);

    assert!(
        matches!(
            classify_boundary(&ledger, CAT, OV),
            BoundaryOutcome::RevisionMismatch
        ),
        "a tombstone at a foreign revision classifies as RevisionMismatch"
    );
}

#[test]
fn classify_boundary_replays_and_reader_maps_a_matching_slice() {
    let tmp = TempDir::new().expect("tempdir");
    let ledger = tmp.path().join("usage.db");
    let db = open(&ledger).expect("open ledger");
    seed_tombstone(db.conn(), 100, i64::from(CAT), i64::try_from(OV).unwrap());
    seed_event(
        db.conn(),
        200,
        "gpt-nick",
        "web_search",
        "broken",
        "f1",
        "live",
        "self-identifying",
        i64::from(CAT),
        i64::try_from(OV).unwrap(),
    );
    drop(db);

    let BoundaryOutcome::Replay(tombstone) = classify_boundary(&ledger, CAT, OV) else {
        panic!("a matching tombstone must classify as Replay");
    };

    // The reader hands the post-boundary row to the replayer, mapped onto the
    // pinned clock. read_events opens read-only and never mutates the ledger.
    let before = std::fs::read(&ledger).expect("read ledger bytes");
    let reader = LedgerCapabilityReader::new(ledger.clone(), tombstone);
    let rows = reader.read_events();
    let after = std::fs::read(&ledger).expect("read ledger bytes");

    assert_eq!(rows.len(), 1, "the single post-boundary row is read");
    assert_eq!(rows[0].state_key, "gpt-nick");
    assert_eq!(rows[0].capability, "web_search");
    assert_eq!(reader.loaded_rows(), 1);
    assert!(
        rows[0].observed_at <= reader.now(),
        "the mapped instant is anchored on the reader's pinned now"
    );
    assert_eq!(before, after, "read_events must not mutate the ledger");
}

#[test]
fn open_error_class_is_path_free_for_every_variant() {
    // Every class token is a fixed discriminant, never a path. The
    // path-bearing variants (Display embeds the DB path) must still map to a
    // clean token.
    let cases = [
        open_error_class(&OpenError::Open {
            path: "/secret/usage.db".into(),
            source: rusqlite::Error::QueryReturnedNoRows,
        }),
        open_error_class(&OpenError::CreateDir {
            path: "/secret/dir".into(),
            source: std::io::Error::other("x"),
        }),
    ];
    for token in cases {
        assert!(
            !token.contains('/') && !token.contains("secret"),
            "class token must be path-free: {token}"
        );
    }
    assert_eq!(
        open_error_class(&OpenError::Open {
            path: "/secret/usage.db".into(),
            source: rusqlite::Error::QueryReturnedNoRows,
        }),
        "open"
    );
}
