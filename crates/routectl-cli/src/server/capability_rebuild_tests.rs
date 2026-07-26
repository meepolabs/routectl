use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::Config;
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter, open};
use rusqlite::params;
use tempfile::TempDir;

use super::*;
use crate::server::build_router_from_config;

/// Build the default-config router this crate ships: baked catalog version,
/// overlay revision zero. Both feed the boot revision the warm stamps.
async fn default_router(tmp: &TempDir) -> Router {
    let mut config = Config::default();
    config.usage.db_path = tmp.path().join("router-usage.db");
    let config = Arc::new(config);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    build_router_from_config(config, secrets)
        .await
        .expect("build router")
}

/// A usage handle backed by a real writer at `path`, enabled so capability
/// events are actually persisted. Returns the owning writer for shutdown.
fn writer_at(path: &std::path::Path) -> (UsageHandle, UsageWriter) {
    UsageWriter::start(path.to_path_buf(), CHANNEL_CAPACITY, 0, true)
}

/// Insert one non-tombstone capability event row directly, so the replay
/// path can be exercised against a real read-only open with precise control
/// over the persisted revision and tokens.
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
            overlay_revision,
        ],
    )
    .expect("seed capability event");
}

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

/// Count the tombstone rows in a ledger.
fn tombstone_count(path: &std::path::Path) -> i64 {
    let db = open(path).expect("open ledger");
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM capability_events WHERE verdict = 'tombstone'",
            [],
            |row| row.get(0),
        )
        .expect("count tombstones")
}

#[tokio::test]
async fn absent_ledger_read_is_silent_and_enqueues_one_boot_tombstone() {
    // Arrange: the read path points at a genuinely absent ledger, while the
    // write handle is backed by a separate scratch DB -- so the read is a
    // clean NoData with no writer racing to create the file underneath it.
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let ledger = tmp.path().join("absent.db");
    let scratch = tmp.path().join("scratch.db");
    let (handle, writer) = writer_at(&scratch);

    // Act: the warm reads an absent ledger (NoData) and must fail closed
    // silently -- no read-failure WARN -- while still enqueuing exactly one
    // fresh tombstone.
    let events = routectl_testkit::capture_events(|| {
        warm_capability_registry_from_ledger(&ledger, &router, &handle);
    });

    // Assert: the absent-ledger read never warns (distinct from the
    // unreadable-ledger path), and nothing replayed.
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "an absent ledger must not emit a read-failure WARN"
    );
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "a cold ledger replays nothing"
    );

    // Drain the writer, then confirm exactly one boot tombstone reached it,
    // stamped this boot's revision.
    drop(handle);
    writer.shutdown();
    assert_eq!(
        tombstone_count(&scratch),
        1,
        "exactly one fresh boot tombstone must reach the writer"
    );
    let db = open(&scratch).expect("reopen ledger");
    let boundary = latest_tombstone(db.conn())
        .expect("read tombstone")
        .expect("a boot tombstone exists");
    assert_eq!(
        boundary.catalog_version,
        Some(i64::from(router.catalog_version()))
    );
    assert_eq!(
        boundary.overlay_revision,
        Some(i64::try_from(router.overlay_revision()).unwrap())
    );
}

#[tokio::test]
async fn boot_tombstone_reaches_writer_through_the_production_seam() {
    // Arrange: the production shape -- the writer is started at the SAME path
    // the warm reads, exactly as the reordered serve seam wires it.
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = writer_at(&ledger);

    // Act
    warm_capability_registry_from_ledger(&ledger, &router, &handle);
    drop(handle);
    writer.shutdown();

    // Assert: exactly one tombstone landed, carrying this boot's revision.
    assert_eq!(tombstone_count(&ledger), 1);
    let db = open(&ledger).expect("reopen ledger");
    let boundary = latest_tombstone(db.conn())
        .expect("read tombstone")
        .expect("a boot tombstone exists");
    assert_eq!(
        boundary.catalog_version,
        Some(i64::from(router.catalog_version()))
    );
}

#[tokio::test]
async fn matching_tombstone_replays_post_boundary_negative() {
    // Arrange: a ledger whose tombstone matches this boot's revision, with a
    // self-identifying broken negative recorded after it.
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let cat = i64::from(router.catalog_version());
    let overlay = i64::try_from(router.overlay_revision()).unwrap();

    let ledger = tmp.path().join("usage.db");
    let db = open(&ledger).expect("open ledger");
    seed_tombstone(db.conn(), 100, cat, overlay);
    seed_event(
        db.conn(),
        200,
        "gpt-nick",
        "web_search",
        "broken",
        "f1",
        "live",
        "self-identifying",
        cat,
        overlay,
    );
    drop(db);

    // A throwaway writer: a matching tombstone replays without enqueuing.
    let scratch = tmp.path().join("scratch.db");
    let (handle, writer) = writer_at(&scratch);

    // Act
    warm_capability_registry_from_ledger(&ledger, &router, &handle);

    // Assert: the negative is resident after warm, under its lane / capability.
    let snapshot = router.learned_capability_snapshot();
    assert!(
        snapshot.iter().any(|e| e.state_key == "gpt-nick"
            && e.feature_key == "web_search"
            && e.verdict.as_str() == "broken"),
        "a post-boundary negative must be replayed and resident after warm"
    );

    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn matching_tombstone_skips_a_stale_revision_straggler() {
    // Arrange: two post-boundary negatives -- one at this boot's revision, one
    // stamped a stale catalog version (an old-router straggler).
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let cat = i64::from(router.catalog_version());
    let overlay = i64::try_from(router.overlay_revision()).unwrap();

    let ledger = tmp.path().join("usage.db");
    let db = open(&ledger).expect("open ledger");
    seed_tombstone(db.conn(), 100, cat, overlay);
    seed_event(
        db.conn(),
        200,
        "lane-current",
        "cap-current",
        "broken",
        "f1",
        "live",
        "self-identifying",
        cat,
        overlay,
    );
    seed_event(
        db.conn(),
        300,
        "lane-stale",
        "cap-stale",
        "broken",
        "f1",
        "live",
        "self-identifying",
        cat + 1,
        overlay,
    );
    drop(db);

    let scratch = tmp.path().join("scratch.db");
    let (handle, writer) = writer_at(&scratch);

    // Act
    warm_capability_registry_from_ledger(&ledger, &router, &handle);

    // Assert: the current-revision negative replays; the stale straggler does not.
    let snapshot = router.learned_capability_snapshot();
    assert!(
        snapshot.iter().any(|e| e.state_key == "lane-current"),
        "the current-revision negative replays"
    );
    assert!(
        !snapshot.iter().any(|e| e.state_key == "lane-stale"),
        "a stale-revision straggler is skipped by the per-row filter"
    );

    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn revision_mismatch_fails_closed_and_writes_a_fresh_tombstone() {
    // Arrange: a ledger whose tombstone + negative were stamped at overlay 0,
    // but this boot runs at a different overlay revision.
    let tmp = TempDir::new().expect("tempdir");
    let mut router = default_router(&tmp).await;
    router.note_overlay_revision(99);
    let cat = i64::from(router.catalog_version());

    let ledger = tmp.path().join("usage.db");
    let db = open(&ledger).expect("open ledger");
    seed_tombstone(db.conn(), 100, cat, 0);
    seed_event(
        db.conn(),
        200,
        "gpt-nick",
        "web_search",
        "broken",
        "f1",
        "live",
        "self-identifying",
        cat,
        0,
    );
    drop(db);

    // The fresh tombstone must reach a writer at the SAME ledger path.
    let (handle, writer) = writer_at(&ledger);

    // Act
    warm_capability_registry_from_ledger(&ledger, &router, &handle);

    // Assert: nothing replayed (fail closed on the revision mismatch).
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "a revision mismatch replays nothing"
    );

    drop(handle);
    writer.shutdown();

    // A second, fresh tombstone was written at this boot's revision.
    assert_eq!(
        tombstone_count(&ledger),
        2,
        "the boot seam adds a fresh tombstone on a revision mismatch"
    );
    let db = open(&ledger).expect("reopen ledger");
    let boundary = latest_tombstone(db.conn())
        .expect("read tombstone")
        .expect("a boot tombstone exists");
    assert_eq!(boundary.overlay_revision, Some(99));
}

#[tokio::test]
async fn unreadable_ledger_leaves_registry_empty_and_warns() {
    // Arrange: a non-DB file at the ledger path -- it exists, so the
    // read-only open clears the existence probe and fails on the first
    // PRAGMA (a genuine read failure, not an empty ledger).
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let ledger = tmp.path().join("usage.db");
    std::fs::write(&ledger, b"this is not a sqlite database").expect("write junk");

    let scratch = tmp.path().join("scratch.db");
    let (handle, writer) = writer_at(&scratch);

    // Act
    let events = routectl_testkit::capture_events(|| {
        warm_capability_registry_from_ledger(&ledger, &router, &handle);
    });

    // Assert: a WARN fired, the registry stayed empty, and boot did not panic.
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN),
        "an unreadable ledger must emit a read-failure WARN"
    );
    assert!(router.learned_capability_snapshot().is_empty());

    drop(handle);
    writer.shutdown();
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
    // instant -- already expired, so it lapses to a single re-probe rather
    // than acting. The saturating age never panics the instant subtraction.
    let ancient = map_instant(now, now_ms, i64::MIN);
    assert!(
        ancient < now,
        "an ancient event maps to a past (already-expired) instant"
    );
}

#[test]
fn rebuild_log_warns_when_row_cap_hit() {
    let summary = CapabilityRebuildSummary {
        replayed_negative: 3,
        ..CapabilityRebuildSummary::default()
    };
    let events = routectl_testkit::capture_events(|| {
        emit_rebuild_log(&summary, REBUILD_ROW_LIMIT);
    });

    let warn = events
        .iter()
        .find(|e| e.level == tracing::Level::WARN)
        .expect("cap-hit warning emitted");
    assert!(warn.message.contains("hit the row cap"));
    let info = events
        .iter()
        .find(|e| e.level == tracing::Level::INFO)
        .expect("info rebuild log emitted");
    assert_eq!(info.field("replayed_negative"), Some("3"));
    assert_eq!(info.field("row_cap"), Some("5000"));
}

#[test]
fn rebuild_log_no_warn_under_cap() {
    let events = routectl_testkit::capture_events(|| {
        emit_rebuild_log(&CapabilityRebuildSummary::default(), REBUILD_ROW_LIMIT - 1);
    });
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "no cap-hit warning under the row cap"
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::INFO),
        "the info rebuild log still fires"
    );
}
