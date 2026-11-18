//! Per-seat latest-quota store and its per-window merge rule.
//!
//! # Why a plain map, and why it is bounded
//!
//! One [`AccountMeter`] per seat, behind one mutex. The keyspace is the OAuth
//! seat set the credential store declares and the loaded config admits -- it
//! is operator-declared, not client-driven, so a plain map cannot grow past
//! the seats EVER declared in this process. That is the same bound the
//! per-lane calibration store rests on, one dimension over: a lane is a
//! declared nickname, a seat here is a declared credential account.
//!
//! # The carry-over contract
//!
//! A hot-reload shares this store's `Arc` across the rebuild rather than
//! copying its entries (see `Router::carry_over_quota_from`), so a write
//! racing the swap lands on the one map both the outgoing and incoming
//! Router read -- neither lost nor written to a copy nobody reads. Sharing
//! means insertion admission ([`QuotaStore::admit_seats`]) is a WRITE-TIME
//! gate re-declared from the new config on every rebuild, not an
//! import-time filter: a seat's stale reading is actively dropped from the
//! map the moment `admit_seats` no longer names it (folded into
//! `admit_seats` itself, so both the boot install path and the reload
//! carry-over path prune through the one call), and a write for a
//! not-currently-admitted seat is refused outright and counted
//! ([`QuotaStore::refused_by_admission_total`]). The keyspace bound is
//! therefore "seats ever declared in this process, minus whatever the most
//! recent `admit_seats` call pruned" -- operator-action-bounded, never
//! traffic-bounded, reset on restart.
//!
//! Two shapes in this crate were deliberately NOT copied. The calibration
//! store keeps a bounded RING per lane because its reduction takes a median
//! over recent history; quota has no reduction -- the freshest reading is the
//! answer and an older one is not evidence about the current window, so a ring
//! would only cost memory. The K tracker keeps an LRU because its key is a
//! client-supplied session; nothing here is client-supplied, so there is no
//! eviction policy to get wrong.
//!
//! # The merge rule
//!
//! Each account holds one window per reported length and scope; the merge
//! rule lives in [`super::meter`]. Two entry points feed it:
//! [`QuotaStore::observe_account`] takes a validated, source-tagged
//! observation, and [`QuotaStore::observe`] adapts one response-header
//! snapshot onto a partial observation of the curated windows. Both are
//! refused whole when a reading contradicts the stored one for its window,
//! and the refusal is counted.
//!
//! [`QuotaStore::reading_for`] projects an account back onto the two-role
//! snapshot placement reads, applying read-time expiry.
//!
//! # Log hygiene
//!
//! A rejected observation is counted by fixed reason and reported through one
//! rate-limited WARN carrying the running totals. Nothing here logs a raw
//! header, a quota `extras` pair, a credential, a session key, an account id
//! or email, a token, or any part of a body: the only identifier that reaches
//! a log line is the routectl-internal account key, and the only figures are
//! counters.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use super::freshness::ObservationStamp;
use super::key::SeatKey;
use super::meter::AccountMeter;
#[cfg(test)]
use super::meter::ReportState;
use super::reading::AccountObservation;
use super::reduce::{QuotaSnapshot, RejectionReason};
use super::view::{AccountReading, AccountReadings};

/// How long a stored reading stays authoritative on the MONOTONIC clock,
/// independent of its own reset.
///
/// The backstop half of the two freshness bounds: a seat that stopped
/// receiving traffic must not keep answering from an old reading just because
/// that reading's wall-clock reset has not arrived, and a wall clock stepped
/// backwards must not make a reading immortal. Sized above the longest curated
/// window so it never pre-empts the semantic bound for a seat under live
/// traffic; the reset is what normally expires a window.
pub const MAX_OBSERVATION_AGE: Duration = Duration::from_hours(24 * 8);

/// Minimum seconds between two rejected-observation WARNs in one process.
///
/// A seat producing malformed metadata on every response must not turn a
/// diagnostic into an unbounded log stream. The counters stay exact regardless;
/// the WARN reports their running totals.
const REJECTION_WARN_INTERVAL_SECS: u64 = 300;

/// Exact per-reason rejection counters plus the throttle stamp for their
/// shared WARN.
///
/// The reason vocabulary is [`RejectionReason`], owned by the reducer that
/// DECIDES a refusal rather than restated here: a second enum would let the two
/// drift, and a reason the reducer can produce but this store cannot count is
/// exactly the silent gap the counters exist to close.
#[derive(Debug, Default)]
struct RejectionCounters {
    invalid_utilization: AtomicU64,
    expired_reset: AtomicU64,
    implausible_reset: AtomicU64,
    overflow: AtomicU64,
    contradictory: AtomicU64,
    unknown_length: AtomicU64,
    invalid_started: AtomicU64,
    malformed_body: AtomicU64,
    last_warn_epoch_secs: AtomicU64,
}

impl RejectionCounters {
    /// Count one rejection and return the reason's new running total.
    fn incr(&self, reason: RejectionReason) -> u64 {
        let counter = match reason {
            RejectionReason::InvalidUtilization => &self.invalid_utilization,
            RejectionReason::ExpiredReset => &self.expired_reset,
            RejectionReason::ImplausibleReset => &self.implausible_reset,
            RejectionReason::Overflow => &self.overflow,
            RejectionReason::Contradictory => &self.contradictory,
            RejectionReason::UnknownLength => &self.unknown_length,
            RejectionReason::InvalidStarted => &self.invalid_started,
            RejectionReason::MalformedBody => &self.malformed_body,
        };
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Every reason's running total, in the fixed reason order.
    fn totals(&self) -> RejectionTotals {
        RejectionTotals {
            invalid_utilization: self.invalid_utilization.load(Ordering::Relaxed),
            expired_reset: self.expired_reset.load(Ordering::Relaxed),
            implausible_reset: self.implausible_reset.load(Ordering::Relaxed),
            overflow: self.overflow.load(Ordering::Relaxed),
            contradictory: self.contradictory.load(Ordering::Relaxed),
            unknown_length: self.unknown_length.load(Ordering::Relaxed),
            invalid_started: self.invalid_started.load(Ordering::Relaxed),
            malformed_body: self.malformed_body.load(Ordering::Relaxed),
        }
    }

    /// Claim the right to emit one WARN at `now_secs`, or refuse because the
    /// interval has not elapsed. One compare-and-swap, so concurrent claimants
    /// yield exactly one winner; `saturating_sub` makes a backwards clock jump
    /// suppress rather than re-open the window.
    fn claim_warn(&self, now_secs: u64) -> bool {
        let last = self.last_warn_epoch_secs.load(Ordering::Relaxed);
        if now_secs.saturating_sub(last) < REJECTION_WARN_INTERVAL_SECS {
            return false;
        }
        self.last_warn_epoch_secs
            .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

/// Why an observation was not merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveRefusal {
    /// The account is not in the configured seat set.
    NotAdmitted,
    /// The merge refused the observation for this reason.
    Rejected(RejectionReason),
}

/// Running rejection totals, partitioned by the fixed reason set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RejectionTotals {
    /// Windows refused for a utilization outside the utilization scale.
    pub invalid_utilization: u64,
    /// Windows refused for a reset already passed at observation.
    pub expired_reset: u64,
    /// Windows refused for a reset beyond the window's own length.
    pub implausible_reset: u64,
    /// Windows refused because a bound could not be computed.
    pub overflow: u64,
    /// Readings refused for contradicting the stored reading of their window.
    pub contradictory: u64,
    /// Windows refused for a length that cannot be classified.
    pub unknown_length: u64,
    /// Windows refused for a started state contradicting usage or reset.
    pub invalid_started: u64,
    /// Observations refused for a body or window set of the wrong shape.
    pub malformed_body: u64,
}

/// Seconds since the Unix epoch, `0` on a pre-epoch clock (which then
/// suppresses rather than emits -- the safe direction for a bounded WARN).
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Per-seat latest-quota readings. Interior-locked, so the post-response feed
/// and the placement read both work through a shared reference.
#[derive(Debug, Default)]
pub struct QuotaStore {
    seats: Mutex<HashMap<SeatKey, AccountMeter>>,
    /// The seat keys insertion admits. Empty until
    /// [`QuotaStore::admit_seats`] declares the configured set, and an empty
    /// set admits NOTHING -- a store nobody declared seats for holds no
    /// readings rather than accepting every identity that turns up.
    admitted: Mutex<HashSet<SeatKey>>,
    rejections: RejectionCounters,
    /// Writes refused because their seat is not in the currently admitted
    /// set -- see [`QuotaStore::refused_by_admission_total`].
    refused_by_admission: AtomicU64,
}

impl QuotaStore {
    /// Declare the configured seat set this store will hold readings for,
    /// and prune any reading this store already holds for a seat outside
    /// it.
    ///
    /// Called from `install_resolved_models` on every Router build (so the
    /// keyspace bound is the config's rather than the traffic's) and again
    /// from `Router::carry_over_quota_from` after the store's `Arc` is
    /// shared with the outgoing Router, so a seat the new config dropped
    /// cannot keep answering from a reading a past config's traffic wrote.
    ///
    /// The prune builds the new admitted set locally, then swaps it into
    /// `admitted` FIRST and prunes `seats` SECOND (two sequential
    /// acquisitions, never held together here). Ordering matters: a
    /// concurrent [`QuotaStore::observe`] holds `seats` across ITS
    /// admission check (see there), so any `observe` that raced ahead of
    /// the swap sees the OLD admitted set but still serializes on `seats`
    /// with this prune -- its insert either lands before the prune (and
    /// is then pruned away) or after the swap (and is refused outright).
    /// There is no window where a write for a seat this call retires
    /// survives.
    pub fn admit_seats(&self, seats: impl IntoIterator<Item = SeatKey>) {
        let new_admitted: HashSet<SeatKey> = seats.into_iter().collect();
        *self.admitted.lock() = new_admitted.clone();
        let mut guard = self.seats.lock();
        guard.retain(|key, _| new_admitted.contains(key));
    }

    /// Whether `key` is in the configured seat set.
    pub fn admits(&self, key: &SeatKey) -> bool {
        self.admitted.lock().contains(key)
    }

    /// Refuse `key` as [`ObserveRefusal::NotAdmitted`], counted like a refused
    /// write, when it is outside the configured seat set. A gate for a caller
    /// that must refuse before doing any other work for the account; the
    /// merge re-checks under its own lock.
    pub fn check_admission(&self, key: &SeatKey) -> Result<(), ObserveRefusal> {
        if self.admits(key) {
            return Ok(());
        }
        self.refused_by_admission.fetch_add(1, Ordering::Relaxed);
        Err(ObserveRefusal::NotAdmitted)
    }

    /// Merge one response-header snapshot into `key`'s meter.
    ///
    /// The snapshot's known windows become a partial observation of the
    /// curated windows (see [`AccountMeter`]); an unknown window is omitted
    /// and never erases. Refused outright -- no entry created, nothing merged
    /// -- when `key` is outside the configured seat set, and refused whole and
    /// counted when a window contradicts the stored one.
    ///
    /// Returns whether the snapshot was merged, which the feed's tests read;
    /// the production call site ignores it.
    pub fn observe(&self, key: &SeatKey, observed: QuotaSnapshot) -> bool {
        self.merge(key, observed.observed, |meter| {
            meter.merged_header(&observed)
        })
        .is_ok()
    }

    /// Merge one validated observation into `key`'s meter.
    ///
    /// Refused as [`ObserveRefusal::NotAdmitted`] (counted in
    /// [`QuotaStore::refused_by_admission_total`], no entry created) when
    /// `key` is outside the configured seat set, and as
    /// [`ObserveRefusal::Rejected`] (counted by reason, stored state
    /// unchanged) when the merge refuses it.
    pub fn observe_account(
        &self,
        key: &SeatKey,
        observation: &AccountObservation,
    ) -> Result<(), ObserveRefusal> {
        self.merge(key, *observation.observed(), |meter| {
            meter.merged(observation)
        })
    }

    /// Apply `merge` to `key`'s meter (a fresh one stamped `observed` when it
    /// has none) and store the result.
    ///
    /// Holds `seats` across BOTH the admission check and the insert (nesting
    /// `admitted` inside it via a brief, immediately-released acquisition)
    /// so this serializes against [`QuotaStore::admit_seats`]'s prune: this
    /// is the ONLY method that nests the two locks, and always in this one
    /// direction (`seats` outer, `admitted` inner) -- `admit_seats` itself
    /// takes them strictly sequentially, never together. That one-directional
    /// discipline is what closes the TOCTOU: there is no interleaving where a
    /// retired seat's write survives a concurrent prune.
    fn merge(
        &self,
        key: &SeatKey,
        observed: ObservationStamp,
        merge: impl FnOnce(&AccountMeter) -> Result<AccountMeter, RejectionReason>,
    ) -> Result<(), ObserveRefusal> {
        let mut guard = self.seats.lock();
        if !self.admits(key) {
            self.refused_by_admission.fetch_add(1, Ordering::Relaxed);
            return Err(ObserveRefusal::NotAdmitted);
        }
        let merged = match guard.get(key) {
            Some(stored) => merge(stored),
            None => merge(&AccountMeter::new(observed)),
        };
        match merged {
            Ok(meter) => {
                guard.insert(key.clone(), meter);
                Ok(())
            }
            Err(reason) => {
                drop(guard);
                self.record_rejection(key, reason);
                Err(ObserveRefusal::Rejected(reason))
            }
        }
    }

    /// `key`'s reading projected onto today's two-role snapshot as of `now`,
    /// with every window past its own bounds reported `Unknown` rather than
    /// at its last value.
    ///
    /// FAST is the account-wide window of the curated five-hour length; SLOW
    /// the account-wide window of the curated long length. `None` for a seat
    /// with no entry at all. A seat WITH an entry whose every window has
    /// expired answers `Some` with both windows `Unknown`, which a caller
    /// treats exactly as no evidence -- the distinction exists so a
    /// diagnostic can tell "never observed" from "observed and expired".
    pub fn reading_for(&self, key: &SeatKey, now: &ObservationStamp) -> Option<QuotaSnapshot> {
        self.seats
            .lock()
            .get(key)
            .map(|meter| meter.projection(now))
    }

    /// A copy of `key`'s meter, or `None` when it has no entry.
    #[cfg(test)]
    pub fn account(&self, key: &SeatKey) -> Option<AccountMeter> {
        self.seats.lock().get(key).cloned()
    }

    /// Every configured account's readings at `now`, in account order, with
    /// the running refusal totals. One lock acquisition each for the admitted
    /// set and the meters, never held together.
    pub fn readings(&self, now: &ObservationStamp) -> AccountReadings {
        let mut admitted: Vec<SeatKey> = self.admitted.lock().iter().cloned().collect();
        admitted.sort();
        let accounts = {
            let seats = self.seats.lock();
            admitted
                .iter()
                .map(|key| AccountReading::of(key.as_str(), seats.get(key), now))
                .collect()
        };
        AccountReadings {
            accounts,
            rejections: self.rejections.totals(),
            refused_by_admission: self.refused_by_admission_total(),
        }
    }

    /// What `key` has reported: `NoReading` when it has no entry.
    #[cfg(test)]
    pub fn report_for(&self, key: &SeatKey) -> ReportState {
        self.seats
            .lock()
            .get(key)
            .map_or(ReportState::NoReading, AccountMeter::report)
    }

    /// Count one rejected window by reason, emitting at most one throttled
    /// WARN per interval carrying every reason's running total.
    ///
    /// The account key is a routectl-internal identifier; no header, `extras`
    /// pair, credential, token, session key or body byte reaches the line.
    pub fn record_rejection(&self, key: &SeatKey, reason: RejectionReason) {
        self.rejections.incr(reason);
        if !self.rejections.claim_warn(now_epoch_secs()) {
            return;
        }
        let totals = self.rejections.totals();
        tracing::warn!(
            event = "quota_observation_rejected",
            seat = %key.as_str(),
            reason = ?reason,
            invalid_utilization_total = totals.invalid_utilization,
            expired_reset_total = totals.expired_reset,
            implausible_reset_total = totals.implausible_reset,
            overflow_total = totals.overflow,
            contradictory_total = totals.contradictory,
            unknown_length_total = totals.unknown_length,
            invalid_started_total = totals.invalid_started,
            malformed_body_total = totals.malformed_body,
            "upstream subscription-quota observation rejected; prior accepted state kept",
        );
    }

    /// Running rejection totals, partitioned by reason.
    #[cfg(test)]
    pub fn rejection_totals(&self) -> RejectionTotals {
        self.rejections.totals()
    }

    /// Number of seats holding a reading. Test-only: nothing in production
    /// asks how many seats have reported, only what a given seat reports.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.seats.lock().len()
    }

    /// True when no seat holds a reading -- the state a process restart starts
    /// in, since nothing rebuilds this store from the ledger. Test-only, for
    /// the same reason as [`QuotaStore::len`].
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.seats.lock().is_empty()
    }

    /// Cumulative writes refused because their seat was not in the currently
    /// admitted set.
    ///
    /// A nonzero value right after a reload is EXPECTED and bounded: it
    /// counts a retired seat's late write (one still in flight against the
    /// outgoing Router when the carry-over re-declared admission) being
    /// correctly refused. A large or steadily GROWING value outside that
    /// window is the signal worth acting on -- it means a seat this Router
    /// should be serving is not in the admitted set, which is exactly the
    /// symptom of a missed re-admit after a config change.
    pub fn refused_by_admission_total(&self) -> u64 {
        self.refused_by_admission.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
