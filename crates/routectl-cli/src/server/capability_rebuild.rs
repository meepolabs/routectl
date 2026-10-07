//! Startup warm of the learned-capability registry from the usage ledger.
//!
//! On a fresh process start the router's learned-capability registry is
//! empty, so every capability the router previously learned as unsupported
//! (or confirmed working, or probe-settled) would have to be re-learned from
//! live traffic. This module runs a one-shot rebuild at the serve bootstrap,
//! mirroring the K-estimator warm in [`super::k_rebuild`]. The read-only
//! ledger bridge itself (the [`routectl_router::CapabilityLedgerReader`]
//! adapter, the clock
//! map, and the boundary classification) lives in [`super::ledger_reader`],
//! shared with the offline doctor gather; this module owns the serve-side
//! reaction to that classification -- replay-or-enqueue -- and nothing else.
//!
//! The warm runs ONLY at the initial bootstrap, never on a hot-reload:
//! `Router::carry_over_learned_from` already carries (or deliberately clears)
//! the live registry across a reload, so re-running the rebuild there would
//! clobber that decision.
//!
//! # Fail-closed boundary
//!
//! A tombstone row marks the correctness boundary: only a tombstone whose
//! stamped revision matches this boot's catalog / overlay revision replays
//! the post-boundary slice. A tombstone stamped a DIFFERENT revision moves
//! the boundary the same way a revision-changing reload does: the slice after
//! the stale tombstone is replayed through `should_replay` against this
//! boot's revision (catalog-scoped rows of another revision drop,
//! catalog-independent rows survive), and the survivors are restated past a
//! fresh tombstone in one atomic batch built by
//! `capability_boundary::boundary_batch`, the same builder the reload
//! uses. Every other outcome fails closed -- replay nothing AND commit exactly
//! one fresh tombstone stamped this boot's revision so this session's later
//! events sit after a valid boundary:
//!   * no ledger yet (`Cold`), or an unreadable / version-too-new ledger;
//!   * a ledger with no tombstone at all.

use std::path::Path;

use routectl_router::{
    CapabilityRebuildSummary, LearnedCapabilityRegistry, ReplayTombstone, Router,
    rebuild_capabilities_into,
};
use routectl_usage::{BatchCommit, CapabilityEvent, UsageHandle};

use super::capability_boundary::boundary_batch;
use super::ledger_reader::{
    BoundaryOutcome, LedgerCapabilityReader, REBUILD_ROW_LIMIT, SliceReader, classify_boundary,
    epoch_ms_now,
};

/// One-shot warm of the router's learned-capability registry from the usage
/// ledger at serve bootstrap.
///
/// Best-effort and fail-closed: reads this boot's revision from `router`,
/// classifies the ledger's tombstone boundary
/// ([`super::ledger_reader::classify_boundary`]), and either replays the
/// post-boundary slice through [`Router::rebuild_learned_from_ledger`] (on a
/// matching tombstone), restates the surviving verdicts past a fresh boundary
/// (on a stale-revision tombstone, see `restate_survivors_past_new_boundary`),
/// or logs the case and commits one fresh tombstone through `usage` (every
/// fail-closed case). Never fails bootstrap.
///
/// Unlike its sibling warms this one does NOT run its own migrating open:
/// doing so would open a second read-write connection against the SAME file
/// the writer this warm's caller starts just ahead of it may still be
/// migrating on its own spawned thread, racing that open rather than
/// avoiding it -- and that risk is not worth taking for a case the
/// classification split below already makes diagnosable without it. A
/// too-old-schema read here therefore stays on the ordinary fail-closed path
/// below; [`super::ledger_reader::open_error_class`] gives that case its own
/// `version_too_old` token so it renders distinctly from a cold ledger
/// wherever this classification is surfaced (the doctor gather, which never
/// migrates either).
pub(crate) fn warm_capability_registry_from_ledger(
    db_path: &Path,
    router: &Router,
    usage: &UsageHandle,
) {
    let catalog_version = router.catalog_version();
    let overlay_revision = router.overlay_revision();

    match classify_boundary(db_path, catalog_version, overlay_revision) {
        BoundaryOutcome::Replay(tombstone) => {
            let reader = LedgerCapabilityReader::new(db_path.to_path_buf(), tombstone);
            let summary = router.rebuild_learned_from_ledger(&reader);
            emit_rebuild_log(&summary, reader.loaded_rows());
        }
        BoundaryOutcome::RevisionMismatch { stale_rowid } => {
            tracing::info!(
                catalog_version,
                overlay_revision,
                "capability tombstone revision differs from this boot; \
                 restating catalog-independent verdicts past a fresh tombstone"
            );
            restate_survivors_past_new_boundary(db_path, router, usage, stale_rowid);
        }
        outcome => {
            log_fail_closed(&outcome, db_path, catalog_version, overlay_revision);
            commit_fresh_tombstone(usage, catalog_version, overlay_revision);
        }
    }
}

/// Log the fail-closed classification at the level its case warrants and no
/// higher: a cold ledger and an absent tombstone are the ordinary cold-start
/// shapes (debug), and only a genuinely unreadable ledger is a WARN.
fn log_fail_closed(
    outcome: &BoundaryOutcome,
    db_path: &Path,
    catalog_version: u32,
    overlay_revision: u64,
) {
    match outcome {
        // Unreachable in practice (the caller replays or restates these
        // cases), listed for exhaustiveness.
        BoundaryOutcome::Replay(_) | BoundaryOutcome::RevisionMismatch { .. } => {}
        BoundaryOutcome::Cold => tracing::debug!(
            db_path = %db_path.display(),
            "no usage ledger yet; capability warm fails closed to a fresh tombstone (cold start)"
        ),
        BoundaryOutcome::NoTombstone => tracing::debug!(
            catalog_version,
            overlay_revision,
            "no capability tombstone in the ledger; failing closed and writing a fresh one"
        ),
        BoundaryOutcome::Unreadable(class) => tracing::warn!(
            db_path = %db_path.display(),
            reason = class,
            "usage ledger not readable during capability startup warm; \
             leaving registry empty and writing a fresh tombstone"
        ),
    }
}

/// Move the replay boundary from a stale-revision tombstone to this boot's
/// revision WITHOUT evicting the verdicts that survive a revision change.
///
/// The slice after the stale tombstone is read once and replayed into a
/// scratch registry under a boundary descriptor stamped THIS boot's revision,
/// so `should_replay` drops catalog-scoped rows of any other revision and
/// keeps the catalog-independent ones. Every entry the scratch replay leaves
/// resident is restated, after a fresh tombstone, in one acknowledged atomic
/// batch.
///
/// The live registry is populated only once that batch commits, by replaying
/// the same rows into it. On any failure -- the slice read or the batch
/// commit -- nothing is committed and the live registry stays empty:
/// installing verdicts whose restatement never became durable would let them
/// act now and vanish at the next restart, and a bare tombstone (or one
/// restating an unread, empty slice) would evict them outright. The stale tombstone stays the newest
/// boundary, so the next boot retries this same restatement, and rows this
/// session appends at its own revision still replay through `should_replay`.
fn restate_survivors_past_new_boundary(
    db_path: &Path,
    router: &Router,
    usage: &UsageHandle,
    stale_rowid: i64,
) {
    let catalog_version = router.catalog_version();
    let overlay_revision = router.overlay_revision();
    let boundary = ReplayTombstone::new(stale_rowid, catalog_version, overlay_revision);
    let reader = LedgerCapabilityReader::new(db_path.to_path_buf(), boundary);
    let rows = match reader.try_read_events() {
        Ok(rows) => rows,
        Err(failure) => {
            tracing::error!(
                catalog_version,
                overlay_revision,
                reason = failure.as_str(),
                "capability event slice unreadable after the boundary read; boot boundary NOT \
                 committed, registry left empty and the stale tombstone kept so the next boot \
                 retries the restatement"
            );
            return;
        }
    };
    // Read once and served to both replays, so the scratch replay that decides
    // the restatement and the live replay after it consume identical rows.
    let slice = SliceReader::new(boundary, rows);

    let live = router.learned_registry();
    let scratch =
        LearnedCapabilityRegistry::new(live.decay(), live.inferred_window(), live.max_entries());
    scratch.set_seed_scope(live.seed_scope());
    let _ = rebuild_capabilities_into(&slice, &scratch, &router.config.providers);
    let survivors = scratch.recorded_snapshot();
    // Seed clears stamped with this boot's revision survive the replay like any
    // entry of that revision, so they are restated too: otherwise the live
    // replay below honors them now and the next boot, starting past the fresh
    // tombstone, has nothing to replay them from.
    let seed_clears = scratch.seed_clear_snapshot();
    let batch = boundary_batch(
        &survivors,
        &seed_clears,
        epoch_ms_now(),
        catalog_version,
        overlay_revision,
    );

    match usage.commit_capability_events_blocking(batch, live.generation()) {
        BatchCommit::Committed { .. } => {
            let summary = router.rebuild_learned_from_ledger(&slice);
            tracing::info!(
                catalog_version,
                overlay_revision,
                restated_survivors = survivors.len(),
                "committed fresh capability tombstone at boot with survivor restatements"
            );
            emit_rebuild_log(&summary, reader.loaded_rows());
        }
        failure => tracing::error!(
            catalog_version,
            overlay_revision,
            pending_survivors = survivors.len(),
            reason = ?failure,
            "capability boot boundary NOT committed; registry left empty and the stale \
             tombstone kept so the next boot retries the restatement"
        ),
    }
}

/// Commit exactly one fresh tombstone stamped this boot's revision, through
/// the ACKNOWLEDGED batch path.
///
/// Not best-effort, and not subject to `usage.enabled`. Two reasons, both
/// correctness rather than fidelity:
///
/// - The reader trusts only rows after the newest tombstone. If this boot
///   leaves a STALE-revision tombstone in place, every verdict this session
///   learns lands before that boundary and is invisible to every later boot --
///   the daemon appears to learn while nothing it learns can survive a
///   restart.
/// - The gate cannot be consulted, because it is not fixed for the session: a
///   later same-revision reload can enable capture, and a same-revision reload
///   moves no boundary and so writes none of its own. A boot that skipped this
///   write because capture happened to be off at the time would leave that
///   session persisting rows behind a stale boundary.
///
/// A failure is logged at ERROR rather than propagated: boot never fails on
/// the usage subsystem. The consequence is bounded and named -- this session's
/// verdicts may not survive a restart -- which is strictly better than the
/// silent version.
fn commit_fresh_tombstone(usage: &UsageHandle, catalog_version: u32, overlay_revision: u64) {
    let event = CapabilityEvent::tombstone(
        epoch_ms_now(),
        i64::from(catalog_version),
        i64::try_from(overlay_revision).unwrap_or(i64::MAX),
    );
    match usage.commit_capability_events_blocking(vec![event], 1) {
        BatchCommit::Committed { .. } => tracing::info!(
            catalog_version,
            overlay_revision,
            "committed fresh capability tombstone at boot (fail-closed replay boundary)"
        ),
        failure => tracing::error!(
            catalog_version,
            overlay_revision,
            reason = ?failure,
            "capability boot tombstone NOT committed; verdicts learned this session \
             may not survive a restart"
        ),
    }
}

/// Report the rebuild outcome: an `info` with the per-verdict tally, the row
/// cap, and the loaded-row count, plus a one-shot `warn` when the load hit the
/// cap (warm state may then be truncated to the newest `REBUILD_ROW_LIMIT`
/// post-boundary rows).
fn emit_rebuild_log(summary: &CapabilityRebuildSummary, loaded_rows: usize) {
    if loaded_rows == REBUILD_ROW_LIMIT {
        tracing::warn!(
            loaded_rows,
            row_cap = REBUILD_ROW_LIMIT,
            "capability warm rebuild hit the row cap; warm state may be truncated"
        );
    }
    tracing::info!(
        replayed_verified = summary.replayed_verified,
        replayed_negative = summary.replayed_negative,
        replayed_cleared = summary.replayed_cleared,
        cleared_noop = summary.cleared_noop,
        replayed_probe = summary.replayed_probe,
        skipped_unknown = summary.skipped_unknown,
        skipped_revision = summary.skipped_revision,
        skipped_vocab = summary.skipped_vocab,
        skipped_lane = summary.skipped_lane,
        skipped_owner = summary.skipped_owner,
        loaded_rows,
        row_cap = REBUILD_ROW_LIMIT,
        "warmed learned-capability registry from usage ledger"
    );
}

#[cfg(test)]
#[path = "capability_rebuild_tests.rs"]
mod tests;
