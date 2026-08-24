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
//! the post-boundary slice. Every other outcome fails closed -- replay
//! nothing AND enqueue exactly one fresh tombstone stamped this boot's
//! revision so this session's later events sit after a valid boundary:
//!   * no ledger yet (`Cold`), or an unreadable / version-too-new ledger;
//!   * a ledger with no tombstone at all;
//!   * a tombstone whose stamped revision differs from this boot's.

use std::path::Path;

use routectl_router::{CapabilityRebuildSummary, Router};
use routectl_usage::{CapabilityEvent, UsageHandle};

use super::ledger_reader::{
    BoundaryOutcome, LedgerCapabilityReader, REBUILD_ROW_LIMIT, classify_boundary, epoch_ms_now,
};

/// One-shot warm of the router's learned-capability registry from the usage
/// ledger at serve bootstrap.
///
/// Best-effort and fail-closed: reads this boot's revision from `router`,
/// classifies the ledger's tombstone boundary
/// ([`super::ledger_reader::classify_boundary`]), and either replays the
/// post-boundary slice through [`Router::rebuild_learned_from_ledger`] (on a
/// matching tombstone) or logs the case and enqueues one fresh tombstone
/// through `usage` (every fail-closed case). Never fails bootstrap.
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
        outcome => {
            log_fail_closed(&outcome, db_path, catalog_version, overlay_revision);
            enqueue_fresh_tombstone(usage, catalog_version, overlay_revision);
        }
    }
}

/// Log the fail-closed classification at the level its case warrants and no
/// higher: a cold ledger and an absent tombstone are the ordinary cold-start
/// shapes (debug), a revision mismatch is an expected post-reload / upgrade
/// transition (info), and only a genuinely unreadable ledger is a WARN.
fn log_fail_closed(
    outcome: &BoundaryOutcome,
    db_path: &Path,
    catalog_version: u32,
    overlay_revision: u64,
) {
    match outcome {
        // Unreachable in practice (the caller replays this case), listed for
        // exhaustiveness.
        BoundaryOutcome::Replay(_) => {}
        BoundaryOutcome::Cold => tracing::debug!(
            db_path = %db_path.display(),
            "no usage ledger yet; capability warm fails closed to a fresh tombstone (cold start)"
        ),
        BoundaryOutcome::NoTombstone => tracing::debug!(
            catalog_version,
            overlay_revision,
            "no capability tombstone in the ledger; failing closed and writing a fresh one"
        ),
        BoundaryOutcome::RevisionMismatch => tracing::info!(
            catalog_version,
            overlay_revision,
            "capability tombstone revision differs from this boot; \
             failing closed and writing a fresh tombstone"
        ),
        BoundaryOutcome::Unreadable(class) => tracing::warn!(
            db_path = %db_path.display(),
            reason = class,
            "usage ledger not readable during capability startup warm; \
             leaving registry empty and writing a fresh tombstone"
        ),
    }
}

/// Enqueue exactly one fresh tombstone stamped this boot's revision through
/// the usage writer. Non-blocking and best-effort like every usage write; the
/// enabled gate applies, and a disabled writer drops it (consistent with a
/// disabled ledger writing no events at all).
fn enqueue_fresh_tombstone(usage: &UsageHandle, catalog_version: u32, overlay_revision: u64) {
    let event = CapabilityEvent::tombstone(
        epoch_ms_now(),
        i64::from(catalog_version),
        i64::try_from(overlay_revision).unwrap_or(i64::MAX),
    );
    usage.try_send_capability_event(event);
    tracing::info!(
        catalog_version,
        overlay_revision,
        "enqueued fresh capability tombstone at boot (fail-closed replay boundary)"
    );
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
        loaded_rows,
        row_cap = REBUILD_ROW_LIMIT,
        "warmed learned-capability registry from usage ledger"
    );
}

#[cfg(test)]
#[path = "capability_rebuild_tests.rs"]
mod tests;
