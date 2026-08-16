//! Per-seat latest-quota store and its per-window merge rule.
//!
//! # Why a plain map, and why it is bounded
//!
//! One [`QuotaSnapshot`] per seat, behind one mutex. The keyspace is the OAuth
//! seat set the credential store declares and the loaded config admits -- it
//! is operator-declared, not client-driven, so a plain map cannot grow past
//! the configured seats. That is the same bound the per-lane calibration store
//! rests on, one dimension over: a lane is a declared nickname, a seat here is
//! a declared credential account. Insertion additionally refuses a key outside
//! the configured seat set, so a renamed or removed seat cannot leave an entry
//! behind across a reload, and a stray identity cannot mint one at all.
//!
//! Two shapes in this crate were deliberately NOT copied. The calibration
//! store keeps a bounded RING per lane because its reduction takes a median
//! over recent history; quota has no reduction -- the freshest reading is the
//! answer and an older one is not evidence about the current window, so a ring
//! would only cost memory. The K tracker keeps an LRU because its key is a
//! client-supplied session; nothing here is client-supplied, so there is no
//! eviction policy to get wrong.
//!
//! # The merge rule, and why it is per-window
//!
//! An observation carries both windows, but an upstream can report one and
//! omit the other, so the windows are merged INDEPENDENTLY. Per window:
//!
//! - A newer valid `Known` replaces an older `Known` or `Unknown`, INCLUDING
//!   when utilization DECREASES. A window resets; a drop is a real reading,
//!   not an anomaly to smooth away.
//! - An incoming `Unknown` PRESERVES a still-fresh stored `Known`. One
//!   response that failed to carry a header is not evidence the budget
//!   changed.
//! - An incoming `Unknown` leaves `Unknown` when the stored reading is absent
//!   or no longer fresh -- so a preserved reading can never outlive its own
//!   window.
//! - An OLDER observation never overwrites a newer one. Responses complete out
//!   of order, and a late arrival describes an earlier moment.
//! - Omitting one window never erases the other.
//! - Expired state is never revived: freshness is judged at merge time and
//!   again at read time, against the window's own reset.
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

use super::freshness::{ObservationStamp, is_fresh};
use super::key::SeatKey;
use super::reduce::{QuotaSnapshot, RejectionReason};
use super::window::QuotaWindow;

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
}

/// Seconds since the Unix epoch, `0` on a pre-epoch clock (which then
/// suppresses rather than emits -- the safe direction for a bounded WARN).
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// One seat's stored reading: each window paired with the stamp of the
/// observation THAT window came from.
///
/// Public only so a carry-over can move readings between two stores; every
/// field stays private, so the merge rule below is the only way to build or
/// change one. A caller reads a seat's state through
/// [`QuotaStore::reading_for`], which applies read-time expiry -- it cannot
/// reach an unexpired-checked window by holding this type.
///
/// The per-window stamp is load-bearing rather than bookkeeping. A window
/// preserved across an unknown-bearing response must keep aging on its ORIGINAL
/// observation, or the monotonic ceiling never bites: a seat under steady
/// traffic whose responses stop carrying the header would re-stamp the same
/// stale reading forever, and a far-future reset would make it immortal --
/// exactly the failure the second freshness bound exists to prevent. Storing one
/// stamp for the whole snapshot cannot express that, because the two windows are
/// merged independently and so can come from different responses.
#[derive(Debug, Clone)]
pub struct StoredReading {
    /// Stamp of the most recent observation merged into this seat, whatever it
    /// carried. Orders arrivals; never ages a window.
    latest: ObservationStamp,
    fast: StoredWindow,
    slow: StoredWindow,
    /// Billing state and the stamp of the observation it came from.
    billing: super::window::Billing,
    billing_observed: ObservationStamp,
}

/// One window and the observation instant it was read at.
#[derive(Debug, Clone)]
struct StoredWindow {
    window: QuotaWindow,
    observed: ObservationStamp,
}

impl StoredWindow {
    /// The window as it reads at `now`: itself while both freshness bounds
    /// hold, `Unknown` once either lapses.
    fn effective(&self, now: &ObservationStamp) -> QuotaWindow {
        match &self.window {
            QuotaWindow::Unknown => QuotaWindow::Unknown,
            QuotaWindow::Known { reset_at, .. } => {
                if is_fresh(reset_at.at(), &self.observed, now, MAX_OBSERVATION_AGE) {
                    self.window.clone()
                } else {
                    QuotaWindow::Unknown
                }
            }
        }
    }

    /// This window merged with an incoming one observed at `observed`.
    ///
    /// A `Known` incoming reading wins outright and brings its own stamp --
    /// including when its utilization DECREASED, which is a window resetting and
    /// not an anomaly to smooth. An `Unknown` incoming reading carries this
    /// window forward UNCHANGED, keeping its original stamp, and only while it
    /// is still effective; once it lapses the seat reads `Unknown` for that
    /// window rather than at its last value.
    fn merged(&self, incoming: &QuotaWindow, observed: &ObservationStamp) -> Self {
        match incoming {
            QuotaWindow::Known { .. } => Self {
                window: incoming.clone(),
                observed: *observed,
            },
            QuotaWindow::Unknown => match self.effective(observed) {
                QuotaWindow::Unknown => Self {
                    window: QuotaWindow::Unknown,
                    observed: *observed,
                },
                _ => self.clone(),
            },
        }
    }
}

impl StoredReading {
    /// A first reading for a seat, every part stamped at its observation.
    const fn first(snapshot: QuotaSnapshot) -> Self {
        Self {
            latest: snapshot.observed,
            fast: StoredWindow {
                window: snapshot.fast,
                observed: snapshot.observed,
            },
            slow: StoredWindow {
                window: snapshot.slow,
                observed: snapshot.observed,
            },
            billing: snapshot.billing,
            billing_observed: snapshot.observed,
        }
    }

    /// This reading merged with `observed`, or `None` when `observed` is OLDER
    /// than what is stored and must be discarded whole.
    ///
    /// Ordering is judged on the MONOTONIC component, the reading that cannot
    /// step backwards; a wall clock adjusted between two responses would
    /// otherwise reorder them. Responses do complete out of order, and a late
    /// arrival describes an earlier moment.
    fn merged(&self, observed: &QuotaSnapshot) -> Option<Self> {
        if observed.observed.monotonic() < self.latest.monotonic() {
            return None;
        }
        Some(Self {
            latest: observed.observed,
            fast: self.fast.merged(&observed.fast, &observed.observed),
            slow: self.slow.merged(&observed.slow, &observed.observed),
            billing: self.merged_billing(observed),
            billing_observed: observed.observed,
        })
    }

    /// The billing state to keep. A known incoming state wins; an `Unknown`
    /// incoming state preserves a known stored one only while the observation it
    /// came from is itself within the age ceiling, and otherwise falls back to
    /// `Unknown`.
    ///
    /// Aged rather than expired, because billing is reported alongside the
    /// windows and has no reset of its own. An unknown incoming state is a
    /// missing header, never evidence a seat became cheaper.
    fn merged_billing(&self, observed: &QuotaSnapshot) -> super::window::Billing {
        if observed.billing != super::window::Billing::Unknown {
            return observed.billing.clone();
        }
        let within_ceiling = observed
            .observed
            .monotonic()
            .checked_duration_since(self.billing_observed.monotonic())
            .is_some_and(|age| age <= MAX_OBSERVATION_AGE);
        if within_ceiling {
            self.billing.clone()
        } else {
            super::window::Billing::Unknown
        }
    }

    /// This reading as a snapshot read at `now`, every window past its bounds
    /// reported `Unknown` rather than at its last value.
    fn as_of(&self, now: &ObservationStamp) -> QuotaSnapshot {
        QuotaSnapshot {
            observed: self.latest,
            fast: self.fast.effective(now),
            slow: self.slow.effective(now),
            billing: self.billing.clone(),
        }
    }
}

/// Per-seat latest-quota readings. Interior-locked, so the post-response feed
/// and the placement read both work through a shared reference.
#[derive(Debug, Default)]
pub struct QuotaStore {
    seats: Mutex<HashMap<SeatKey, StoredReading>>,
    /// The seat keys insertion admits. Empty until
    /// [`QuotaStore::admit_seats`] declares the configured set, and an empty
    /// set admits NOTHING -- a store nobody declared seats for holds no
    /// readings rather than accepting every identity that turns up.
    admitted: Mutex<HashSet<SeatKey>>,
    rejections: RejectionCounters,
}

impl QuotaStore {
    /// Declare the configured seat set this store will hold readings for.
    ///
    /// Called once per Router build from the resolved model table, so the
    /// keyspace bound is the config's rather than the traffic's. Replaces any
    /// previously declared set: a rebuild's set is the current truth.
    pub fn admit_seats(&self, seats: impl IntoIterator<Item = SeatKey>) {
        *self.admitted.lock() = seats.into_iter().collect();
    }

    /// Whether `key` is in the configured seat set.
    pub fn admits(&self, key: &SeatKey) -> bool {
        self.admitted.lock().contains(key)
    }

    /// Merge one observed snapshot into `key`'s stored reading.
    ///
    /// Refused outright -- no entry created, no window merged -- when `key` is
    /// outside the configured seat set, or when the incoming observation is
    /// OLDER than the stored one. Otherwise each window merges independently
    /// per this module's rule.
    ///
    /// Returns whether anything was stored, which the feed's tests read; the
    /// production call site ignores it.
    pub fn observe(&self, key: &SeatKey, observed: QuotaSnapshot) -> bool {
        if !self.admits(key) {
            return false;
        }
        let mut guard = self.seats.lock();
        let merged = match guard.get(key) {
            Some(stored) => match stored.merged(&observed) {
                Some(merged) => merged,
                None => return false,
            },
            None => StoredReading::first(observed),
        };
        guard.insert(key.clone(), merged);
        true
    }

    /// `key`'s reading as of `now`, with every window past its own bounds
    /// reported `Unknown` rather than at its last value.
    ///
    /// `None` for a seat with no reading at all. A seat WITH a reading whose
    /// every window has expired answers `Some` with both windows `Unknown`,
    /// which a caller treats exactly as no evidence -- the distinction exists
    /// so a diagnostic can tell "never observed" from "observed and expired".
    pub fn reading_for(&self, key: &SeatKey, now: &ObservationStamp) -> Option<QuotaSnapshot> {
        self.seats.lock().get(key).map(|stored| stored.as_of(now))
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
            "upstream subscription-quota window refused; seat reads as \
             no-evidence for that window",
        );
    }

    /// Running rejection totals, partitioned by reason.
    pub fn rejection_totals(&self) -> RejectionTotals {
        self.rejections.totals()
    }

    /// Number of seats holding a reading.
    pub fn len(&self) -> usize {
        self.seats.lock().len()
    }

    /// True when no seat holds a reading -- the state a process restart starts
    /// in, since nothing rebuilds this store from the ledger.
    pub fn is_empty(&self) -> bool {
        self.seats.lock().is_empty()
    }

    /// Snapshot every seat and its reading, for carrying live readings across a
    /// hot-reload rebuild.
    ///
    /// Each window travels with its OWN observation stamp, so a carried reading
    /// keeps aging on the instant it was actually read; a reload does not
    /// refresh what the upstream said.
    pub fn export_entries(&self) -> Vec<(SeatKey, StoredReading)> {
        self.seats
            .lock()
            .iter()
            .map(|(key, reading)| (key.clone(), reading.clone()))
            .collect()
    }

    /// Install snapshotted entries, subject to the SAME admission the live feed
    /// is subject to -- so a reload whose config dropped a seat cannot carry
    /// that seat's reading forward, which is what keeps the keyspace bounded by
    /// the CURRENT config across a run of reloads.
    ///
    /// No ordering discipline (unlike the LRU-shaped session stores): this map
    /// never evicts, so there is no eviction frontier a scattered replay could
    /// disturb.
    pub fn import_entries(&self, entries: Vec<(SeatKey, StoredReading)>) {
        let mut guard = self.seats.lock();
        let admitted = self.admitted.lock();
        for (key, reading) in entries {
            if admitted.contains(&key) {
                guard.insert(key, reading);
            }
        }
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
