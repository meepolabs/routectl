//! Ledger-backed warm rebuild of the per-lane token-estimate correction.
//!
//! On a fresh process start the in-memory [`CalibrationStore`] is empty, so
//! every lane falls back to the uncorrected estimate until live traffic
//! re-earns its correction -- and because that fallback IS the
//! pre-correction behavior, the loss is invisible: nothing errors, every
//! lane simply reads as not-yet-calibrated. This module replays a bounded
//! slice of the persisted evidence back through the SAME store write the
//! live path uses, so a restart does not un-learn a lane.
//!
//! The ledger lives in a leaf crate this crate does not depend on, so the
//! read is expressed through the [`CalibrationLedgerReader`]
//! dependency-inversion seam: a concrete reader bridging the usage crate to
//! the router-side row type is injected from the binary that owns both.
//!
//! Three rules keep the rebuilt state identical to what live traffic would
//! have produced:
//!
//! - Rows replay OLDEST-FIRST, so the per-lane ring retains the same
//!   samples arrival order would have left in it.
//! - Every row goes through [`CalibrationStore::record`] and is read back
//!   through the one reduction, so no second validation, bound or refusal
//!   path exists to drift from the live one.
//! - Freshness is judged against the clock at rebuild time, not against the
//!   row timestamps alone: a lane whose newest evidence is already past the
//!   age bound comes back uncorrected.
//!
//! A row whose nickname is absent from the current resolved-model table is
//! dropped: a history of renamed models would otherwise grow the lane map
//! with lanes that can never serve a request.

use std::time::SystemTime;

use super::factor::MAX_SAMPLE_AGE;
use super::store::{CalibrationStore, LaneKey, cohort_of};

/// One calibration-evidence row the rebuild consumes, in router-side terms.
///
/// `#[non_exhaustive]` so an additional column can be carried later without
/// a breaking change; the cross-crate reader builds rows through
/// [`CalibrationLedgerRow::new`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationLedgerRow {
    /// Wall-clock start time of the request the evidence came from.
    pub ts: SystemTime,
    /// Stable provider-kind token of the served target.
    pub provider_kind: String,
    /// Served model NICKNAME, never the upstream wire id. Keying this on the
    /// wire id would silently never match a lane the gate looks up.
    pub nickname: String,
    /// Inbound session identifier the request was recorded under, absent for
    /// a request that carried no recognized identity. Reduced to an opaque
    /// cohort tag and never stored.
    pub session_key: Option<String>,
    /// routectl's own byte-heuristic estimate for the dispatched payload.
    pub estimated_tokens: u64,
    /// The upstream's own cache-inclusive prompt total for the same request.
    pub prompt_tokens: u64,
}

impl CalibrationLedgerRow {
    /// Construct a row from the mapped ledger columns. Provided because the
    /// type is `#[non_exhaustive]`: the concrete
    /// [`CalibrationLedgerReader`] lives in a different crate and cannot use
    /// a struct literal.
    pub const fn new(
        ts: SystemTime,
        provider_kind: String,
        nickname: String,
        session_key: Option<String>,
        estimated_tokens: u64,
        prompt_tokens: u64,
    ) -> Self {
        Self {
            ts,
            provider_kind,
            nickname,
            session_key,
            estimated_tokens,
            prompt_tokens,
        }
    }
}

/// Dependency-inversion seam between the usage ledger and the calibration
/// store. The concrete implementation lives in the binary that depends on
/// both the usage crate and this one; this crate (and its tests) only ever
/// sees the trait. `Send + Sync` so a caller may hold one behind an `Arc`.
pub trait CalibrationLedgerReader: Send + Sync {
    /// Return up to `limit` evidence rows whose request start time is at or
    /// after `window_start`, oldest-first. Implementations perform the
    /// ledger IO and admit exactly the rows the live path would have
    /// recorded; a failed read returns no rows, never a partial set.
    fn read_calibration_samples(
        &self,
        window_start: SystemTime,
        limit: usize,
    ) -> Vec<CalibrationLedgerRow>;
}

/// Per-rebuild tally, for boot observability and pinned by tests.
///
/// `#[non_exhaustive]` so a further tally field can be added later without a
/// breaking change; construct through `Default` plus struct-update syntax.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CalibrationRebuildSummary {
    /// Rows the reader handed back. Equal to the cap means the read was
    /// truncated to the newest `limit` rows.
    pub rows_loaded: usize,
    /// Rows stored as lane evidence.
    pub accepted: usize,
    /// Rows dropped because the nickname no longer exists in the resolved
    /// model table (a renamed or removed model).
    pub rejected_unknown_nickname: usize,
    /// Rows dropped by the store's own pair validation -- the same refusal
    /// the live write applies.
    pub rejected_pair: usize,
    /// Lanes that produce a correction once the replay is in, judged against
    /// the rebuild's own clock.
    pub lanes_calibrated: usize,
}

impl CalibrationRebuildSummary {
    /// Construct a tally from its counts. Provided because the type is
    /// `#[non_exhaustive]`: a caller in another crate (the boot-observability
    /// log's own tests) cannot use a struct literal, and struct-update syntax
    /// is equally unavailable across the crate boundary.
    pub const fn new(
        rows_loaded: usize,
        accepted: usize,
        rejected_unknown_nickname: usize,
        rejected_pair: usize,
        lanes_calibrated: usize,
    ) -> Self {
        Self {
            rows_loaded,
            accepted,
            rejected_unknown_nickname,
            rejected_pair,
            lanes_calibrated,
        }
    }
}

/// Replay a ledger slice into `store`, returning the tally.
///
/// Reads the `[now - MAX_SAMPLE_AGE, now]` window (nothing older can survive
/// the reduction's age bound, so nothing older is worth loading), capped at
/// `limit` rows. `known_nickname` answers whether a nickname is still in the
/// resolved model table. Best-effort throughout: a reader that yields no
/// rows leaves every lane uncorrected, which is exactly today's behavior.
pub fn rebuild_into(
    reader: &dyn CalibrationLedgerReader,
    store: &CalibrationStore,
    known_nickname: &dyn Fn(&str) -> bool,
    now: SystemTime,
    limit: usize,
) -> CalibrationRebuildSummary {
    let window_start = now
        .checked_sub(MAX_SAMPLE_AGE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut rows = reader.read_calibration_samples(window_start, limit);
    // The reader already orders oldest-first; sorting here makes the replay
    // order a property of the rebuild rather than of one implementation. The
    // sort is stable, so rows sharing a timestamp keep the reader's order.
    rows.sort_by_key(|row| row.ts);

    let mut summary = CalibrationRebuildSummary {
        rows_loaded: rows.len(),
        ..CalibrationRebuildSummary::default()
    };
    for row in rows {
        if !known_nickname(&row.nickname) {
            summary.rejected_unknown_nickname += 1;
            continue;
        }
        let key = LaneKey {
            provider_kind: row.provider_kind,
            nickname: row.nickname,
        };
        let stored = store.record(
            key,
            row.estimated_tokens,
            row.prompt_tokens,
            cohort_of(row.session_key.as_deref()),
            row.ts,
        );
        if stored {
            summary.accepted += 1;
        } else {
            summary.rejected_pair += 1;
        }
    }
    summary.lanes_calibrated = store.calibrated_lane_count(now);
    summary
}

#[cfg(test)]
#[path = "rebuild_tests.rs"]
mod rebuild_tests;
