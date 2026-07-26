//! Shared read-only bridge from the usage capability-event ledger to the
//! router-side learned-capability replay.
//!
//! Two callers construct the SAME reader against the SAME clock map and
//! lane-key contract: the serve bootstrap warm
//! ([`super::capability_rebuild`]) and the offline doctor gather
//! ([`crate::commands::doctor`]). Lifting the bridge here keeps a single
//! replay-and-clock-map implementation -- a second copy would let the two
//! surfaces drift on the future-clamp, the row cap, or the lane-key map.
//!
//! # Read posture and boundary
//!
//! The ledger is opened read-only per call (never created or migrated -- the
//! daemon is the sole writer and may be live). A tombstone row marks the
//! correctness boundary: the replayer trusts only rows after it, and only
//! when its stamped revision matches the caller's catalog / overlay revision.
//! [`classify_boundary`] resolves this read-only and PURELY (no logging, no
//! writes): the serve warm collapses every non-`Replay` outcome to a fresh
//! tombstone write, while the doctor maps each to a first-class availability
//! state. Neither action lives here -- this module owns the read, not the
//! reaction.
//!
//! # Clock map
//!
//! The persisted `ts` is epoch-milliseconds; the registry keys decay on the
//! monotonic [`Instant`] clock. The reader owns both clocks, captured once at
//! construction (so every mapped row shares one `now`), and maps each event:
//! `event_instant = now - (now_ms - event_ms)`, with the age clamped to zero
//! so a future-dated row lands at `now` (never underflowing the duration). An
//! event older than its decay window maps to a far-past instant and so to an
//! already-expired entry. `checked_sub` guards the one platform where an age
//! beyond the monotonic origin is unrepresentable, degrading that row to
//! `now` rather than panicking.
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

use routectl_router::{CapabilityEventRow as ReplayRow, CapabilityLedgerReader, ReplayTombstone};
use routectl_usage::{
    CapabilityEventRow as LedgerRow, OpenError, TombstoneRow, latest_tombstone, open_readonly,
    read_capability_events_after,
};

/// Upper bound on the number of ledger rows a single rebuild reads. A plain
/// compile-time cap (never derived from runtime input), matching the
/// K-estimator warm precedent. A full result is treated as possibly truncated
/// and warned by the warm caller.
pub(crate) const REBUILD_ROW_LIMIT: usize = 5000;

/// The read-only outcome of resolving the replay boundary against a target
/// revision. Pure classification: it preserves WHY a boundary did not match
/// so the diagnostic surface can distinguish a cold ledger from an unreadable
/// one, where the serve boot only needs "replay or fail closed".
pub(crate) enum BoundaryOutcome {
    /// A tombstone stamped the target's exact revision: replay its post-slice.
    Replay(ReplayTombstone),
    /// No ledger yet (`NoData`): the legitimately-cold ledger.
    Cold,
    /// An otherwise-readable ledger carrying no tombstone at all.
    NoTombstone,
    /// A tombstone stamped a revision other than the target's.
    RevisionMismatch,
    /// The ledger could not be opened, or its boundary could not be read; the
    /// token is a path-free failure class (see [`open_error_class`], plus the
    /// `tombstone_read` class for a boundary-query failure).
    Unreadable(&'static str),
}

/// Path-free class token for a usage-DB open failure. Several `OpenError`
/// variants embed the DB PATH in their Display, so any logging or diagnostic
/// site must emit this fixed class rather than the Display. A new variant is a
/// compile error here, forcing a deliberate log-hygiene decision for any new
/// failure mode.
pub(crate) const fn open_error_class(err: &OpenError) -> &'static str {
    match err {
        OpenError::CreateDir { .. } => "create_dir",
        OpenError::Open { .. } => "open",
        OpenError::Pragma(_) => "pragma",
        OpenError::Permissions { .. } => "permissions",
        OpenError::Migrate(_) => "migrate",
        OpenError::VersionTooNew { .. } => "version_too_new",
        OpenError::NotWal { .. } => "not_wal",
        OpenError::NoData { .. } | OpenError::VersionTooOld { .. } => "expected",
    }
}

/// Resolve the replay boundary read-only, classifying it against the target
/// `catalog_version` / `overlay_revision`. Opens the ledger read-only (never
/// creating or migrating -- the daemon is the sole writer and may be live)
/// and reads its latest tombstone. The classification is pure so each caller
/// applies its own action (write a fresh tombstone / render an availability
/// state).
pub(crate) fn classify_boundary(
    db_path: &Path,
    catalog_version: u32,
    overlay_revision: u64,
) -> BoundaryOutcome {
    let db = match open_readonly(db_path) {
        Ok(db) => db,
        Err(OpenError::NoData { .. }) => return BoundaryOutcome::Cold,
        Err(e) => return BoundaryOutcome::Unreadable(open_error_class(&e)),
    };

    match latest_tombstone(db.conn()) {
        Ok(Some(t)) if tombstone_matches(&t, catalog_version, overlay_revision) => {
            BoundaryOutcome::Replay(ReplayTombstone::new(
                t.rowid,
                catalog_version,
                overlay_revision,
            ))
        }
        Ok(Some(_)) => BoundaryOutcome::RevisionMismatch,
        Ok(None) => BoundaryOutcome::NoTombstone,
        Err(_) => BoundaryOutcome::Unreadable("tombstone_read"),
    }
}

/// Whether a persisted tombstone was stamped with the target revision. A
/// `NULL` or unrepresentable stored revision never matches (fail closed).
fn tombstone_matches(t: &TombstoneRow, catalog_version: u32, overlay_revision: u64) -> bool {
    t.catalog_version == Some(i64::from(catalog_version))
        && t.overlay_revision == i64::try_from(overlay_revision).ok()
}

/// Bridges the leaf usage ledger to the router-side [`CapabilityLedgerReader`]
/// seam for the matching-tombstone replay path.
///
/// Holds the resolved boundary and the two clock anchors captured once at
/// construction (so every mapped row shares one `now`), and opens a fresh
/// read-only connection per `read_events` call -- both callers read once, so
/// a per-call open avoids holding a connection open beyond the read.
pub(crate) struct LedgerCapabilityReader {
    db_path: PathBuf,
    tombstone: ReplayTombstone,
    now: Instant,
    now_ms: i64,
    loaded_rows: AtomicUsize,
}

impl LedgerCapabilityReader {
    pub(crate) fn new(db_path: PathBuf, tombstone: ReplayTombstone) -> Self {
        Self {
            db_path,
            tombstone,
            now: Instant::now(),
            now_ms: epoch_ms_now(),
            loaded_rows: AtomicUsize::new(0),
        }
    }

    /// The single `Instant` anchor every mapped row was taken against. A
    /// consumer deriving cell ages MUST use this same anchor so one skew-free
    /// basis covers the whole snapshot.
    pub(crate) const fn now(&self) -> Instant {
        self.now
    }

    /// The epoch-millisecond anchor paired with [`Self::now`].
    pub(crate) const fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// The number of rows the most recent `read_events` loaded, for the warm
    /// caller's row-cap observability.
    pub(crate) fn loaded_rows(&self) -> usize {
        self.loaded_rows.load(Ordering::Relaxed)
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
        // A fresh open here can race the classify-time open (the daemon is the
        // sole writer and may be live); a failure now yields no rows rather
        // than a panic, and the rebuild degrades to an empty registry.
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
                    "capability event read failed; leaving registry empty"
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
pub(crate) fn map_instant(now: Instant, now_ms: i64, event_ms: i64) -> Instant {
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

#[cfg(test)]
#[path = "ledger_reader_tests.rs"]
mod tests;
