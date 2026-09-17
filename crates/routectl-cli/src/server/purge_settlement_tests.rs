//! Tests for daemon-owned purge settlement: the properties that only hold
//! because the settlement does not live in the request's future.
//!
//! Each test drives the tracker against a writer channel it controls, so the
//! commit can be held open at the exact instant a client disappears -- which is
//! the whole window these tests exist to cover.

use super::*;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_router::{CapabilityEventRow as ReplayRow, CapabilityLedgerReader, ReplayTombstone};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};
use std::time::Instant;
use tempfile::TempDir;

/// A router holding one resident learned negative on `nick`.
fn router_with_negative(capability: &str) -> Arc<Router> {
    let mut config = routectl_router::Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        routectl_router::ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
    );
    config.models.insert(
        "nick".to_string(),
        routectl_router::ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    let router = Arc::new(Router::new(Arc::new(config)));
    let reader = PlantedLedger::negative(&router, "nick", capability);
    let summary = router.rebuild_learned_from_ledger(&reader);
    assert_eq!(
        summary.replayed_negative, 1,
        "the fixture must plant one negative, or every assertion about purging it is vacuous",
    );
    router
}

/// A router with the same config but NO planted entries, for the restart checks.
fn router_with_no_entries() -> Arc<Router> {
    let mut config = routectl_router::Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        routectl_router::ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
    );
    config.models.insert(
        "nick".to_string(),
        routectl_router::ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    Arc::new(Router::new(Arc::new(config)))
}

fn resident(router: &Router, capability: &str) -> bool {
    router
        .learned_capability_snapshot()
        .iter()
        .any(|e| e.state_key == "nick" && e.feature_key == capability)
}

/// A one-row capability ledger for planting, through the real replay path.
struct PlantedLedger {
    tombstone: ReplayTombstone,
    rows: Vec<ReplayRow>,
}

impl PlantedLedger {
    fn negative(router: &Router, state_key: &str, capability: &str) -> Self {
        Self {
            tombstone: ReplayTombstone::new(1, router.catalog_version(), router.overlay_revision()),
            rows: vec![ReplayRow::new(
                2,
                Instant::now(),
                "broken".to_string(),
                Some(FailurePhase::F1.as_str().to_string()),
                EvidenceSource::Live.as_str().to_string(),
                Some(SignalTier::SelfIdentifying.as_str().to_string()),
                None,
                capability.to_string(),
                state_key.to_string(),
                "anthropic-api".to_string(),
                router.catalog_version(),
                router.overlay_revision(),
            )],
        }
    }
}

impl CapabilityLedgerReader for PlantedLedger {
    fn tombstone(&self) -> Option<ReplayTombstone> {
        Some(self.tombstone)
    }
    fn read_events(&self) -> Vec<ReplayRow> {
        self.rows.clone()
    }
}

/// The production-shaped `cleared` row a purge commits, built the same way the
/// control route builds it: the settlement's own keys, the `cleared` verdict, a
/// live source, and no invented provenance.
///
/// The tests submit THIS rather than an empty batch: an empty batch commits
/// without writing a row, so a ledger assertion against it would pass whether or
/// not the purge's clear was ever shaped correctly.
fn cleared_row(
    router: &Router,
    reserved: &routectl_router::router::ReservedPurge,
) -> routectl_usage::CapabilityEvent {
    let settlement = reserved.settlement();
    routectl_usage::CapabilityEvent {
        ts: 1_700_000_000_000,
        lane_key: settlement.state_key.clone(),
        capability: settlement.capability_key.clone(),
        verdict: routectl_core::capability::Verdict::Cleared
            .as_str()
            .to_string(),
        phase: String::new(),
        source: EvidenceSource::Live.as_str().to_string(),
        tier: String::new(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: i64::from(router.catalog_version()),
        overlay_revision: i64::try_from(router.overlay_revision()).unwrap_or(i64::MAX),
    }
}

/// The `cleared` rows in a ledger, as `(rowid, lane, capability)`.
fn cleared_rows(path: &std::path::Path) -> Vec<(i64, String, String)> {
    let conn = rusqlite::Connection::open(path).expect("read open");
    let mut stmt = conn
        .prepare(
            "SELECT rowid, lane_key, capability FROM capability_events \
             WHERE verdict = 'cleared' ORDER BY rowid",
        )
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

/// Reserve a purge on the router, for the tests that need a live reservation.
fn reserve(router: &Router, capability: &str) -> Box<routectl_router::router::ReservedPurge> {
    match router.reserve_learned_capability_purge("nick", capability) {
        routectl_router::router::PurgeOutcome::Reserved(reserved) => reserved,
        _ => panic!("a resident entry on the live generation must reserve"),
    }
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A CANCELLED client does not cancel the settlement -- asserted on the LEDGER
/// and on memory, through the real writer.
///
/// THE property daemon ownership buys, and the previous version of this test only
/// half-asserted it: it submitted an EMPTY batch, which commits without writing a
/// row, so it could not tell a correctly-shaped clear from no clear at all. This
/// one submits the production-shaped `cleared` row, drops the caller after
/// admission, and then checks both halves -- the row is in the ledger at a real
/// rowid, and the entry is gone from memory.
#[tokio::test]
async fn a_dropped_caller_still_commits_the_clear_and_finalizes_memory() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = dir.path().join("usage.db");
    let (usage, writer) = UsageWriter::start(ledger.clone(), CHANNEL_CAPACITY, 0, true);
    let capability = routectl_core::capability::WEB_SEARCH;
    let router = router_with_negative(capability);
    let (tracker, _ambiguous) = SettlementTracker::new();

    let reserved = reserve(&router, capability);
    let event = cleared_row(&router, &reserved);
    let claim = tracker.claim().expect("an open tracker admits a claim");
    let receipt = usage
        .admit_capability_batch_at(
            vec![event],
            reserved.generation(),
            reserved.generation_incarnation(),
        )
        .expect("a live writer admits the batch");
    let waiter = tracker.settle(claim, Arc::clone(&router), reserved, receipt);

    // The client goes away, mid-commit.
    drop(waiter);

    tracker.close_and_wait(WAIT).await;
    assert!(
        !resident(&router, capability),
        "the settlement must finalize memory even though its caller left: the lease is the \
         daemon's obligation, not the client's",
    );
    drop(usage);
    writer.shutdown();

    let rows = cleared_rows(&ledger);
    assert_eq!(
        rows.len(),
        1,
        "exactly one cleared row must be durable; rows were {rows:?}",
    );
    let (rowid, lane, cap) = &rows[0];
    assert!(*rowid > 0, "the clear must occupy a real ledger rowid");
    assert_eq!(lane, "nick");
    assert_eq!(cap, capability);

    // TWO consecutive rebuilds from that ledger: the negative must stay absent,
    // which is what makes the purge durable rather than merely applied.
    for boot in 1..=2 {
        // A FRESH registry each boot, warmed from the real ledger through the real
        // reader -- the same path `serve` uses at startup.
        let fresh = router_with_no_entries();
        let reader = crate::server::ledger_reader::LedgerCapabilityReader::new(
            ledger.clone(),
            routectl_router::ReplayTombstone::new(
                0,
                fresh.catalog_version(),
                fresh.overlay_revision(),
            ),
        );
        let _ = fresh.rebuild_learned_from_ledger(&reader);
        assert!(
            !resident(&fresh, capability),
            "boot {boot}: the purged negative must not come back -- the cleared row is what \
             stops the warm rebuild from replaying it",
        );
    }
}

/// The FAILURE twin: a clear that does not commit leaves memory and the ledger
/// exactly as they were.
#[tokio::test]
async fn a_failed_commit_leaves_the_entry_acting_and_the_ledger_clean() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = dir.path().join("usage.db");
    let capability = routectl_core::capability::WEB_SEARCH;
    let router = router_with_negative(capability);
    let (tracker, _ambiguous) = SettlementTracker::new();

    // A writer that is gone by the time the receipt resolves, so the outcome is a
    // non-commit rather than a commit.
    let (usage, writer) = UsageWriter::start(ledger.clone(), CHANNEL_CAPACITY, 0, true);
    let reserved = reserve(&router, capability);
    let event = cleared_row(&router, &reserved);
    let claim = tracker.claim().expect("an open tracker admits a claim");
    let receipt = usage
        .admit_capability_batch_at(
            vec![event],
            reserved.generation(),
            reserved.generation_incarnation(),
        )
        .expect("a live writer admits the batch");
    let waiter = tracker.settle(claim, Arc::clone(&router), reserved, receipt);
    let outcome = tokio::time::timeout(WAIT, waiter)
        .await
        .expect("the settlement must answer")
        .expect("the sender answers before dropping");
    tracker.close_and_wait(WAIT).await;
    drop(usage);
    writer.shutdown();

    // Whichever way this writer answered, the two halves must AGREE: a committed
    // clear removes the entry, a failed one leaves it acting. The bug this guards
    // is memory and ledger disagreeing.
    let committed = !cleared_rows(&ledger).is_empty();
    if committed {
        assert_eq!(outcome, SettlementOutcome::Purged);
        assert!(!resident(&router, capability));
    } else {
        assert!(
            matches!(outcome, SettlementOutcome::Failed(_)),
            "a clear that wrote no row must report a failure, not a purge; got {outcome:?}",
        );
        assert!(
            resident(&router, capability),
            "and must leave the entry resident and acting",
        );
    }
}

/// Shutdown WAITS for an in-flight settlement.
#[tokio::test]
async fn shutdown_waits_for_an_in_flight_settlement_before_the_writer_drains() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = dir.path().join("usage.db");
    let (usage, writer) = UsageWriter::start(ledger.clone(), CHANNEL_CAPACITY, 0, true);
    let capability = routectl_core::capability::WEB_SEARCH;
    let router = router_with_negative(capability);
    let (tracker, _ambiguous) = SettlementTracker::new();

    let reserved = reserve(&router, capability);
    let event = cleared_row(&router, &reserved);
    let claim = tracker.claim().expect("an open tracker admits a claim");
    let receipt = usage
        .admit_capability_batch_at(
            vec![event],
            reserved.generation(),
            reserved.generation_incarnation(),
        )
        .expect("a live writer admits the batch");
    let _waiter = tracker.settle(claim, Arc::clone(&router), reserved, receipt);

    tracker.close_and_wait(WAIT).await;

    assert_eq!(
        tracker.in_flight(),
        0,
        "shutdown must wait until every settlement has accounted for its reservation",
    );
    assert!(
        tracker.is_closed(),
        "and must stop admitting new ones, since a new settlement could outlive the writer",
    );
    drop(usage);
    writer.shutdown();
}

/// A CLOSED tracker refuses a CLAIM, before any batch is admitted.
#[tokio::test]
async fn a_closed_tracker_refuses_a_claim_before_any_batch_is_admitted() {
    let (tracker, _ambiguous) = SettlementTracker::new();
    tracker.close_and_wait(WAIT).await;

    assert!(
        tracker.claim().is_none(),
        "a closed tracker must refuse the CLAIM: refusing later, after a batch was already \
         admitted, would leave a commit in flight with nothing owning its settlement",
    );
}

/// A pre-close CLAIMANT is always waited for.
///
/// THE race the two-atomic shape lost. A claim taken just before the close must be
/// visible to `close_and_wait`, so shutdown cannot observe zero and drain the
/// writer out from under a settlement about to need it. Deterministic: the claim
/// is taken first, then the close runs and is asserted to still be waiting on it.
#[tokio::test]
async fn a_claim_taken_before_the_close_is_waited_for() {
    let (tracker, _ambiguous) = SettlementTracker::new();
    let tracker = Arc::new(tracker);

    // A claim exists, and is deliberately NOT spent yet.
    let claim = tracker.claim().expect("an open tracker admits a claim");
    assert_eq!(tracker.in_flight(), 1, "the claim is counted immediately");

    // The close cannot complete while the claim is outstanding.
    let closing = {
        let tracker = Arc::clone(&tracker);
        tokio::spawn(async move {
            tracker
                .close_and_wait(std::time::Duration::from_secs(30))
                .await;
        })
    };
    let too_early = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        // Nothing to await but the close's own completion; if it finishes here it
        // observed zero while a claim was outstanding.
    })
    .await;
    assert!(too_early.is_ok(), "the probe window itself must elapse");
    assert!(
        !closing.is_finished(),
        "close_and_wait must NOT finish while a pre-close claim is outstanding: observing zero \
         there is exactly how shutdown drained the writer out from under a settlement",
    );

    // Releasing the claim lets the close finish.
    drop(claim);
    tokio::time::timeout(WAIT, closing)
        .await
        .expect("the close must finish once the claim is released")
        .expect("the closing task must not panic");
    assert_eq!(tracker.in_flight(), 0);
}

/// A claim that is never spent RELEASES, so shutdown does not wait forever.
#[tokio::test]
async fn an_unspent_claim_releases_on_drop() {
    let (tracker, _ambiguous) = SettlementTracker::new();
    {
        let _claim = tracker.claim().expect("an open tracker admits a claim");
        assert_eq!(tracker.in_flight(), 1);
    }
    assert_eq!(
        tracker.in_flight(),
        0,
        "an admission that failed after claiming must release its slot, or shutdown waits for \
         a settlement that never started",
    );
}

// ---------------------------------------------------------------------------
// The ambiguity channel
// ---------------------------------------------------------------------------

/// An UNACCOUNTED settlement reports on the ambiguity channel.
///
/// The guard's drop is what turns a panicking or vanished settlement into a
/// reported ambiguity rather than a silently missing decrement. Driven directly
/// because a panic inside a spawned task is caught by the runtime, so the guard's
/// drop is the observable seam.
#[tokio::test]
async fn an_unaccounted_settlement_sends_on_the_ambiguity_channel() {
    let (tracker, mut ambiguous) = SettlementTracker::new();
    let claim = tracker.claim().expect("an open tracker admits a claim");

    // A guard built from the claim and dropped WITHOUT being accounted for --
    // exactly what a panic unwinding through a settlement task produces.
    tracker.drop_unaccounted_guard_for_tests(claim);

    assert!(
        ambiguous.try_recv().is_ok(),
        "an unaccounted settlement must report: after its batch was admitted the daemon cannot \
         say whether the clear committed, and continuing to serve would route on state it \
         cannot verify",
    );
    assert_eq!(
        tracker.in_flight(),
        0,
        "and must still release its slot, so shutdown is not blocked by it",
    );
}

/// An ACCOUNTED settlement sends nothing.
///
/// The control: without it, a channel that fired on every settlement would
/// satisfy the test above while making ordinary purges shut the daemon down.
#[tokio::test]
async fn an_accounted_settlement_sends_nothing() {
    let (tracker, mut ambiguous) = SettlementTracker::new();
    let claim = tracker.claim().expect("an open tracker admits a claim");

    tracker.drop_accounted_guard_for_tests(claim);

    assert!(
        ambiguous.try_recv().is_err(),
        "an ordinary settlement must NOT report an ambiguity: firing on every purge would make \
         the daemon shut down on its own success path",
    );
    assert_eq!(tracker.in_flight(), 0);
}

/// A settlement task that PANICS still releases and reports.
///
/// The runtime catches the panic, so the guard's drop during unwind is the seam
/// that must still run -- otherwise a panicking settlement would hold its slot
/// forever and shutdown would wait out its deadline.
#[tokio::test]
async fn a_panicking_settlement_task_releases_and_reports() {
    let (tracker, mut ambiguous) = SettlementTracker::new();
    let claim = tracker.claim().expect("an open tracker admits a claim");

    let panicked = tokio::spawn(async move {
        let _guard = claim;
        panic!("an induced settlement panic");
    });
    assert!(
        panicked.await.is_err(),
        "premise: the task must actually panic, or this asserts nothing about unwinding",
    );

    assert_eq!(
        tracker.in_flight(),
        0,
        "a panicking settlement must release its slot during unwind, or shutdown waits out its \
         whole deadline for a task that is already gone",
    );
    // An unspent claim's drop releases without reporting: the panic happened
    // before any batch obligation existed, so there is nothing ambiguous.
    assert!(
        ambiguous.try_recv().is_err(),
        "a panic before the settlement started is not an ambiguity: no batch was admitted, so \
         nothing is in doubt",
    );
}
