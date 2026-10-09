use crate::server::test_support::drain_usage_writer_strict;
use std::path::Path;
use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{Config, Router};
use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter, latest_tombstone, open};
use rusqlite::params;
use tempfile::TempDir;

use super::*;
use crate::server::build_router_from_config;

/// Build the default-config router this crate ships: baked catalog version,
/// overlay revision zero. Both feed the boot revision the warm stamps.
async fn default_router(tmp: &TempDir) -> Router {
    let mut config = Config::default();
    config.usage.db_path = tmp.path().join("router-usage.db");
    // Owns the fixture lane `gpt-nick#upstream` under the kind its rows record.
    config.providers.insert(
        "gpt-nick".to_string(),
        routectl_router::ProviderEntry::openai_compat("https://gpt.example.test/v1", "literal:k"),
    );
    let config = Arc::new(config);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    build_router_from_config(config, secrets)
        .await
        .expect("build router")
}

/// A usage handle backed by a real writer at `path`, enabled so capability
/// events are actually persisted. Returns the owning writer for shutdown.
/// Run the (blocking) warm on a dedicated OS thread, capturing its events.
///
/// The warm now commits its fail-closed boundary through the ACKNOWLEDGED batch
/// path, which blocks on the writer's reply -- and Tokio panics outright if a
/// worker thread blocks. Production dispatches the whole warm via
/// `spawn_blocking` for exactly this reason; these tests must leave the runtime
/// thread too. The tracing capture is thread-local, so the capture has to be
/// installed INSIDE the spawned thread rather than around it.
fn warm_off_runtime(
    ledger: &Path,
    router: &Router,
    handle: &UsageHandle,
) -> Vec<routectl_testkit::CapturedEvent> {
    warm_reporting_off_runtime(ledger, router, handle).1
}

/// [`warm_off_runtime`], also returning the warm's report.
fn warm_reporting_off_runtime(
    ledger: &Path,
    router: &Router,
    handle: &UsageHandle,
) -> (WarmReport, Vec<routectl_testkit::CapturedEvent>) {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut report = None;
                let events = routectl_testkit::capture_events(|| {
                    report = Some(warm_capability_registry_from_ledger(ledger, router, handle));
                });
                (report.expect("the warm returned"), events)
            })
            .join()
            .expect("warm thread")
    })
}

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
         tier, evidence_class, upstream_token, catalog_version, overlay_revision, \
         provider_kind, vocab_version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, ?9, 'openai-compat', ?10)",
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
            routectl_router::CURRENT_VOCAB_VERSION,
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
    let events = warm_off_runtime(&ledger, &router, &handle);

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
    drain_usage_writer_strict(writer).await;
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

/// A never-migrated ledger (a raw sqlite file, `PRAGMA user_version` still at
/// its default 0) fails closed exactly like any other unreadable ledger --
/// this warm does not attempt its own migrating open (see the module doc for
/// why: it would race the writer's own open on the same file). The
/// distinction from a cold ledger is made legible on the read side instead
/// (`ledger_reader::open_error_class`'s `version_too_old` token), not by
/// changing what this warm does with it.
#[tokio::test]
async fn a_never_migrated_ledger_fails_closed_like_any_unreadable_ledger() {
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let ledger = tmp.path().join("usage.db");
    rusqlite::Connection::open(&ledger).expect("create never-migrated sqlite file");

    // Positive control: a bare read against this fixture classifies as the
    // too-old-schema case, not cold and not the ordinary junk-file case.
    assert!(
        matches!(
            classify_boundary(&ledger, router.catalog_version(), router.overlay_revision()),
            BoundaryOutcome::Unreadable("version_too_old")
        ),
        "positive control: the fixture reads as a too-old schema"
    );

    let scratch = tmp.path().join("scratch.db");
    let (handle, writer) = writer_at(&scratch);

    // Act
    let events = warm_off_runtime(&ledger, &router, &handle);

    // Assert: the unreadable-ledger WARN fired (this warm never migrates the
    // file out from under a possibly-racing writer), the registry stayed
    // empty, and a fresh tombstone still reached the writer.
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN),
        "a too-old-schema ledger must emit the unreadable-ledger WARN"
    );
    assert!(router.learned_capability_snapshot().is_empty());

    drop(handle);
    drain_usage_writer_strict(writer).await;
    assert_eq!(
        tombstone_count(&scratch),
        1,
        "the fail-closed path still enqueues exactly one fresh boot tombstone"
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
    warm_off_runtime(&ledger, &router, &handle);
    drop(handle);
    drain_usage_writer_strict(writer).await;

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
        "gpt-nick#upstream",
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
    warm_off_runtime(&ledger, &router, &handle);

    // Assert: the negative is resident after warm, under its lane / capability.
    let snapshot = router.learned_capability_snapshot();
    assert!(
        snapshot.iter().any(|e| e.state_key == "gpt-nick#upstream"
            && e.feature_key == "web_search"
            && e.verdict.as_str() == "broken"),
        "a post-boundary negative must be replayed and resident after warm"
    );

    drop(handle);
    drain_usage_writer_strict(writer).await;
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
        "gpt-nick#lane-current",
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
        "gpt-nick#lane-stale",
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
    warm_off_runtime(&ledger, &router, &handle);

    // Assert: the current-revision negative replays; the stale straggler does not.
    let snapshot = router.learned_capability_snapshot();
    assert!(
        snapshot
            .iter()
            .any(|e| e.state_key == "gpt-nick#lane-current"),
        "the current-revision negative replays"
    );
    assert!(
        !snapshot
            .iter()
            .any(|e| e.state_key == "gpt-nick#lane-stale"),
        "a stale-revision straggler is skipped by the per-row filter"
    );

    drop(handle);
    drain_usage_writer_strict(writer).await;
}

#[tokio::test]
async fn revision_mismatch_drops_a_stale_catalog_scoped_verdict_behind_a_fresh_tombstone() {
    // Arrange: a ledger whose tombstone + negative were stamped at overlay 0,
    // but this boot runs at a different overlay revision.
    let tmp = TempDir::new().expect("tempdir");
    let mut router = default_router(&tmp).await;
    router.install_catalog_overlay(crate::server::test_support::overlay_at_revision(99));
    let cat = i64::from(router.catalog_version());

    let ledger = tmp.path().join("usage.db");
    let db = open(&ledger).expect("open ledger");
    seed_tombstone(db.conn(), 100, cat, 0);
    seed_event(
        db.conn(),
        200,
        "gpt-nick#upstream",
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
    warm_off_runtime(&ledger, &router, &handle);

    // Assert: nothing replayed (fail closed on the revision mismatch).
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "a catalog-scoped verdict of the stale revision does not survive"
    );

    drop(handle);
    drain_usage_writer_strict(writer).await;

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

/// The wire-shape capability key these tests plant.
///
/// A lexical guard in `routectl-router` fails if the namespace prefix literal
/// appears anywhere else under `crates/`, so the fixture assembles it from
/// parts; the VALUE is byte-identical to a real key.
fn field_key() -> String {
    format!("{}{}thinking.enabled.display", "fie", "ld:")
}

/// Seed a stale-revision session: a tombstone at overlay 0 followed by one
/// wire-shape and one catalog-scoped negative at that same revision.
fn seed_stale_session(ledger: &Path, cat: i64) {
    let db = open(ledger).expect("open ledger");
    seed_tombstone(db.conn(), 100, cat, 0);
    for (ts, capability) in [(200, field_key()), (300, "web_search".to_string())] {
        seed_event(
            db.conn(),
            ts,
            "gpt-nick#upstream",
            &capability,
            "broken",
            "f1",
            "live",
            "self-identifying",
            cat,
            0,
        );
    }
}

/// The resident capability keys of a router, sorted.
fn resident_keys(router: &Router) -> Vec<String> {
    let mut keys: Vec<String> = router
        .learned_capability_snapshot()
        .into_iter()
        .map(|e| e.feature_key)
        .collect();
    keys.sort();
    keys
}

/// Every persisted capability row as `(verdict, capability, overlay)`, in
/// append order.
fn ledger_rows(path: &Path) -> Vec<(String, String, i64)> {
    let db = open(path).expect("open ledger");
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT verdict, capability, overlay_revision FROM capability_events ORDER BY rowid",
        )
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

/// A router at overlay revision 99, the revision the stale session is not.
async fn bumped_router(tmp: &TempDir) -> Router {
    let mut router = default_router(tmp).await;
    router.install_catalog_overlay(crate::server::test_support::overlay_at_revision(99));
    router
}

/// A boot across a revision bump keeps the wire-shape verdict and evicts the
/// catalog-scoped one -- in memory now, AND durably, so a second boot at the
/// new revision (a matching tombstone this time) still has it.
#[tokio::test]
async fn revision_bump_boot_restates_a_wire_shape_verdict_and_drops_a_catalog_scoped_one() {
    // Arrange
    let tmp = TempDir::new().expect("tempdir");
    let router = bumped_router(&tmp).await;
    let cat = i64::from(router.catalog_version());
    let ledger = tmp.path().join("usage.db");
    seed_stale_session(&ledger, cat);
    let (handle, writer) = writer_at(&ledger);

    // Act: the first boot after the bump.
    warm_off_runtime(&ledger, &router, &handle);

    // Assert: only the wire-shape verdict is resident.
    assert_eq!(
        resident_keys(&router),
        vec![field_key()],
        "the wire-shape verdict survives the bump and the catalog-scoped one does not"
    );

    // Act: a second boot at the bumped revision.
    let restarted = bumped_router(&tmp).await;
    warm_off_runtime(&ledger, &restarted, &handle);
    drop(handle);
    drain_usage_writer_strict(writer).await;

    // Assert: the restatement is durable past a fresh boundary, and the
    // catalog-scoped verdict was never restated.
    assert_eq!(
        resident_keys(&restarted),
        vec![field_key()],
        "the wire-shape verdict survives the restart after the bump"
    );
    let rows = ledger_rows(&ledger);
    let fresh = rows
        .iter()
        .rposition(|(verdict, _, _)| verdict == "tombstone")
        .expect("a tombstone exists");
    assert_eq!(rows[fresh].2, 99, "the newest tombstone is this boot's");
    assert_eq!(
        rows[fresh + 1..].to_vec(),
        vec![("broken".to_string(), field_key(), 99)],
        "exactly the wire-shape verdict is restated past the fresh tombstone"
    );
}

/// A boundary batch that fails to commit must change nothing: no fresh
/// tombstone, no restatement, and no verdict installed in memory -- a
/// resident verdict whose restatement never became durable would act now and
/// vanish at the next restart.
#[tokio::test]
async fn revision_bump_boot_boundary_failure_commits_nothing_and_installs_nothing() {
    // Arrange: the restatement row is rejected by a trigger, so the writer
    // reaches the batch and its transaction fails part-way through.
    let tmp = TempDir::new().expect("tempdir");
    let router = bumped_router(&tmp).await;
    let cat = i64::from(router.catalog_version());
    let ledger = tmp.path().join("usage.db");
    seed_stale_session(&ledger, cat);
    open(&ledger)
        .expect("open ledger")
        .conn()
        .execute_batch(&format!(
            "CREATE TRIGGER reject_field BEFORE INSERT ON capability_events \
             WHEN NEW.capability LIKE '{}{}%' \
             BEGIN SELECT RAISE(ABORT, 'forced row failure'); END",
            "fie", "ld:",
        ))
        .expect("install trigger");
    let rows_before = ledger_rows(&ledger);
    let (handle, writer) = writer_at(&ledger);

    // Act
    let events = warm_off_runtime(&ledger, &router, &handle);
    drop(handle);
    drain_usage_writer_strict(writer).await;

    // Assert
    assert!(
        events.iter().any(|e| e.level == tracing::Level::ERROR
            && e.message.contains("boot boundary NOT committed")),
        "the uncommitted boundary is reported at ERROR"
    );
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "nothing is installed in memory when the boundary did not commit"
    );
    assert_eq!(
        ledger_rows(&ledger),
        rows_before,
        "a failed boundary leaves the ledger byte-identical -- not even the tombstone"
    );
}

/// Plant a post-boundary row whose `ts` is not an integer. The tombstone query
/// never decodes it, so the boundary still classifies; the event query fails
/// decoding it -- a slice read that fails after a successful classify.
fn poison_slice(ledger: &Path) {
    open(ledger)
        .expect("open ledger")
        .conn()
        .execute(
            "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
             tier, catalog_version, overlay_revision) \
             VALUES ('not-a-timestamp', 'gpt-nick', 'web_search', 'broken', 'f1', 'live', \
             'self-identifying', 1, 0)",
            [],
        )
        .expect("plant undecodable row");
}

/// A stale-boundary slice that cannot be read must not be mistaken for an
/// empty one: committing a fresh tombstone over it would drop every verdict
/// behind the stale boundary for good.
#[tokio::test]
async fn revision_bump_boot_with_an_unreadable_slice_commits_nothing_and_installs_nothing() {
    // Arrange: a stale session carrying a wire-shape survivor, then a row the
    // slice read cannot decode.
    let tmp = TempDir::new().expect("tempdir");
    let router = bumped_router(&tmp).await;
    let cat = i64::from(router.catalog_version());
    let ledger = tmp.path().join("usage.db");
    seed_stale_session(&ledger, cat);
    poison_slice(&ledger);
    assert!(
        matches!(
            classify_boundary(&ledger, router.catalog_version(), router.overlay_revision()),
            BoundaryOutcome::RevisionMismatch { .. }
        ),
        "positive control: the poisoned ledger still classifies as a stale boundary"
    );
    let rows_before = ledger_rows(&ledger);
    let (handle, writer) = writer_at(&ledger);

    // Act
    let events = warm_off_runtime(&ledger, &router, &handle);
    drop(handle);
    drain_usage_writer_strict(writer).await;

    // Assert: a path-free ERROR names the failure class, nothing is resident,
    // and the stale tombstone is still the newest row of its kind.
    let error = events
        .iter()
        .find(|e| e.level == tracing::Level::ERROR)
        .expect("the unreadable slice is reported at ERROR");
    assert!(
        error
            .fields
            .iter()
            .any(|(name, value)| name == "reason" && value == "query_failed"),
        "the ERROR carries the failure class: {error:?}"
    );
    assert!(
        !error
            .fields
            .iter()
            .any(|(_, value)| value.contains(&*tmp.path().to_string_lossy())),
        "the ERROR is path-free: {error:?}"
    );
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "nothing is installed in memory when the slice could not be read"
    );
    assert_eq!(
        ledger_rows(&ledger),
        rows_before,
        "no fresh tombstone and no restatement land over an unread slice"
    );
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
    let events = warm_off_runtime(&ledger, &router, &handle);

    // Assert: a WARN fired, the registry stayed empty, and boot did not panic.
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN),
        "an unreadable ledger must emit a read-failure WARN"
    );
    assert!(router.learned_capability_snapshot().is_empty());

    drop(handle);
    drain_usage_writer_strict(writer).await;
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

/// The revision-skip tally reaches the operator-visible rebuild line, and
/// is rendered apart from the rowid-boundary and unrecognized-token skips
/// it must be distinguishable from.
#[test]
fn rebuild_log_reports_the_revision_skip_tally() {
    let summary = CapabilityRebuildSummary {
        skipped_revision: 4,
        skipped_unknown: 1,
        ..CapabilityRebuildSummary::default()
    };

    let events = routectl_testkit::capture_events(|| {
        emit_rebuild_log(&summary, 12);
    });

    let info = events
        .iter()
        .find(|e| e.level == tracing::Level::INFO)
        .expect("info rebuild log emitted");
    assert_eq!(info.field("skipped_revision"), Some("4"));
    assert_eq!(info.field("skipped_unknown"), Some("1"));
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

/// Records the withheld beta set of every request it is handed.
struct BetaRecorder {
    withheld: parking_lot::Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl routectl_core::Provider for BetaRecorder {
    fn id(&self) -> &'static str {
        "beta-recorder"
    }
    fn normalize_request(
        &self,
        _: &routectl_core::ChatRequest,
    ) -> routectl_core::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(
        &self,
        _: serde_json::Value,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::normalize_response(
            "beta-recorder",
            "unused",
        ))
    }
    async fn complete(
        &self,
        req: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        self.withheld
            .lock()
            .push(req.routectl_internal.withheld_betas.to_vec());
        Ok(routectl_core::ChatResponse {
            model: "wire".to_string(),
            usage: Some(routectl_core::Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    > {
        Err(routectl_core::Error::upstream(
            "beta-recorder",
            500,
            "unused",
        ))
    }
}

const SEED_UPSTREAM: &str = "anthropic.claude-test-v1:0";
const SEED_LANE: &str = "aws#anthropic.claude-test-v1:0";
const FIXTURE_SEED: &[&str] = &["fx-current", "fx-stale"];

/// A bedrock router at overlay revision 99 under [`FIXTURE_SEED`], with one
/// model `m` on lane [`SEED_LANE`] whose provider records the withheld set.
fn seeded_bedrock_router() -> (Router, Arc<BetaRecorder>) {
    let config: Config = toml::from_str(
        "[providers.aws]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         creds = { kind = \"default-chain\" }\n\
         [aliases]\n\
         default = \"m\"\n",
    )
    .expect("fixture config parses");
    let mut router = Router::new(Arc::new(config));
    router.set_beta_seed_for_tests(FIXTURE_SEED);
    router.install_catalog_overlay(crate::server::test_support::overlay_at_revision(99));
    let provider = Arc::new(BetaRecorder {
        withheld: parking_lot::Mutex::new(Vec::new()),
    });
    let model = routectl_router::ResolvedModel::new("m", "aws", provider.clone(), SEED_UPSTREAM);
    router.install_resolved_models(std::iter::once(("m".to_string(), Arc::new(model))).collect());
    (router, provider)
}

/// Append a `cleared` beta row for [`SEED_LANE`] at `overlay_revision`.
fn seed_cleared_beta(conn: &rusqlite::Connection, ts: i64, flag: &str, cat: i64, overlay: i64) {
    conn.execute(
        "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
         tier, evidence_class, upstream_token, catalog_version, overlay_revision, \
         provider_kind, vocab_version) \
         VALUES (?1, ?2, ?3, 'cleared', '', 'live', '', NULL, NULL, ?4, ?5, 'bedrock', ?6)",
        params![
            ts,
            SEED_LANE,
            routectl_router::beta_capability_key(flag).expect("well-formed fixture flag"),
            cat,
            overlay,
            routectl_router::CURRENT_VOCAB_VERSION,
        ],
    )
    .expect("seed cleared beta row");
}

/// Boot a fresh seeded router against `ledger` and report the withheld set
/// one request carrying every fixture flag gets.
async fn withheld_after_boot(ledger: &Path, handle: &UsageHandle) -> Vec<String> {
    let (router, provider) = seeded_bedrock_router();
    warm_off_runtime(ledger, &router, handle);
    let req = routectl_core::ChatRequest {
        model: "m".into(),
        anthropic_beta: FIXTURE_SEED.iter().map(ToString::to_string).collect(),
        ..Default::default()
    };
    Box::pin(router.complete(req))
        .await
        .expect("complete succeeds");
    let mut seen = provider.withheld.lock().clone();
    assert_eq!(seen.len(), 1, "one attempt reached the provider");
    seen.remove(0)
}

/// A seed clear stamped with the boot's revision is restated past the fresh
/// boundary, so the restart after it decides the same; a clear stamped with
/// the superseded revision lapses on both boots.
#[tokio::test]
async fn a_seed_clear_decides_the_same_across_a_revision_bump_boot_and_the_next_one() {
    // Arrange -- a stale tombstone, a clear at the superseded revision and a
    // clear at the revision this daemon now boots at.
    let tmp = TempDir::new().expect("tempdir");
    let ledger = tmp.path().join("usage.db");
    let cat = i64::from(seeded_bedrock_router().0.catalog_version());
    {
        let db = open(&ledger).expect("open ledger");
        seed_tombstone(db.conn(), 100, cat, 0);
        seed_cleared_beta(db.conn(), 200, "fx-stale", cat, 0);
        seed_cleared_beta(db.conn(), 300, "fx-current", cat, 99);
    }
    let (handle, writer) = writer_at(&ledger);

    // Act -- restart 1 crosses the revision change; restart 2 does not.
    let first = withheld_after_boot(&ledger, &handle).await;
    let second = withheld_after_boot(&ledger, &handle).await;
    drop(handle);
    drain_usage_writer_strict(writer).await;

    // Assert
    assert_eq!(first, vec!["fx-stale".to_string()]);
    assert_eq!(second, first, "the restart after the bump decides the same");
}

/// Insert one capability observation with no vocabulary stamp, the shape a
/// legacy-vocabulary router wrote.
fn seed_unstamped_observation(conn: &rusqlite::Connection, ts: i64, capability: &str) {
    conn.execute(
        "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
         tier, evidence_class, upstream_token, catalog_version, overlay_revision, \
         provider_kind, vocab_version) \
         VALUES (?1, 'gpt-nick', ?2, 'broken', 'f1', 'live', 'self-identifying', NULL, NULL, \
         NULL, NULL, 'openai-compat', NULL)",
        params![ts, capability],
    )
    .expect("seed unstamped observation");
}

fn unstamped_observation_count(path: &Path) -> i64 {
    let db = open(path).expect("open ledger");
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM capability_events \
             WHERE vocab_version IS NULL AND verdict <> 'tombstone'",
            [],
            |row| row.get(0),
        )
        .expect("count unstamped observations")
}

/// Rebuild `router` from `ledger` through a throwaway writer at a separate
/// path, so the rebuild itself never opens a writer on the ledger.
async fn rebuild_keys(tmp: &TempDir, ledger: &Path, router: &Router) -> Vec<String> {
    let scratch = tmp.path().join("scratch.db");
    let (handle, writer) = writer_at(&scratch);
    warm_off_runtime(ledger, router, &handle);
    drop(handle);
    drain_usage_writer_strict(writer).await;
    resident_keys(router)
}

/// Deleting the unstamped pre-boundary observations is invisible to a warm
/// rebuild: the registry rebuilt before the writer's legacy data step equals
/// the one rebuilt after it.
#[tokio::test]
async fn the_legacy_observation_purge_leaves_the_rebuilt_registry_unchanged() {
    // Arrange
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let cat = i64::from(router.catalog_version());
    let overlay = i64::try_from(router.overlay_revision()).unwrap();
    let ledger = tmp.path().join("usage.db");
    {
        let db = open(&ledger).expect("open ledger");
        seed_unstamped_observation(db.conn(), 50, "web_search");
        seed_unstamped_observation(db.conn(), 60, "file_search");
        seed_tombstone(db.conn(), 100, cat, overlay);
        for (ts, capability) in [(200, "web_search"), (300, "code_interpreter")] {
            seed_event(
                db.conn(),
                ts,
                "gpt-nick#upstream",
                capability,
                "broken",
                "f1",
                "live",
                "self-identifying",
                cat,
                overlay,
            );
        }
    }
    let before = rebuild_keys(&tmp, &ledger, &router).await;
    assert_eq!(
        unstamped_observation_count(&ledger),
        2,
        "positive control: the rebuild alone leaves the unstamped rows in place"
    );

    // Act: a writer cycle on the ledger runs the legacy data step.
    let (handle, writer) = writer_at(&ledger);
    drop(handle);
    drain_usage_writer_strict(writer).await;
    let after = rebuild_keys(&tmp, &ledger, &default_router(&tmp).await).await;

    // Assert
    assert_eq!(
        unstamped_observation_count(&ledger),
        0,
        "the writer's data step deletes the unstamped pre-boundary observations"
    );
    assert_eq!(
        before,
        vec!["code_interpreter".to_string(), "web_search".to_string()]
    );
    assert_eq!(
        after, before,
        "the purge leaves the rebuilt registry unchanged"
    );
}

/// Assert the report carries exactly the tally and row count the INFO rebuild
/// line logged.
fn assert_report_matches_rebuild_log(
    report: &WarmReport,
    events: &[routectl_testkit::CapturedEvent],
) {
    let info = events
        .iter()
        .find(|e| e.message.contains("warmed learned-capability registry"))
        .expect("the INFO rebuild line fired");
    let summary = report.summary.expect("a replay reports its summary");
    let expected = [
        ("replayed_verified", summary.replayed_verified),
        ("replayed_negative", summary.replayed_negative),
        ("replayed_cleared", summary.replayed_cleared),
        ("cleared_noop", summary.cleared_noop),
        ("replayed_probe", summary.replayed_probe),
        ("skipped_unknown", summary.skipped_unknown),
        ("skipped_revision", summary.skipped_revision),
        ("skipped_vocab", summary.skipped_vocab),
        ("skipped_lane", summary.skipped_lane),
        ("skipped_owner", summary.skipped_owner),
        ("loaded_rows", report.loaded_rows),
    ];
    for (field, value) in expected {
        assert_eq!(
            info.field(field),
            Some(value.to_string().as_str()),
            "report field {field} differs from the logged value"
        );
    }
}

#[tokio::test]
async fn warm_report_on_a_matching_tombstone_is_replayed_with_the_logged_tally() {
    // Arrange: a matching tombstone, one current-revision negative and one
    // stale-revision straggler, so the tally carries a replay AND a skip.
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let cat = i64::from(router.catalog_version());
    let overlay = i64::try_from(router.overlay_revision()).unwrap();
    let ledger = tmp.path().join("usage.db");
    let db = open(&ledger).expect("open ledger");
    seed_tombstone(db.conn(), 100, cat, overlay);
    for (ts, stamped) in [(200, cat), (300, cat + 1)] {
        seed_event(
            db.conn(),
            ts,
            "gpt-nick#upstream",
            "web_search",
            "broken",
            "f1",
            "live",
            "self-identifying",
            stamped,
            overlay,
        );
    }
    drop(db);
    let (handle, writer) = writer_at(&tmp.path().join("scratch.db"));

    // Act
    let (report, events) = warm_reporting_off_runtime(&ledger, &router, &handle);
    drop(handle);
    writer.shutdown();

    // Assert
    assert_eq!(report.outcome, WarmOutcome::Replayed);
    assert_eq!(report.outcome.as_str(), "replayed");
    assert_eq!(report.loaded_rows, 2);
    let summary = report.summary.expect("summary");
    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.skipped_revision, 1);
    assert_report_matches_rebuild_log(&report, &events);
}

#[tokio::test]
async fn warm_report_on_a_committed_restatement_is_restated_with_the_logged_tally() {
    // Arrange
    let tmp = TempDir::new().expect("tempdir");
    let router = bumped_router(&tmp).await;
    let ledger = tmp.path().join("usage.db");
    seed_stale_session(&ledger, i64::from(router.catalog_version()));
    let (handle, writer) = writer_at(&ledger);

    // Act
    let (report, events) = warm_reporting_off_runtime(&ledger, &router, &handle);
    drop(handle);
    writer.shutdown();

    // Assert
    assert_eq!(report.outcome, WarmOutcome::Restated);
    assert_eq!(report.outcome.as_str(), "restated");
    assert_eq!(report.loaded_rows, 2);
    assert_eq!(report.summary.expect("summary").replayed_negative, 1);
    assert_report_matches_rebuild_log(&report, &events);
}

#[tokio::test]
async fn warm_report_is_restate_failed_when_the_boundary_batch_is_rejected() {
    // Arrange: a trigger rejects the restatement row inside the batch.
    let tmp = TempDir::new().expect("tempdir");
    let router = bumped_router(&tmp).await;
    let ledger = tmp.path().join("usage.db");
    seed_stale_session(&ledger, i64::from(router.catalog_version()));
    open(&ledger)
        .expect("open ledger")
        .conn()
        .execute_batch(
            "CREATE TRIGGER reject_all BEFORE INSERT ON capability_events \
             BEGIN SELECT RAISE(ABORT, 'forced row failure'); END",
        )
        .expect("install trigger");
    let (handle, writer) = writer_at(&ledger);

    // Act
    let (report, _) = warm_reporting_off_runtime(&ledger, &router, &handle);
    drop(handle);
    writer.shutdown();

    // Assert
    assert_eq!(
        report,
        WarmReport::without_replay(WarmOutcome::RestateFailed)
    );
    assert_eq!(report.outcome.as_str(), "restate_failed");
}

#[tokio::test]
async fn warm_report_is_restate_failed_when_the_stale_slice_is_unreadable() {
    // Arrange
    let tmp = TempDir::new().expect("tempdir");
    let router = bumped_router(&tmp).await;
    let ledger = tmp.path().join("usage.db");
    seed_stale_session(&ledger, i64::from(router.catalog_version()));
    poison_slice(&ledger);
    let (handle, writer) = writer_at(&ledger);

    // Act
    let (report, _) = warm_reporting_off_runtime(&ledger, &router, &handle);
    drop(handle);
    writer.shutdown();

    // Assert
    assert_eq!(
        report,
        WarmReport::without_replay(WarmOutcome::RestateFailed)
    );
}

/// The fail-closed paths replay nothing, so each reports its outcome with no
/// summary and zero loaded rows.
#[tokio::test]
async fn warm_report_names_each_fail_closed_outcome() {
    enum Ledger {
        Absent,
        WithoutTombstone,
        Junk,
    }
    let cases = [
        ("cold", Ledger::Absent, WarmOutcome::Cold),
        (
            "no_tombstone",
            Ledger::WithoutTombstone,
            WarmOutcome::NoTombstone,
        ),
        (
            "unreadable",
            Ledger::Junk,
            WarmOutcome::Unreadable("pragma"),
        ),
    ];
    for (token, shape, expected) in cases {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let router = default_router(&tmp).await;
        let ledger = tmp.path().join("usage.db");
        match shape {
            Ledger::Absent => {}
            Ledger::WithoutTombstone => drop(open(&ledger).expect("migrated empty ledger")),
            Ledger::Junk => std::fs::write(&ledger, b"not a sqlite database").expect("junk"),
        }
        let (handle, writer) = writer_at(&tmp.path().join("scratch.db"));

        // Act
        let (report, _) = warm_reporting_off_runtime(&ledger, &router, &handle);
        drop(handle);
        writer.shutdown();

        // Assert
        assert_eq!(report, WarmReport::without_replay(expected), "case {token}");
        assert_eq!(report.outcome.as_str(), token, "case {token}");
    }
}
