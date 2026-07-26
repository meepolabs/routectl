//! Startup warm of the learned-capability registry from the usage ledger.
//!
//! On a fresh process start the router's learned-capability registry is
//! empty, so every capability the router previously learned as unsupported
//! (or confirmed working, or probe-settled) would have to be re-learned from
//! live traffic. This module bridges the leaf usage crate to the router-side
//! [`CapabilityLedgerReader`] seam (which the router crate cannot satisfy
//! itself, since it does not depend on the usage crate) and runs a one-shot
//! rebuild at the serve bootstrap, mirroring the K-estimator warm in
//! [`super::k_rebuild`].
//!
//! The warm runs ONLY at the initial bootstrap, never on a hot-reload:
//! `Router::carry_over_learned_from` already carries (or deliberately clears)
//! the live registry across a reload, so re-running the rebuild there would
//! clobber that decision.
//!
//! # Read posture and fail-closed boundary
//!
//! The ledger is opened read-only per call (never created or migrated -- the
//! daemon is the sole writer and may be live). A tombstone row marks the
//! correctness boundary: the replayer trusts only rows after it, and only
//! when its stamped revision matches this boot's catalog / overlay revision.
//! Three cases fail closed -- replay nothing AND enqueue exactly one fresh
//! tombstone stamped this boot's revision so this session's later events sit
//! after a valid boundary:
//!   * no ledger yet (`NoData`), or an unreadable / version-too-new ledger;
//!   * a ledger with no tombstone at all;
//!   * a tombstone whose stamped revision differs from this boot's.
//!
//! Only a matching tombstone replays the post-boundary slice.
//!
//! # Clock map
//!
//! The persisted `ts` is epoch-milliseconds; the registry keys decay on the
//! monotonic [`Instant`] clock. This bridge owns both clocks, so it maps each
//! event: `event_instant = now - (now_ms - event_ms)`, with the age clamped
//! to zero so a future-dated row lands at `now` (never underflowing the
//! duration). An event older than its decay window maps to a far-past instant
//! and so to an already-expired entry, which lapses to a single re-probe --
//! decay-across-restart falls out of the map for free. `checked_sub` guards
//! the one platform where an age beyond the monotonic origin is
//! unrepresentable, degrading that row to `now` rather than panicking.
//!
//! # Lane-key contract
//!
//! The ledger stores `lane_key` + normalized `capability`; the router's
//! registry key is `(state_key, normalized_feature_key)`. `lane_key` IS the
//! `state_key`, and the persisted `capability` is already the normalized
//! feature key, so the map is direct. `provider_kind` only feeds the router's
//! idempotent capability normalization on replay; because the persisted key
//! is already normalized, an empty provider is inert and reconstructs the
//! identical registry key.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use routectl_router::{
    CapabilityEventRow as ReplayRow, CapabilityLedgerReader, CapabilityRebuildSummary,
    ReplayTombstone, Router,
};
use routectl_usage::{
    CapabilityEvent, CapabilityEventRow as LedgerRow, OpenError, TombstoneRow, UsageHandle,
    latest_tombstone, open_readonly, read_capability_events_after,
};

/// Upper bound on the number of ledger rows the startup rebuild reads. A
/// plain compile-time cap (never derived from runtime input) on the boot
/// read, matching the K-estimator warm precedent. A full result is treated
/// as possibly truncated and warned.
const REBUILD_ROW_LIMIT: usize = 5000;

/// One-shot warm of the router's learned-capability registry from the usage
/// ledger at serve bootstrap.
///
/// Best-effort and fail-closed: reads this boot's revision from `router`,
/// compares the ledger's tombstone boundary, and either replays the
/// post-boundary slice through [`Router::rebuild_learned_from_ledger`] (on a
/// matching tombstone) or replays nothing and enqueues one fresh tombstone
/// through `usage` (every fail-closed case). Never fails bootstrap.
pub(crate) fn warm_capability_registry_from_ledger(
    db_path: &Path,
    router: &Router,
    usage: &UsageHandle,
) {
    let catalog_version = router.catalog_version();
    let overlay_revision = router.overlay_revision();

    match resolve_boundary(db_path, catalog_version, overlay_revision) {
        Boundary::Replay(tombstone) => {
            let reader = LedgerCapabilityReader::new(db_path.to_path_buf(), tombstone);
            let summary = router.rebuild_learned_from_ledger(&reader);
            emit_rebuild_log(&summary, reader.loaded_rows.load(Ordering::Relaxed));
        }
        Boundary::FailClosed => {
            enqueue_fresh_tombstone(usage, catalog_version, overlay_revision);
        }
    }
}

/// The resolved boot boundary: either a matching tombstone whose post-slice
/// is replayed, or the fail-closed verdict (replay nothing, write a fresh
/// tombstone).
enum Boundary {
    Replay(ReplayTombstone),
    FailClosed,
}

/// Read the ledger's latest tombstone read-only and classify it against this
/// boot's revision. `NoData` is the legitimately-cold ledger (silent); a read
/// error or version mismatch leaves the registry empty with a WARN; both
/// still fail closed so the caller writes a fresh boundary.
fn resolve_boundary(db_path: &Path, catalog_version: u32, overlay_revision: u64) -> Boundary {
    let db = match open_readonly(db_path) {
        Ok(db) => db,
        Err(OpenError::NoData { .. }) => {
            tracing::debug!(
                db_path = %db_path.display(),
                "no usage ledger yet; capability warm fails closed to a fresh tombstone (cold start)"
            );
            return Boundary::FailClosed;
        }
        Err(e) => {
            tracing::warn!(
                db_path = %db_path.display(),
                error = %e,
                "usage ledger not readable during capability startup warm; \
                 leaving registry empty and writing a fresh tombstone"
            );
            return Boundary::FailClosed;
        }
    };

    match latest_tombstone(db.conn()) {
        Ok(Some(t)) if tombstone_matches(&t, catalog_version, overlay_revision) => {
            Boundary::Replay(ReplayTombstone::new(
                t.rowid,
                catalog_version,
                overlay_revision,
            ))
        }
        Ok(Some(_)) => {
            tracing::info!(
                catalog_version,
                overlay_revision,
                "capability tombstone revision differs from this boot; \
                 failing closed and writing a fresh tombstone"
            );
            Boundary::FailClosed
        }
        Ok(None) => {
            tracing::debug!(
                catalog_version,
                overlay_revision,
                "no capability tombstone in the ledger; failing closed and writing a fresh one"
            );
            Boundary::FailClosed
        }
        Err(e) => {
            tracing::warn!(
                db_path = %db_path.display(),
                error = %e,
                "capability tombstone read failed during startup warm; \
                 leaving registry empty and writing a fresh tombstone"
            );
            Boundary::FailClosed
        }
    }
}

/// Whether a persisted tombstone was stamped with this boot's exact revision.
/// A `NULL` or unrepresentable stored revision never matches (fail closed).
fn tombstone_matches(t: &TombstoneRow, catalog_version: u32, overlay_revision: u64) -> bool {
    t.catalog_version == Some(i64::from(catalog_version))
        && t.overlay_revision == i64::try_from(overlay_revision).ok()
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

/// Bridges the leaf usage ledger to the router-side [`CapabilityLedgerReader`]
/// seam for the matching-tombstone replay path.
///
/// Holds the resolved boundary and the two clock anchors captured once at
/// construction (so every mapped row shares one `now`), and opens a fresh
/// read-only connection per `read_events` call -- the rebuild calls it once at
/// startup, so a per-call open avoids holding a connection for the daemon's
/// life.
struct LedgerCapabilityReader {
    db_path: PathBuf,
    tombstone: ReplayTombstone,
    now: Instant,
    now_ms: i64,
    loaded_rows: AtomicUsize,
}

impl LedgerCapabilityReader {
    fn new(db_path: PathBuf, tombstone: ReplayTombstone) -> Self {
        Self {
            db_path,
            tombstone,
            now: Instant::now(),
            now_ms: epoch_ms_now(),
            loaded_rows: AtomicUsize::new(0),
        }
    }

    /// Map one ledger row into the router-side replay row, applying the clock
    /// map and the lane-key contract. A row missing a required column
    /// (`lane_key` / `capability` / `verdict` / `source` / revision) is
    /// malformed and dropped -- the replayer stays open-set tolerant on token
    /// VALUES, but cannot construct a row from absent identity columns.
    fn map_row(&self, row: LedgerRow) -> Option<ReplayRow> {
        let state_key = row.lane_key?;
        let capability = row.capability?;
        let verdict = row.verdict?;
        let source = row.source?;
        let catalog_version = row.catalog_version.and_then(|v| u32::try_from(v).ok())?;
        let overlay_revision = row.overlay_revision.and_then(|v| u64::try_from(v).ok())?;
        let observed_at = map_instant(self.now, self.now_ms, row.ts);

        Some(ReplayRow::new(
            row.rowid,
            observed_at,
            verdict,
            row.phase,
            source,
            row.tier,
            row.evidence_class,
            capability,
            state_key,
            // Inert on replay: the persisted capability is already normalized
            // and the registry key carries no provider dimension (see the
            // lane-key contract in the module docs).
            String::new(),
            catalog_version,
            overlay_revision,
        ))
    }
}

impl CapabilityLedgerReader for LedgerCapabilityReader {
    fn tombstone(&self) -> Option<ReplayTombstone> {
        Some(self.tombstone)
    }

    fn read_events(&self) -> Vec<ReplayRow> {
        // A fresh open here can race the resolve-time open (the daemon is the
        // sole writer and may be live); a failure now yields no rows rather
        // than a panic, and the warm degrades to a cold registry.
        let db = match open_readonly(&self.db_path) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(
                    db_path = %self.db_path.display(),
                    error = %e,
                    "usage ledger became unreadable between boundary read and event read; \
                     leaving registry empty"
                );
                return Vec::new();
            }
        };

        match read_capability_events_after(db.conn(), self.tombstone.rowid, REBUILD_ROW_LIMIT) {
            Ok(rows) => {
                let loaded = rows.len();
                self.loaded_rows.store(loaded, Ordering::Relaxed);
                rows.into_iter().filter_map(|r| self.map_row(r)).collect()
            }
            Err(e) => {
                tracing::warn!(
                    db_path = %self.db_path.display(),
                    error = %e,
                    "capability event read failed during startup warm; leaving registry empty"
                );
                Vec::new()
            }
        }
    }
}

/// Map an epoch-millisecond event timestamp onto the monotonic clock relative
/// to `now`. The age is clamped to zero (a future-dated row lands at `now`,
/// never underflowing the duration); an older event maps into the past (an
/// already-expired entry). `checked_sub` degrades an age beyond the monotonic
/// origin to `now` rather than panicking on platforms where it is
/// unrepresentable.
fn map_instant(now: Instant, now_ms: i64, event_ms: i64) -> Instant {
    let age_ms = u64::try_from(now_ms.saturating_sub(event_ms).max(0)).unwrap_or(0);
    now.checked_sub(Duration::from_millis(age_ms))
        .unwrap_or(now)
}

/// Current wall-clock time in epoch milliseconds, saturating rather than
/// panicking on a pre-epoch or overflowing clock. Shared with the hot-reload
/// tombstone seam so both replay boundaries stamp the same clock basis.
pub(super) fn epoch_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
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
        skipped_probe = summary.skipped_probe,
        skipped_unknown = summary.skipped_unknown,
        loaded_rows,
        row_cap = REBUILD_ROW_LIMIT,
        "warmed learned-capability registry from usage ledger"
    );
}

#[cfg(test)]
#[path = "capability_rebuild_tests.rs"]
mod tests;
