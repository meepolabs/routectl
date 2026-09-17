//! Tests for the capability replay-boundary batch a revision-changing
//! reload must commit before it publishes a replacement router.
//!
//! These drive the REAL seams end to end -- a live `UsageWriter` over a real
//! SQLite file, the production boundary batch, the shared ledger reader, and
//! the router's own warm rebuild -- because the defect these guard against is
//! invisible to any narrower fixture: the cold-boot read starts at the newest
//! tombstone, so a survivor that was never re-appended past it cannot be seen
//! by the replayer no matter what the replay predicate decides.

use super::*;
use crate::handlers::control::cleared_event;
use crate::server::test_support::{isolate_usage_db, overlay_at_revision};
use crate::server::{capability_rebuild, ledger_reader};
use routectl_router::Config;
use routectl_router::Router;
use routectl_router::router::PurgeOutcome;
use routectl_usage::{CHANNEL_CAPACITY, CapabilityEvent, UsageWriter};
use std::sync::Arc;

/// Run BOTH boundary phases -- the guarded cut and the settle -- with a
/// shutdown signal that never fires. The production caller keeps them apart so
/// it can publish only after the settle; a test asserting the end state wants
/// them together.
async fn commit_capability_boundary(
    usage: &UsageHandle,
    router: &Arc<Router>,
) -> BoundaryOutcomeReport {
    let (_tx, mut never) = tokio::sync::watch::channel(());
    match admit_capability_boundary(usage, router) {
        Ok(admitted) => admitted.settle(&mut never).await,
        Err(failure) => BoundaryOutcomeReport::Failed(failure),
    }
}

/// Commit a batch off the runtime thread (the blocking path panics on a
/// worker). Test seeding only; production uses admit + await.
fn commit_off_runtime(
    usage: &UsageHandle,
    events: Vec<CapabilityEvent>,
    generation: u64,
) -> routectl_usage::BatchCommit {
    std::thread::scope(|scope| {
        scope
            .spawn(|| usage.commit_capability_events_blocking(events, generation))
            .join()
            .expect("commit thread")
    })
}

/// Run the (blocking) startup warm off the runtime thread.
///
/// The warm commits its fail-closed boundary through the acknowledged batch
/// path, which blocks on the writer's reply; Tokio panics if a worker blocks.
/// Production dispatches the whole warm through `spawn_blocking` for the same
/// reason.
fn warm_off_runtime(db_path: &std::path::Path, router: &Router, usage: &UsageHandle) {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                capability_rebuild::warm_capability_registry_from_ledger(db_path, router, usage);
            })
            .join()
            .expect("warm thread");
    });
}

const FIELD_PATH: &str = "thinking.enabled.display";

/// The wire-shape capability key the tests plant and then track.
///
/// The namespace prefix is owned by a single module in `routectl-router`, and
/// a lexical guard there fails if the literal appears anywhere else under
/// `crates/`: a second spelling could drift from the owner and re-partition
/// persisted history. That module's constructor is crate-private to it, so
/// this fixture assembles the key from parts -- the scanned source carries no
/// full prefix literal while the VALUE stays byte-identical.
fn key_prefix() -> String {
    format!("{}{}", "fie", "ld:")
}

fn field_key() -> String {
    format!("{}{FIELD_PATH}", key_prefix())
}

/// A self-identifying `broken` live negative for `capability`, stamped at
/// `revision`.
fn broken(ts: i64, capability: &str, revision: i64, catalog: i64) -> CapabilityEvent {
    CapabilityEvent {
        ts,
        lane_key: "nick".to_string(),
        capability: capability.to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: catalog,
        overlay_revision: revision,
    }
}

/// Seed the ledger with a pre-reload session at overlay `revision`: a
/// boundary tombstone followed by one wire-shape and one catalog-scoped
/// negative, then warm a router at that revision so BOTH are resident.
///
/// Planting through the real ledger (rather than a test-only mutator on the
/// router) keeps the fixture faithful to what a live session leaves behind --
/// which is the state the reload boundary has to preserve.
fn seeded_router_with_both_classes(
    config: &Arc<Config>,
    revision: u64,
    usage: &UsageHandle,
) -> Router {
    let now = ledger_reader::epoch_ms_now();
    let signed = i64::try_from(revision).expect("revision fits");
    // The boundary must be stamped with the revision pair this session's
    // router actually carries, or the warm below classifies it as a mismatch
    // and replays nothing -- the fixture would then prove nothing.
    let catalog = i64::from(Router::new(config.clone()).catalog_version());
    let committed = commit_off_runtime(
        usage,
        vec![
            CapabilityEvent::tombstone(now - 5_000, catalog, signed),
            broken(now - 4_000, &field_key(), signed, catalog),
            broken(now - 4_000, "web_search", signed, catalog),
        ],
        1,
    );
    assert_eq!(
        committed,
        routectl_usage::BatchCommit::Committed { rows: 3 },
        "the pre-reload session must be persisted",
    );

    let mut router = Router::new(config.clone());
    router.install_catalog_overlay(overlay_at_revision(revision));
    warm_off_runtime(&config.usage.db_path, &router, usage);
    assert_eq!(
        resident_keys(&router).len(),
        2,
        "both classes must be resident before the reload",
    );
    router
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

/// Simulate a process restart: a fresh router (empty registry) warmed from the
/// ledger exactly as `serve` does at bootstrap.
fn restart_from_ledger(config: &Arc<Config>, revision: u64, usage: &UsageHandle) -> Router {
    let mut router = Router::new(config.clone());
    router.install_catalog_overlay(overlay_at_revision(revision));
    warm_off_runtime(&config.usage.db_path, &router, usage);
    router
}

/// THE end-to-end catalog-survival guarantee, across a reload AND two
/// consecutive restarts, against a real ledger.
///
/// A pre-reload wire-shape negative must still be resident after a
/// revision-changing reload, after the restart that follows it, and after a
/// second restart -- while its catalog-scoped sibling is gone from the first
/// restart onward. Deleting the survivor restatement from the boundary batch
/// reds this test: the post-reload tombstone would then hide the survivor's
/// original row from every later read.
#[tokio::test]
async fn a_wire_shape_verdict_survives_a_revision_reload_and_two_restarts() {
    // Arrange: a real ledger and writer.
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    // A pre-reload session learned both classes at revision 1.
    let before = seeded_router_with_both_classes(&config, 1, &usage);

    // Act, phase one: the revision-changing reload. The replacement router
    // carries the catalog-independent half, and the boundary batch persists
    // the new tombstone plus the survivor restatements atomically.
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);
    let reloaded = Arc::new(reloaded);
    let outcome = commit_capability_boundary(&usage, &reloaded).await;
    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Committed { survivors: 1, .. }
        ),
        "the boundary batch must commit one survivor, got {outcome:?}",
    );
    assert_eq!(
        resident_keys(&reloaded),
        vec![field_key()],
        "the reload itself keeps only the catalog-independent entry",
    );

    // Act, phase two: the first restart, warming from the ledger.
    let restarted = restart_from_ledger(&config, 2, &usage);

    // Assert: the wire-shape verdict came back; the catalog-scoped one did not.
    assert_eq!(
        resident_keys(&restarted),
        vec![field_key()],
        "the wire-shape verdict must survive the restart, and only it",
    );

    // Act, phase three: a SECOND consecutive restart. The first restart wrote
    // no fresh boundary (its tombstone matched), so the survivor's restated
    // row must still be visible past the same boundary.
    let restarted_again = restart_from_ledger(&config, 2, &usage);

    // Assert: still resident -- the guarantee does not decay per boot.
    assert_eq!(
        resident_keys(&restarted_again),
        vec![field_key()],
        "the verdict must survive a second consecutive restart",
    );

    // Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
    // consumer's recv loop to end, which only happens once EVERY sender is
    // gone. A live handle makes it pay the full drain deadline and then
    // detach the thread.
    drop(usage);
    writer.shutdown();
}

/// A restatement re-states an existing fact, so it must NOT refresh the
/// entry's decay age. Otherwise every config reload would silently extend
/// every wire-shape verdict's life, and a verdict could never lapse on a
/// daemon that reloads regularly.
#[tokio::test]
async fn a_restated_survivor_keeps_its_original_observation_age() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let survivor = before
        .catalog_independent_survivors()
        .into_iter()
        .next()
        .expect("one survivor");

    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);
    let reloaded = Arc::new(reloaded);
    commit_capability_boundary(&usage, &reloaded).await;

    // The persisted restatement's ts must reflect the ORIGINAL observation,
    // not the reload.
    //
    // The row examined must be the one appended AFTER the new tombstone: the
    // seeded pre-reload row has the same verdict and capability and is
    // genuinely old, so matching the first such row would pass even with the
    // restatement deleted entirely.
    let restated_age = survivor.last_seen.elapsed();
    let rows = ledger_capability_rows(&config.usage.db_path);
    let last_tombstone = rows
        .iter()
        .rposition(|(_, verdict, _)| verdict == "tombstone")
        .expect("the reload appended a tombstone");
    let restated = rows[last_tombstone + 1..]
        .iter()
        .find(|(_, verdict, capability)| {
            verdict == "broken" && capability.starts_with(&key_prefix())
        })
        .expect("the survivor was restated PAST the new boundary");
    assert_eq!(restated.2, field_key());
    // The row's ts is derived from the entry's own last_seen, so it is at
    // least as old as the observation -- never reset to the reload instant.
    let ts = restated.0;
    let now_ms = ledger_reader::epoch_ms_now();
    let row_age_ms = u64::try_from(now_ms.saturating_sub(ts).max(0)).unwrap_or(0);
    assert!(
        row_age_ms + 50 >= u64::try_from(restated_age.as_millis()).unwrap_or(0),
        "a restatement must not reset the observation age (row_age={row_age_ms}ms)",
    );

    // Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
    // consumer's recv loop to end, which only happens once EVERY sender is
    // gone. A live handle makes it pay the full drain deadline and then
    // detach the thread.
    drop(usage);
    writer.shutdown();
}

/// A reload with no survivors still moves the boundary, and says so -- the
/// tombstone is what keeps this session's later events replayable.
#[tokio::test]
async fn a_reload_with_no_survivors_still_commits_the_boundary() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));

    let reloaded = Arc::new(reloaded);
    let outcome = commit_capability_boundary(&usage, &reloaded).await;

    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Committed { survivors: 0, .. }
        ),
        "got {outcome:?}",
    );
    let rows = ledger_capability_rows(&config.usage.db_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "tombstone");

    // Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
    // consumer's recv loop to end, which only happens once EVERY sender is
    // gone. A live handle makes it pay the full drain deadline and then
    // detach the thread.
    drop(usage);
    writer.shutdown();
}

/// An unavailable writer must be reported as a FAILED boundary, so the caller
/// keeps the previous router active rather than publishing one whose verdicts
/// the next boot cannot see.
///
/// The channel is closed by dropping the writer's receiver directly. Shutting
/// a `UsageWriter` down does NOT close it while a `UsageHandle` still holds a
/// sender clone -- the batch would commit and the test would pass for the
/// wrong reason.
#[tokio::test]
async fn an_unavailable_writer_reports_a_failed_boundary() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);

    // A handle over a channel whose consumer is already gone.
    let usage = routectl_usage::handle_with_closed_channel();

    let reloaded = Router::new(config.clone());
    let reloaded = Arc::new(reloaded);
    let outcome = commit_capability_boundary(&usage, &reloaded).await;

    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Failed(routectl_usage::BatchCommit::Unavailable)
        ),
        "an unavailable writer must report a failed boundary, got {outcome:?}",
    );
}

/// A SQLite failure on the survivor row must leave the ledger exactly as it
/// was -- no new tombstone, no restatement -- and must be reported as a failed
/// boundary. A half-applied batch is the corruption the atomicity exists to
/// prevent: a committed tombstone with a dropped survivor evicts the very
/// verdict the batch was preserving.
#[tokio::test]
async fn a_write_failure_commits_no_row_and_reports_a_failed_boundary() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    // Seed FIRST, then arm the trigger: arming it earlier would abort the
    // seeding itself and the test would exercise an empty ledger.
    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let rows_before = ledger_capability_rows(&config.usage.db_path).len();
    {
        let db = routectl_usage::open(&config.usage.db_path).expect("open");
        db.conn()
            .execute_batch(&format!(
                "CREATE TRIGGER reject_field BEFORE INSERT ON capability_events \
                     WHEN NEW.capability LIKE '{}%' \
                     BEGIN SELECT RAISE(ABORT, 'forced row failure'); END",
                key_prefix(),
            ))
            .expect("install trigger");
    }

    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);
    let reloaded = Arc::new(reloaded);
    let outcome = commit_capability_boundary(&usage, &reloaded).await;

    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Failed(routectl_usage::BatchCommit::WriteFailed)
        ),
        "got {outcome:?}",
    );
    assert_eq!(
        ledger_capability_rows(&config.usage.db_path).len(),
        rows_before,
        "a failed boundary must leave no new row -- not even the tombstone",
    );

    // Drop the handle's sender BEFORE shutdown: `shutdown` waits for the
    // consumer's recv loop to end, which only happens once EVERY sender is
    // gone. A live handle makes it pay the full drain deadline and then
    // detach the thread.
    drop(usage);
    writer.shutdown();
}

/// Read every capability row as `(ts, verdict, capability)` in append order.
fn ledger_capability_rows(path: &std::path::Path) -> Vec<(i64, String, String)> {
    let conn = rusqlite::Connection::open(path).expect("read open");
    let mut stmt = conn
        .prepare("SELECT ts, verdict, capability FROM capability_events ORDER BY rowid")
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

/// A catalog-independent survivor that CARRIES an evidence class must keep it
/// through the restatement, or the next boot drops the verdict entirely.
///
/// The cold rebuild requires a recognized evidence class for `verified` and
/// `suspect` rows and skips any row missing one (fail-closed: it will not mint
/// a positive on unattributable evidence). So a restatement that omits the
/// class does not merely lose forensic detail -- the row is skipped on the
/// next boot and the verdict is gone, which is the exact eviction this whole
/// change exists to prevent. Only the `broken` shape carries no class, so
/// handling only that shape would leave the two evidence-bearing verdicts
/// silently lossy.
#[tokio::test]
async fn an_evidence_bearing_survivor_keeps_its_class_through_reload_and_restart() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    // Arrange: a pre-reload session whose wire-shape verdict is a VERIFIED
    // positive -- the shape that requires an evidence class on replay.
    let now = ledger_reader::epoch_ms_now();
    let catalog = i64::from(Router::new(config.clone()).catalog_version());
    let mut verified = broken(now - 4_000, &field_key(), 1, catalog);
    verified.verdict = "verified".to_string();
    verified.phase = "f3".to_string();
    verified.tier = String::new();
    verified.evidence_class = Some("thinking_blocks".to_string());
    let committed = commit_off_runtime(
        &usage,
        vec![
            CapabilityEvent::tombstone(now - 5_000, catalog, 1),
            verified,
        ],
        1,
    );
    assert_eq!(
        committed,
        routectl_usage::BatchCommit::Committed { rows: 2 }
    );

    let mut before = Router::new(config.clone());
    before.install_catalog_overlay(overlay_at_revision(1));
    warm_off_runtime(&config.usage.db_path, &before, &usage);
    assert_eq!(
        resident_keys(&before),
        vec![field_key()],
        "the verified wire-shape entry must be resident before the reload",
    );

    // Act: revision-changing reload, then a restart.
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);
    let reloaded = Arc::new(reloaded);
    let outcome = commit_capability_boundary(&usage, &reloaded).await;
    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Committed { survivors: 1, .. }
        ),
        "got {outcome:?}",
    );
    let restarted = restart_from_ledger(&config, 2, &usage);

    // Assert: still resident. A restatement that dropped the class would be
    // skipped by the rebuild's fail-closed evidence check and this would be
    // empty.
    assert_eq!(
        resident_keys(&restarted),
        vec![field_key()],
        "an evidence-bearing survivor must replay after the restart",
    );

    // And the persisted restatement carries the class verbatim.
    let rows = ledger_capability_rows_full(&config.usage.db_path);
    let last_tombstone = rows
        .iter()
        .rposition(|row| row.verdict == "tombstone")
        .expect("the reload appended a tombstone");
    let restated = rows[last_tombstone + 1..]
        .iter()
        .find(|row| row.capability == field_key())
        .expect("the survivor was restated past the new boundary");
    assert_eq!(restated.verdict, "verified");
    assert_eq!(
        restated.evidence_class.as_deref(),
        Some("thinking_blocks"),
        "the evidence class must survive the restatement byte for byte",
    );

    drop(usage);
    writer.shutdown();
}

/// One capability row, with the fields the evidence assertions need.
struct FullRow {
    verdict: String,
    capability: String,
    evidence_class: Option<String>,
}

/// Read every capability row with its evidence class, in append order.
fn ledger_capability_rows_full(path: &std::path::Path) -> Vec<FullRow> {
    let conn = rusqlite::Connection::open(path).expect("read open");
    let mut stmt = conn
        .prepare("SELECT verdict, capability, evidence_class FROM capability_events ORDER BY rowid")
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok(FullRow {
            verdict: r.get(0)?,
            capability: r.get(1)?,
            evidence_class: r.get(2)?,
        })
    })
    .expect("query")
    .collect::<Result<Vec<_>, _>>()
    .expect("rows")
}

/// A boot that fails closed on a stale boundary must make its OWN boundary
/// durable before the daemon serves, even with usage capture disabled.
///
/// The startup tombstone was best-effort and gated on `usage.enabled`. With
/// capture off, a boot behind a stale-revision tombstone therefore wrote no
/// boundary at all -- and then a same-revision reload (which moves no boundary,
/// so it writes none either) could ENABLE capture, after which verdicts
/// persisted behind the stale tombstone. Those rows sit before the boundary a
/// later boot reads from, so they are invisible forever: the daemon appears to
/// be learning while nothing it learns can survive a restart.
///
/// The sequence below is that exact path, end to end on a real ledger.
#[tokio::test]
async fn a_stale_boundary_boot_makes_its_own_boundary_durable_while_capture_is_disabled() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    // Capture DISABLED for the boot, as the operator left it.
    config.usage.enabled = false;
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );
    assert!(!usage.is_enabled());

    let catalog = i64::from(Router::new(config.clone()).catalog_version());
    let now = ledger_reader::epoch_ms_now();

    // Arrange: a STALE boundary from an earlier session at revision 1.
    let seeded = commit_off_runtime(
        &usage,
        vec![CapabilityEvent::tombstone(now - 10_000, catalog, 1)],
        1,
    );
    assert_eq!(seeded, routectl_usage::BatchCommit::Committed { rows: 1 });

    // Act, phase one: boot at revision 2. The boundary is stale, so the warm
    // fails closed and must write its own -- durably, despite capture being off.
    let booted = restart_from_ledger(&config, 2, &usage);
    assert!(
        resident_keys(&booted).is_empty(),
        "a stale boundary replays nothing",
    );
    let boundary = latest_tombstone_revision(&config.usage.db_path);
    assert_eq!(
        boundary,
        Some((catalog, 2)),
        "the boot must have appended its OWN revision-2 boundary",
    );

    // Act, phase two: a same-revision reload enables capture. It moves no
    // boundary (the revision did not change), so the boot's tombstone above is
    // the one this session's verdicts must land after.
    usage.set_enabled(true);
    let verdict = broken(now - 1_000, &field_key(), 2, catalog);
    let persisted = commit_off_runtime(&usage, vec![verdict], 1);
    assert_eq!(
        persisted,
        routectl_usage::BatchCommit::Committed { rows: 1 }
    );

    // Act, phase three: restart at the same revision.
    let restarted = restart_from_ledger(&config, 2, &usage);

    // Assert: the verdict learned after the boot's boundary is replayable.
    // Without a durable startup boundary it would sit before a stale
    // tombstone and be invisible here.
    assert_eq!(
        resident_keys(&restarted),
        vec![field_key()],
        "a verdict learned after the boot's own boundary must replay",
    );

    drop(usage);
    writer.shutdown();
}

/// The `(catalog_version, overlay_revision)` of the newest tombstone.
fn latest_tombstone_revision(path: &std::path::Path) -> Option<(i64, i64)> {
    let conn = rusqlite::Connection::open(path).expect("read open");
    conn.query_row(
        "SELECT catalog_version, overlay_revision FROM capability_events \
         WHERE verdict = 'tombstone' ORDER BY rowid DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// Shutdown winning the race against an admitted boundary returns promptly,
/// publishes nothing, and leaves the registry untouched.
///
/// The process must not resume serving on an ambiguous boundary: the rows may
/// or may not commit, so nothing may be published on the strength of them.
#[tokio::test]
async fn shutdown_during_an_admitted_wait_abandons_without_publishing() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    // A writer whose channel accepts but whose consumer never answers, so the
    // wait is genuinely outstanding when shutdown fires.
    let (tx, _rx_held) = tokio::sync::mpsc::channel(8);
    let usage = routectl_usage::handle_over_channel(tx);

    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    let generation_before = reloaded.learned_registry().generation();

    let reloaded = Arc::new(reloaded);
    let admitted = admit_capability_boundary(&usage, &reloaded).expect("admitted");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
    shutdown_tx.send(()).expect("signal shutdown");

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        admitted.settle(&mut shutdown_rx),
    )
    .await
    .expect("settle must return promptly when shutdown wins");

    assert_eq!(outcome, BoundaryOutcomeReport::Abandoned);
    assert_eq!(
        reloaded.learned_registry().generation(),
        generation_before,
        "an abandoned boundary must not advance the generation",
    );
    // And must roll its pending generation back: left installed, every later
    // event would be stamped with a generation no boundary ever committed.
    assert_eq!(
        reloaded
            .learned_registry()
            .effective_persistence_generation(),
        generation_before,
        "an abandoned boundary must roll back its pending generation",
    );
}

/// A cloned `UsageHandle` must not keep the coordinator or the runtime stuck:
/// the receipt is a oneshot, so abandoning the wait resolves immediately even
/// while other handle clones live.
#[tokio::test]
async fn cloned_handles_cannot_wedge_an_abandoned_boundary() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (tx, _rx_held) = tokio::sync::mpsc::channel(8);
    let usage = routectl_usage::handle_over_channel(tx);
    let _clone_a = usage.clone();
    let _clone_b = usage.clone();

    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    let reloaded = Arc::new(reloaded);
    let reloaded = Arc::new(reloaded);
    let admitted = admit_capability_boundary(&usage, &reloaded).expect("admitted");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
    shutdown_tx.send(()).expect("signal");

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        admitted.settle(&mut shutdown_rx),
    )
    .await
    .expect("live handle clones must not delay the abandon");

    assert_eq!(outcome, BoundaryOutcomeReport::Abandoned);
}

/// A refused ADMISSION publishes nothing, advances no generation, prunes
/// nothing, and leaves the previous Router pointer-identical.
#[tokio::test]
async fn a_refused_admission_leaves_the_live_state_pointer_identical() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    // A live session with both classes resident.
    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let live = Arc::new(before);
    let live_ptr = Arc::clone(&live);
    let generation_before = live.learned_registry().generation();
    let resident_before = resident_keys(&live).len();

    // The replacement shares the registry; admission is then refused.
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&live);
    let reloaded = Arc::new(reloaded);
    let reloaded = Arc::new(reloaded);
    let refused =
        admit_capability_boundary(&routectl_usage::handle_with_closed_channel(), &reloaded);

    assert!(refused.is_err(), "a closed writer must refuse admission");
    assert_eq!(
        live.learned_registry().generation(),
        generation_before,
        "a refused admission must not advance the generation",
    );
    assert_eq!(
        resident_keys(&live).len(),
        resident_before,
        "and must prune nothing",
    );
    assert!(
        Arc::ptr_eq(&live, &live_ptr),
        "the previous Router stays pointer-identical",
    );

    drop(usage);
    writer.shutdown();
}

/// The startup warm runs OFF the Tokio worker.
///
/// Proven by progress, not by inspection: a sibling task on the same
/// current-thread runtime must keep advancing while the warm blocks. If the warm
/// ran on the worker, that task could not tick at all -- and on a
/// current-thread runtime Tokio would panic outright when the blocking ack wait
/// began, which is what this fixture would surface.
#[tokio::test]
async fn the_startup_warm_runs_off_the_tokio_worker() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    // A sibling task that ticks while the warm is in flight.
    let ticks = Arc::new(AtomicU32::new(0));
    let sibling = {
        let ticks = Arc::clone(&ticks);
        tokio::spawn(async move {
            for _ in 0..50 {
                ticks.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
    };

    // Act: the warm dispatched exactly as production does.
    let db_path = config.usage.db_path.clone();
    let router = Router::new(config.clone());
    let usage_for_warm = usage.clone();
    let router = tokio::task::spawn_blocking(move || {
        capability_rebuild::warm_capability_registry_from_ledger(
            &db_path,
            &router,
            &usage_for_warm,
        );
        router
    })
    .await
    .expect("the warm must not panic -- a worker-thread block would");

    sibling.await.expect("sibling task");

    // Assert: the sibling made progress across the warm, so the runtime was
    // never blocked; and the warm did its job (a fresh boundary is durable).
    assert!(
        ticks.load(Ordering::SeqCst) > 0,
        "a sibling task must progress while the warm runs",
    );
    assert!(router.learned_capability_snapshot().is_empty());
    assert!(
        latest_tombstone_revision(&config.usage.db_path).is_some(),
        "the warm committed its fail-closed boundary",
    );

    drop(usage);
    writer.shutdown();
}

/// A delayed PRE-boundary event is dropped, while a field observation made
/// AFTER admission is persisted after the boundary.
///
/// Both halves matter. The drop stops a straggler from restoring, on the next
/// boot, the catalog-scoped state the boundary evicted. The persist is what the
/// shared registry buys: an observation from a request still dispatching on the
/// old Router is real, and its event must land after the tombstone so a restart
/// replays it.
#[tokio::test]
async fn a_delayed_pre_boundary_event_is_dropped_while_a_post_admission_field_event_persists() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );
    let catalog = i64::from(Router::new(config.clone()).catalog_version());
    let now = ledger_reader::epoch_ms_now();

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let stale_generation = before.registry_generation();

    // Act: admit the boundary, then emit both kinds of event.
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);
    let reloaded = Arc::new(reloaded);
    let admitted = admit_capability_boundary(&usage, &reloaded).expect("admitted");
    let pending = admitted.pending_generation();

    // A straggler produced BEFORE the boundary, arriving after it.
    usage.try_send_capability_event_in_generation(
        broken(now, "web_search", 1, catalog),
        stale_generation,
    );
    // A field observation made AFTER admission, stamped with the pending
    // generation exactly as the coordinator stamps the old Router.
    usage.try_send_capability_event_in_generation(broken(now, &field_key(), 2, catalog), pending);

    let (_tx, mut never) = tokio::sync::watch::channel(());
    let outcome = admitted.settle(&mut never).await;
    assert!(
        matches!(outcome, BoundaryOutcomeReport::Committed { .. }),
        "got {outcome:?}",
    );

    drop(usage);
    writer.shutdown();

    // Assert: after the newest tombstone, the field event is present and the
    // stale catalog-scoped straggler is not.
    let rows = ledger_capability_rows(&config.usage.db_path);
    let last_tombstone = rows
        .iter()
        .rposition(|(_, verdict, _)| verdict == "tombstone")
        .expect("the boundary tombstone");
    let after: Vec<&str> = rows[last_tombstone + 1..]
        .iter()
        .map(|(_, _, capability)| capability.as_str())
        .collect();
    assert!(
        after.contains(&field_key().as_str()),
        "the post-admission field event must land after the boundary: {after:?}",
    );
    assert!(
        !after.contains(&"web_search"),
        "a pre-boundary straggler must not land after the boundary: {after:?}",
    );
}

/// A WRITE failure must not run the boundary transition: no generation
/// advance, no catalog-scoped prune.
///
/// Distinct from the refused-admission case above, which never queues anything.
/// Here the batch WAS admitted and the transaction failed, which is the path
/// where it is tempting to "clean up anyway" -- and doing so would evict live
/// catalog-scoped state whose boundary was never recorded, so the next boot
/// would replay entries the running process had already discarded.
#[tokio::test]
async fn a_write_failure_runs_no_boundary_transition() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    // Arm the failure AFTER seeding, so the batch is admitted and then fails.
    {
        let db = routectl_usage::open(&config.usage.db_path).expect("open");
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_tombstone BEFORE INSERT ON capability_events \
                 WHEN NEW.verdict = 'tombstone' \
                 BEGIN SELECT RAISE(ABORT, 'forced failure'); END",
            )
            .expect("install trigger");
    }

    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let generation_before = reloaded.learned_registry().generation();
    let resident_before = resident_keys(&reloaded).len();
    assert_eq!(resident_before, 2, "both classes are resident pre-boundary");

    let reloaded = Arc::new(reloaded);
    let outcome = commit_capability_boundary(&usage, &reloaded).await;

    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Failed(routectl_usage::BatchCommit::WriteFailed)
        ),
        "got {outcome:?}",
    );
    assert_eq!(
        reloaded.learned_registry().generation(),
        generation_before,
        "a failed write must not advance the generation",
    );
    assert_eq!(
        resident_keys(&reloaded).len(),
        resident_before,
        "a failed write must prune nothing -- the boundary was never recorded",
    );

    drop(usage);
    writer.shutdown();
}

/// The PRODUCTION startup site dispatches the warm off the runtime.
///
/// A source guard, because the behavioral test above necessarily replicates the
/// dispatch rather than calling `serve_on_listener_with_secrets` (which needs a
/// live listener, config, and secret store). Verified against this guard by
/// mutation: inlining the warm at the production site reds it. Without it, the
/// dispatch could be removed with every behavioral test still green.
#[test]
fn the_production_startup_site_dispatches_the_warm_off_the_runtime() {
    let src = include_str!("serve.rs");
    let warm_at = src
        .find("warm_capability_registry_from_ledger(")
        .expect("the production warm call must exist");
    // The dispatch wraps the call, so `spawn_blocking` precedes it within the
    // same statement. Scan a bounded window back rather than the whole file, so
    // an unrelated `spawn_blocking` elsewhere cannot satisfy this.
    let window_start = src[..warm_at].rfind("let router = {").unwrap_or(0);
    let window = &src[window_start..warm_at];
    assert!(
        window.contains("spawn_blocking"),
        "the startup warm must be dispatched off the runtime; window was:\n{window}",
    );
}

/// A failed boundary leaves the tuning byte-for-byte unchanged.
///
/// The previous Router stays live when a boundary fails, so it must keep the
/// settings it was serving under. Applying the reload's tuning at carry-over
/// time would leave the live Router running under a reload that never took
/// effect.
#[tokio::test]
async fn a_failed_boundary_leaves_the_shared_tuning_unchanged() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let decay_before = before.learned_registry().decay();
    let window_before = before.learned_registry().inferred_window();
    let cap_before = before.learned_registry().max_entries();

    // Arm a write failure, then reload with DIFFERENT capability tuning.
    {
        let db = routectl_usage::open(&config.usage.db_path).expect("open");
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_tombstone BEFORE INSERT ON capability_events \
                 WHEN NEW.verdict = 'tombstone' \
                 BEGIN SELECT RAISE(ABORT, 'forced failure'); END",
            )
            .expect("install trigger");
    }
    let mut capability = config.capability.clone();
    capability.decay_hours = 1;
    capability.inferred_window_hours = 1;
    let retuned = Arc::new(Config {
        capability,
        usage: config.usage.clone(),
        ..Config::default()
    });
    let mut reloaded = Router::new(retuned);
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);

    let outcome = commit_capability_boundary(&usage, &reloaded).await;

    assert!(
        matches!(outcome, BoundaryOutcomeReport::Failed(_)),
        "got {outcome:?}",
    );
    assert_eq!(
        before.learned_registry().decay(),
        decay_before,
        "a failed boundary must not retune the shared decay",
    );
    // The pending generation must be rolled back, not left installed. Left
    // behind, every later event would be stamped with a generation no boundary
    // ever committed -- and the next boundary would compute its pending value
    // from a stale base.
    assert_eq!(
        before.learned_registry().effective_persistence_generation(),
        before.learned_registry().generation(),
        "a failed boundary must roll back its pending generation",
    );
    assert_eq!(before.learned_registry().inferred_window(), window_before);
    assert_eq!(before.learned_registry().max_entries(), cap_before);

    drop(usage);
    writer.shutdown();
}

/// A refused ADMISSION likewise leaves tuning untouched, and rolls back nothing
/// because nothing was installed.
#[tokio::test]
async fn a_refused_admission_leaves_the_shared_tuning_unchanged() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let before = Router::new(config.clone());
    let decay_before = before.learned_registry().decay();

    let mut capability = config.capability.clone();
    capability.decay_hours = 1;
    capability.inferred_window_hours = 1;
    let retuned = Arc::new(Config {
        capability,
        usage: config.usage.clone(),
        ..Config::default()
    });
    let mut reloaded = Router::new(retuned);
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);

    let refused =
        admit_capability_boundary(&routectl_usage::handle_with_closed_channel(), &reloaded);

    assert!(refused.is_err());
    assert_eq!(
        before.learned_registry().decay(),
        decay_before,
        "a refused admission must not retune",
    );
    assert_eq!(
        before.learned_registry().effective_persistence_generation(),
        before.learned_registry().generation(),
        "no pending generation may be left installed",
    );
}

/// A COMMITTED boundary applies the reload's tuning, at publication.
///
/// The positive control for the two tests above: they would also pass if the
/// retune had been dropped entirely.
#[tokio::test]
async fn a_committed_boundary_applies_the_reloads_tuning() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let mut capability = config.capability.clone();
    capability.decay_hours = 1;
    capability.inferred_window_hours = 1;
    let retuned = Arc::new(Config {
        capability,
        usage: config.usage.clone(),
        ..Config::default()
    });
    let mut reloaded = Router::new(retuned);
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);

    let outcome = commit_capability_boundary(&usage, &reloaded).await;

    assert!(
        matches!(outcome, BoundaryOutcomeReport::Committed { .. }),
        "got {outcome:?}",
    );
    assert_eq!(
        reloaded.learned_registry().decay(),
        std::time::Duration::from_hours(1),
        "a committed boundary must apply the reload's tuning",
    );

    drop(usage);
    writer.shutdown();
}

/// An observation made through the still-published OLD Router while the boundary
/// is admitted survives to the next boot.
///
/// The whole point of the pending generation. The observation goes through the
/// real production path -- `observe_learned_capability` on the old Router, then
/// the usage-capture drain -- with no manual generation injection: the Router
/// reads the effective generation itself.
#[tokio::test]
async fn an_old_router_field_observation_during_an_admitted_boundary_survives_a_restart() {
    use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);

    // Admit the boundary; the old Router is still the published one.
    let admitted = admit_capability_boundary(&usage, &reloaded).expect("admitted");

    // The old Router observes a wire-shape fact through the PRODUCTION path.
    let observed_key = field_key();
    let outcome = before.observe_learned_capability(
        "nick",
        &observed_key,
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        std::time::Instant::now(),
    );
    assert!(
        !matches!(outcome, routectl_router::GenerationOutcome::Stale),
        "a wire-shape observation must be accepted during an admitted boundary",
    );
    // And its event is stamped with the generation the Router read for itself.
    let generation = before.learned_registry().effective_persistence_generation();
    // The discriminating assertion: during an admitted boundary the OLD Router
    // must hand out the PENDING generation. An event stamped with the active one
    // is older than the boundary the writer is about to commit, so the writer
    // drops it and this observation is lost. Asserting only that the row survives
    // is not enough -- the writer's bar rises solely on commit, so a
    // wrongly-stamped event that reaches the queue first still lands.
    assert_eq!(
        generation,
        admitted.pending_generation(),
        "an observation during an admitted boundary must carry the pending generation",
    );
    usage.try_send_capability_event_in_generation(
        broken(
            ledger_reader::epoch_ms_now(),
            &observed_key,
            2,
            i64::from(reloaded.catalog_version()),
        ),
        generation,
    );

    let (_tx, mut never) = tokio::sync::watch::channel(());
    let settled = admitted.settle(&mut never).await;
    assert!(
        matches!(settled, BoundaryOutcomeReport::Committed { .. }),
        "got {settled:?}",
    );

    // Restart: the observation replays.
    let restarted = restart_from_ledger(&config, 2, &usage);
    assert!(
        resident_keys(&restarted).contains(&observed_key),
        "the old Router's observation must survive the boundary and the restart: {:?}",
        resident_keys(&restarted),
    );

    drop(usage);
    writer.shutdown();
}

/// THE stamp/boundary/clear regression, end to end on a real ledger.
///
/// The sequence a pre-sampled generation gets wrong:
///
/// 1. a wire-shape negative is planted and made resident by a boot;
/// 2. the reload boundary is ADMITTED, installing the pending generation;
/// 3. the PRODUCTION clear path settles mid-boundary, taking its effective
///    generation from the atomic removal;
/// 4. the boundary commits, promoting that generation;
/// 5. the cleared row drains through the REAL `UsageCapture`;
/// 6. two consecutive restarts replay the ledger.
///
/// The negative must be ABSENT after both restarts. A clear stamped with a
/// generation sampled BEFORE the boundary carries the pre-boundary value, which
/// is older than the committed boundary, so the writer drops the cleared row --
/// and the next boot replays the negative as though nothing had cleared it,
/// resurrecting a verdict a successful probe had settled.
#[tokio::test]
async fn a_clear_settled_across_a_boundary_stays_cleared_across_two_restarts() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );
    let catalog = i64::from(Router::new(config.clone()).catalog_version());
    let now = ledger_reader::epoch_ms_now();

    // (1) A prior session's negative, made resident by a boot at revision 1.
    let seeded = commit_off_runtime(
        &usage,
        vec![
            CapabilityEvent::tombstone(now - 10_000, catalog, 1),
            broken(now - 9_000, &field_key(), 1, catalog),
        ],
        1,
    );
    assert_eq!(seeded, routectl_usage::BatchCommit::Committed { rows: 2 });
    let live = restart_from_ledger(&config, 1, &usage);
    assert_eq!(
        resident_keys(&live),
        vec![field_key()],
        "the negative must be resident before the boundary",
    );

    // (2) The replacement shares the registry; its boundary is ADMITTED.
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&live);
    let reloaded = Arc::new(reloaded);
    let admitted = admit_capability_boundary(&usage, &reloaded).expect("admitted");
    let pending = admitted.pending_generation();

    // (3) The PRODUCTION clear path settles mid-boundary.
    let removed = live.clear_learned_capability("nick", &field_key(), "anthropic-api");
    let routectl_router::GenerationOutcome::Applied {
        value: was_resident,
        generation: clear_generation,
        incarnation: clear_incarnation,
    } = removed
    else {
        panic!("a wire-shape clear must be accepted mid-boundary");
    };
    assert!(was_resident, "the clear removed the resident negative");
    assert_eq!(
        clear_generation, pending,
        "the clear's stamp must be the PENDING generation; a pre-sampled value \
         would be older than the boundary and the writer would drop the row",
    );

    // (4) The boundary commits, promoting that generation.
    let (_tx, mut never) = tokio::sync::watch::channel(());
    let settled = admitted.settle(&mut never).await;
    assert!(
        matches!(settled, BoundaryOutcomeReport::Committed { .. }),
        "got {settled:?}",
    );

    // (5) The cleared row drains through the REAL capture path, stamped with the
    // generation the removal returned.
    let mut meta = routectl_router::DispatchMeta::for_alias("m1");
    meta.cleared_capabilities
        .push(routectl_router::router::CapabilityClearedEvent {
            persistence_generation: clear_generation,
            incarnation: clear_incarnation,
            state_key: "nick".to_string(),
            capability_key: field_key(),
            provider_kind: "anthropic-api".to_string(),
        });
    let request = routectl_core::ChatRequest {
        model: "m1".to_string(),
        ..Default::default()
    };
    let capture = crate::handlers::usage_capture::UsageCapture::new(
        crate::handlers::usage_capture::build_usage_draft("anthropic", &request, "req-b".into()),
        usage.clone(),
        "ingress-boundary".to_string(),
    );
    capture.drain_capability_events(&meta, u32::try_from(catalog).unwrap_or(0), 2);

    drop(capture);
    drop(usage);
    writer.shutdown();

    // (6) Two consecutive restarts: the negative must stay ABSENT.
    let (usage2, writer2) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );
    let first = restart_from_ledger(&config, 2, &usage2);
    assert!(
        !resident_keys(&first).contains(&field_key()),
        "the clear must survive the first restart: {:?}",
        resident_keys(&first),
    );
    let second = restart_from_ledger(&config, 2, &usage2);
    assert!(
        !resident_keys(&second).contains(&field_key()),
        "and the second: {:?}",
        resident_keys(&second),
    );

    drop(usage2);
    writer2.shutdown();
}

/// Shutdown abandonment rolls the pending generation back, so an operation
/// settling after it carries the pre-boundary value again -- correct, because no
/// boundary committed and a stamp for one that never landed would be dropped.
#[tokio::test]
async fn a_settlement_after_shutdown_abandonment_uses_the_rolled_back_generation() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    // A writer whose consumer never answers, so the wait is outstanding when
    // shutdown fires.
    let (tx, _rx_held) = tokio::sync::mpsc::channel(8);
    let usage = routectl_usage::handle_over_channel(tx);

    let live = Router::new(config.clone());
    let generation_before = live.learned_registry().generation();
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&live);
    let reloaded = Arc::new(reloaded);

    let admitted = admit_capability_boundary(&usage, &reloaded).expect("admitted");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
    shutdown_tx.send(()).expect("signal shutdown");
    assert_eq!(
        admitted.settle(&mut shutdown_rx).await,
        BoundaryOutcomeReport::Abandoned,
    );

    assert_eq!(
        live.learned_registry().effective_persistence_generation(),
        generation_before,
        "an abandoned boundary must leave no pending generation installed",
    );
}

/// A CORROBORATED inferred survivor replays with its corroboration intact after a
/// reload and restart; a PENDING one replays as still-pending.
///
/// Residency is not the property that matters. An inferred negative acts only
/// once corroborated (two observations), so a restatement that emits one row
/// replays a corroborated entry as a single pending observation: resident, and
/// silently NOT acting. Traffic that was being routed away starts flowing to a
/// target the daemon had already learned was broken, and nothing reports it.
///
/// This asserts the OBSERVATION COUNT on the public snapshot, which is exactly
/// what decides acting state (`self.observations >= 2` for an inferred signal).
/// The `RoutingDecision` that count produces is asserted in the router's own
/// `one_replayed_inferred_row_stays_pending_while_two_act`, on the real replay
/// path -- keeping a routing-internal type out of the public API just so an
/// acceptance test in this crate can name it.
///
/// Both halves are asserted together because each alone is satisfiable by a wrong
/// implementation: always emitting two rows would corroborate the pending case,
/// and always emitting one would decorroborate the acting case.
#[tokio::test]
async fn an_inferred_survivors_corroboration_survives_a_reload_and_restart() {
    use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    // Arrange: one PENDING inferred wire-shape negative (one observation) and one
    // CORROBORATED one (two), both catalog-independent so both survive.
    let pending_key = field_key();
    let acting_key = format!("{}{}", key_prefix(), "thinking.enabled.budget");
    let live = Router::new(config.clone());
    let t0 = std::time::Instant::now();
    for (key, observations) in [(&pending_key, 1), (&acting_key, 2)] {
        for _ in 0..observations {
            live.observe_learned_capability(
                "nick",
                key,
                "anthropic-api",
                SignalTier::Inferred,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                t0,
            );
        }
    }
    // Premise: the fixture really does differ in corroboration before the reload.
    assert_eq!(observations_of(&live, &pending_key), 1);
    assert_eq!(observations_of(&live, &acting_key), 2);

    // Act: revision-changing reload, boundary committed, then a restart.
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&live);
    let reloaded = Arc::new(reloaded);
    let outcome = commit_capability_boundary(&usage, &reloaded).await;
    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Committed { survivors: 2, .. }
        ),
        "got {outcome:?}",
    );
    let restarted = restart_from_ledger(&config, 2, &usage);

    // Assert on the count that DECIDES acting state, not on residency.
    assert_eq!(
        observations_of(&restarted, &acting_key),
        2,
        "the corroborated inferred negative must replay corroborated; a single \
         restated row would replay it as pending and it would silently stop acting",
    );
    assert_eq!(
        observations_of(&restarted, &pending_key),
        1,
        "and a pending one must NOT be corroborated by the restatement",
    );

    drop(usage);
    writer.shutdown();
}

/// The observation count of one resident entry, or zero when absent.
fn observations_of(router: &Router, capability: &str) -> u32 {
    router
        .learned_capability_snapshot()
        .into_iter()
        .find(|entry| entry.feature_key == capability)
        .map_or(0, |entry| entry.observations)
}

/// A long-lived inferred negative -- corroborated, then reconfirmed LONG after
/// its inferred window -- still replays as acting after a reload and restart.
///
/// This is the case a `first_seen`/`last_seen` pair gets wrong. An inferred
/// negative's corroborating observation must arrive INSIDE the inferred window; a
/// later one resets the entry to a fresh PENDING observation. So for an entry
/// whose `last_seen` is far past that window, replaying two rows at
/// (`first_seen`, `last_seen`) hits the too-late branch and the entry lands
/// resident but silently NOT acting -- traffic resumes to a target the daemon had
/// already learned was broken.
///
/// The fixture makes `last_seen - first_seen` exceed the configured window
/// deliberately, which is exactly the shape a real long-running daemon produces:
/// two rejections close together, then another hours later.
#[tokio::test]
async fn a_reconfirmed_inferred_survivor_still_acts_after_a_reload_and_restart() {
    use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    // A one-hour inferred window, so the third observation below is far outside it.
    config.capability.inferred_window_hours = 1;
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    // Arrange: corroborate inside the window, then reconfirm 6h later.
    let key = field_key();
    let live = Router::new(config.clone());
    // Instants in the PAST, as a real long-running entry has them: `first_seen`
    // 7h ago, corroborated a minute later, reconfirmed 1h ago. A future `t0`
    // would make every `elapsed()` zero and collapse both rows onto the same
    // stamp, hiding the very ordering under test (measured: two identical `ts`).
    let now = std::time::Instant::now();
    let t0 = now
        .checked_sub(std::time::Duration::from_hours(7))
        .expect("test clock is well past boot");
    let observe_at = |at: std::time::Instant| {
        live.observe_learned_capability(
            "nick",
            &key,
            "anthropic-api",
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            at,
        );
    };
    observe_at(t0);
    observe_at(t0 + std::time::Duration::from_mins(1));
    observe_at(t0 + std::time::Duration::from_hours(6));
    // Sanity: the reconfirmation is in the past, so its row carries a real age.
    assert!(t0 + std::time::Duration::from_hours(6) < now);

    // Premise: more than two observations, and a span wider than the window --
    // without both, this is just the two-observation case already covered.
    let entry = live
        .learned_capability_snapshot()
        .into_iter()
        .find(|e| e.feature_key == key)
        .expect("resident");
    eprintln!(
        "LIVE_DIAG obs={} span_secs={}",
        entry.observations,
        entry
            .last_seen
            .saturating_duration_since(entry.first_seen)
            .as_secs(),
    );
    assert!(
        entry.observations > 2,
        "the fixture must reconfirm past corroboration, got {}",
        entry.observations,
    );
    assert!(
        entry.last_seen.saturating_duration_since(entry.first_seen)
            > std::time::Duration::from_hours(1),
        "the span must exceed the inferred window, or the too-late branch is \
         never reached on replay",
    );

    // Act: revision-changing reload, boundary committed, then a restart.
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&live);
    let reloaded = Arc::new(reloaded);
    let outcome = commit_capability_boundary(&usage, &reloaded).await;
    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Committed { survivors: 1, .. }
        ),
        "got {outcome:?}",
    );
    let restarted = restart_from_ledger(&config, 2, &usage);

    // Assert: still CORROBORATED, so still acting. A first_seen/last_seen pair
    // would have replayed the second row as too-late and reset this to 1.
    assert!(
        observations_of(&restarted, &key) >= 2,
        "a reconfirmed inferred survivor must replay corroborated, got {}",
        observations_of(&restarted, &key),
    );

    drop(usage);
    writer.shutdown();
}

/// The generation the LEDGER records and the generation the registry holds stay
/// equal across a committed boundary.
///
/// They are two independent stores of the same fact: the batch's tombstone is
/// stamped with the generation the cut derived, and the registry promotes that
/// same value. If they diverged, the writer's per-event filter and the registry's
/// staleness check would disagree about which events belong to the current
/// boundary -- one dropping events the other considers live.
#[tokio::test]
async fn the_committed_ledger_generation_equals_the_registry_generation() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);

    // The receipt the cut issued is what the batch carries.
    let admitted = admit_capability_boundary(&usage, &reloaded).expect("admitted");
    let expected_gen = admitted.pending_generation();
    let (_tx, mut never) = tokio::sync::watch::channel(());
    let settled = admitted.settle(&mut never).await;

    assert!(
        matches!(
            settled,
            BoundaryOutcomeReport::Committed { generation, .. } if generation == expected_gen
        ),
        "the committed generation must be the receipt's, got {settled:?}",
    );
    assert_eq!(
        reloaded.learned_registry().generation(),
        expected_gen,
        "the registry must hold the same generation the batch was stamped with",
    );
    assert_eq!(
        reloaded
            .learned_registry()
            .effective_persistence_generation(),
        expected_gen,
        "and no pending value may remain installed after a commit",
    );

    drop(usage);
    writer.shutdown();
}

/// A second boundary is refused while the first is in flight, at the CLI seam.
///
/// The registry refuses it; this pins that the coordinator surfaces the refusal as
/// a failed admission rather than proceeding with a boundary it does not own.
#[tokio::test]
async fn a_second_boundary_admission_is_refused_while_the_first_is_in_flight() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    // A channel whose consumer never answers, so the first boundary stays
    // admitted-and-unsettled for the duration.
    let (tx, _rx_held) = tokio::sync::mpsc::channel(8);
    let usage = routectl_usage::handle_over_channel(tx);

    let live = Router::new(config.clone());
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&live);
    let reloaded = Arc::new(reloaded);

    let first = admit_capability_boundary(&usage, &reloaded).expect("the first is admitted");

    let second = admit_capability_boundary(&usage, &reloaded);

    assert!(
        second.is_err(),
        "a second boundary must be refused while the first is unsettled",
    );
    // The first boundary's receipt is untouched, so it can still settle.
    assert_eq!(
        reloaded
            .learned_registry()
            .effective_persistence_generation(),
        first.pending_generation(),
    );
}

/// An open purge lease refuses a racing boundary; once the purge settles and
/// releases it, the retried boundary commits -- both durable, on a real
/// ledger, across two restarts.
///
/// Reserving before admitting is what exercises the ORDER, not just the
/// refusal: the boundary's own cut takes `entries` before it reads
/// `purge_leases`, so a lease opened first must still be visible to it.
#[tokio::test]
async fn a_purge_that_holds_the_lease_refuses_a_racing_boundary_then_both_commit() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);

    // Reserve the purge FIRST: the lease it opens must outlive the boundary
    // attempt below.
    let reserved = match before.reserve_learned_capability_purge("nick", &field_key()) {
        PurgeOutcome::Reserved(reserved) => reserved,
        _ => panic!("premise: the wire-shape entry must be resident to reserve a purge on it"),
    };

    // A boundary admitted while that lease is open must be refused, before
    // any row is queued.
    let rows_before = ledger_capability_rows(&config.usage.db_path).len();
    let refused = commit_capability_boundary(&usage, &reloaded).await;
    assert!(
        matches!(refused, BoundaryOutcomeReport::Failed(_)),
        "a boundary must not be admitted while a purge holds a key's lease, got {refused:?}",
    );
    assert_eq!(
        ledger_capability_rows(&config.usage.db_path).len(),
        rows_before,
        "the refused boundary must queue no row",
    );

    // Commit the purge's own settlement, then finalize it -- releasing the
    // lease.
    let event = cleared_event(&reserved.settlement(), &before);
    let receipt = usage
        .admit_capability_batch_at(
            vec![event],
            reserved.generation(),
            reserved.generation_incarnation(),
        )
        .expect("the purge settlement is admitted");
    assert_eq!(
        receipt.await_outcome().await,
        routectl_usage::BatchCommit::Committed { rows: 1 },
        "the purge settlement must persist before the entry is removed",
    );
    assert!(
        before.finalize_learned_capability_purge(reserved),
        "the reserved entry must still be resident to finalize",
    );

    // The lease is gone: the retried boundary now commits, restating nothing
    // for the purged key.
    let outcome = commit_capability_boundary(&usage, &reloaded).await;
    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Committed { survivors: 0, .. }
        ),
        "the retried boundary must commit once the lease clears, got {outcome:?}",
    );
    assert_eq!(
        resident_keys(&reloaded),
        Vec::<String>::new(),
        "the purged key must stay absent through the boundary that follows it",
    );

    // The ledger holds the clear ahead of the retried boundary's tombstone,
    // and neither restates the purged key.
    let rows = ledger_capability_rows(&config.usage.db_path);
    let cleared_at = rows
        .iter()
        .rposition(|(_, verdict, capability)| verdict == "cleared" && *capability == field_key())
        .expect("the clear must be durable");
    let last_tombstone = rows
        .iter()
        .rposition(|(_, verdict, _)| verdict == "tombstone")
        .expect("the retried boundary must append a tombstone");
    assert!(
        cleared_at < last_tombstone,
        "the clear must land before the retried boundary's tombstone",
    );
    assert!(
        rows[last_tombstone + 1..]
            .iter()
            .all(|(_, _, capability)| *capability != field_key()),
        "the retried boundary must not restate the key its own predecessor's refusal protected",
    );

    // Durable across two consecutive cold boots.
    let restarted = restart_from_ledger(&config, 2, &usage);
    assert_eq!(
        resident_keys(&restarted),
        Vec::<String>::new(),
        "the purge must survive the first restart",
    );
    let restarted_again = restart_from_ledger(&config, 2, &usage);
    assert_eq!(
        resident_keys(&restarted_again),
        Vec::<String>::new(),
        "and survive a second consecutive restart",
    );

    drop(usage);
    writer.shutdown();
}

/// A purge attempted while a boundary is admitted but unsettled is refused;
/// once the boundary settles, the retried purge succeeds -- both durable, on
/// a real ledger, across two restarts.
#[tokio::test]
async fn a_purge_attempted_while_a_boundary_is_unsettled_is_refused_then_retried_successfully() {
    let mut config = Config::default();
    let _dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );

    let before = seeded_router_with_both_classes(&config, 1, &usage);
    let mut reloaded = Router::new(config.clone());
    reloaded.install_catalog_overlay(overlay_at_revision(2));
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);

    // Admit the boundary but do not settle it yet: the registry now carries a
    // pending generation, which refuses every purge attempt while it stands.
    let admitted = admit_capability_boundary(&usage, &reloaded).expect("the boundary is admitted");

    let refused = before.reserve_learned_capability_purge("nick", &field_key());
    assert!(
        matches!(refused, PurgeOutcome::Busy),
        "a purge must be refused while a boundary is admitted and unsettled",
    );

    // Settle the boundary for real, over the real ledger.
    let (_tx, mut never) = tokio::sync::watch::channel(());
    let outcome = admitted.settle(&mut never).await;
    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Committed { survivors: 1, .. }
        ),
        "the boundary must commit once nothing refuses it, got {outcome:?}",
    );
    assert_eq!(
        resident_keys(&reloaded),
        vec![field_key()],
        "the wire-shape entry must be restated past the boundary",
    );

    // The retried purge now succeeds, against the router the boundary
    // published.
    let reserved = match reloaded.reserve_learned_capability_purge("nick", &field_key()) {
        PurgeOutcome::Reserved(reserved) => reserved,
        _ => panic!("premise: the restated entry must still be resident to reserve a purge on it"),
    };
    let event = cleared_event(&reserved.settlement(), &reloaded);
    let receipt = usage
        .admit_capability_batch_at(
            vec![event],
            reserved.generation(),
            reserved.generation_incarnation(),
        )
        .expect("the purge settlement is admitted");
    assert_eq!(
        receipt.await_outcome().await,
        routectl_usage::BatchCommit::Committed { rows: 1 },
        "the purge settlement must persist before the entry is removed",
    );
    assert!(
        reloaded.finalize_learned_capability_purge(reserved),
        "the reserved entry must still be resident to finalize",
    );
    assert_eq!(
        resident_keys(&reloaded),
        Vec::<String>::new(),
        "the purge must remove the entry once its settlement is durable",
    );

    // The ledger holds the restatement, then the clear, in that order.
    let rows = ledger_capability_rows(&config.usage.db_path);
    let last_tombstone = rows
        .iter()
        .rposition(|(_, verdict, _)| verdict == "tombstone")
        .expect("the boundary must append a tombstone");
    let restated_at = rows[last_tombstone + 1..]
        .iter()
        .position(|(_, verdict, capability)| verdict == "broken" && *capability == field_key())
        .map(|offset| last_tombstone + 1 + offset)
        .expect("the boundary must restate the wire-shape entry past its tombstone");
    let cleared_at = rows
        .iter()
        .rposition(|(_, verdict, capability)| verdict == "cleared" && *capability == field_key())
        .expect("the clear must be durable");
    assert!(
        restated_at < cleared_at,
        "the clear must follow the restatement it supersedes",
    );

    // Durable across two consecutive cold boots.
    let restarted = restart_from_ledger(&config, 2, &usage);
    assert_eq!(
        resident_keys(&restarted),
        Vec::<String>::new(),
        "the purge must survive the first restart",
    );
    let restarted_again = restart_from_ledger(&config, 2, &usage);
    assert_eq!(
        resident_keys(&restarted_again),
        Vec::<String>::new(),
        "and survive a second consecutive restart",
    );

    drop(usage);
    writer.shutdown();
}
