//! The placement-safe, read-only view of the account meter.
//!
//! Placement and `/status` both read the meter through this view, which
//! applies the stale rule: a window is stale once its age exceeds
//! [`STALE_AFTER`] on EITHER
//! clock, once a running window's reset has passed, or when its stamp cannot
//! be ordered against the read instant. A stale window keeps its last values
//! for display but gives no numeric room ([`AccountWindow::remaining`]).
//!
//! The types here are plain owned values, detached from the meter's lock.
//! They derive no `Serialize`: a wire shape is mapped from them by its owner,
//! so these field names never become a wire contract.

use std::time::{Duration, SystemTime};

use super::freshness::{ObservationStamp, age, has_passed};
use super::meter::{AccountMeter, LastAccepted, ReportState, StoredWindow};
use super::reading::{
    AccountObservation, LengthClass, ObservationReport, ReadingSource, Started, WindowReading,
};
use super::store::RejectionTotals;
use super::window::Billing;

/// How old a reading may be and still count as fresh.
///
/// The poll is due five minutes after the last fresh reading, the seat driver
/// ticks once a minute and each due time carries up to a minute of jitter, so
/// a healthy idle account is re-read within seven minutes. A shorter bound
/// would mark healthy accounts stale for part of every cycle.
pub const STALE_AFTER: Duration = Duration::from_mins(7);

/// Whether a reading stamped `observed` is no older than [`STALE_AFTER`] at
/// `now` on both clocks. False for an unorderable pair.
pub fn within_stale_after(observed: &ObservationStamp, now: &ObservationStamp) -> bool {
    age(observed, now).is_some_and(|age| age <= STALE_AFTER)
}

/// What one window reports, detached from when it was stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadingWindow {
    /// The reported window length, in seconds.
    pub length_secs: u64,
    /// The class the length falls in.
    pub class: LengthClass,
    /// The vendor scope tag, or `None` for an account-wide window.
    pub scope: Option<String>,
    /// The used fraction, in `[0.0, 1.0]`.
    pub used: f64,
    /// The reset instant. For an unstarted window it is provisional: where the
    /// window would end if it began at the observation.
    pub reset: Option<SystemTime>,
    /// Whether the window is running.
    pub started: Started,
}

impl ReadingWindow {
    fn of(reading: &WindowReading) -> Self {
        let key = reading.key();
        Self {
            length_secs: key.length_secs(),
            class: key.class(),
            scope: key.scope().map(|scope| scope.as_str().to_owned()),
            used: reading.used().fraction(),
            reset: reading.reset().map(super::freshness::ValidatedReset::at),
            started: reading.started(),
        }
    }
}

/// One stored window as read at a given instant.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountWindow {
    /// The window's reported values.
    pub window: ReadingWindow,
    /// Where the reading came from.
    pub source: ReadingSource,
    /// When the reading was observed, on the wall clock.
    pub observed_at: SystemTime,
    /// How old the reading is (the larger of its two clock ages), or `None`
    /// when its stamp cannot be ordered against the read instant.
    pub age: Option<Duration>,
    /// Whether the reading is stale (see the module rule).
    pub stale: bool,
}

impl AccountWindow {
    fn of(stored: &StoredWindow, now: &ObservationStamp) -> Self {
        let observed = stored.observed();
        let reading = stored.reading();
        let reset_passed = reading.started() != Started::No
            && reading
                .reset()
                .is_some_and(|reset| has_passed(reset.at(), now));
        Self {
            window: ReadingWindow::of(reading),
            source: stored.source(),
            observed_at: observed.wall(),
            age: age(observed, now),
            stale: reset_passed || !within_stale_after(observed, now),
        }
    }

    /// The unused fraction of the window, or `None` when the reading is
    /// stale and so gives no room.
    pub fn remaining(&self) -> Option<f64> {
        (!self.stale).then_some(1.0 - self.window.used)
    }
}

/// When an account's latest accepted reading was observed, and from where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastReading {
    /// When it was observed, on the wall clock.
    pub observed_at: SystemTime,
    /// Where it came from.
    pub source: ReadingSource,
    /// Whether it is no older than [`STALE_AFTER`] at the read instant.
    pub fresh: bool,
}

impl LastReading {
    fn of(last: LastAccepted, now: &ObservationStamp) -> Self {
        Self {
            observed_at: last.observed.wall(),
            source: last.source,
            fresh: within_stale_after(&last.observed, now),
        }
    }
}

/// One configured account's readings at a given instant.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountReading {
    /// The account key (`provider#label`, or the bare provider for a default
    /// login).
    pub account: String,
    /// What the account has reported.
    pub report: ReportState,
    /// Every stored window, ordered by length then scope.
    pub windows: Vec<AccountWindow>,
    /// The latest accepted reading, if any.
    pub last_accepted: Option<LastReading>,
    /// The latest billing state the meter keeps.
    pub billing: Billing,
    /// Whether a known billing state was observed no longer than
    /// [`STALE_AFTER`] before the read instant.
    pub billing_fresh: bool,
}

impl AccountReading {
    /// `account`'s readings at `now`; an account with no meter yet has no
    /// reading.
    pub(crate) fn of(account: &str, meter: Option<&AccountMeter>, now: &ObservationStamp) -> Self {
        let Some(meter) = meter else {
            return Self {
                account: account.to_owned(),
                report: ReportState::NoReading,
                windows: Vec::new(),
                last_accepted: None,
                billing: Billing::Unknown,
                billing_fresh: false,
            };
        };
        let mut windows: Vec<AccountWindow> = meter
            .windows()
            .map(|stored| AccountWindow::of(stored, now))
            .collect();
        windows.sort_by(|a, b| {
            (a.window.length_secs, &a.window.scope).cmp(&(b.window.length_secs, &b.window.scope))
        });
        let (billing, billing_observed) = meter.billing();
        Self {
            account: account.to_owned(),
            report: meter.report(),
            windows,
            last_accepted: meter.last_accepted().map(|last| LastReading::of(last, now)),
            billing: billing.clone(),
            billing_fresh: *billing != Billing::Unknown
                && within_stale_after(billing_observed, now),
        }
    }
}

/// Every configured account's readings, plus the meter's refusal totals.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountReadings {
    /// One entry per configured account, in account order.
    pub accounts: Vec<AccountReading>,
    /// Running refusal totals, by reason.
    pub rejections: RejectionTotals,
    /// Running count of writes refused because their account is not
    /// configured.
    pub refused_by_admission: u64,
    /// Running count of stored windows replaced by a confirmed run of lower
    /// readings.
    pub corrections_confirmed: u64,
}

/// An observation the meter accepted, as reported.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedReading {
    /// When the observation was read, on the wall clock.
    pub observed_at: SystemTime,
    /// Every window it reported. Empty only for a complete report that the
    /// account has no windows.
    pub windows: Vec<ReadingWindow>,
}

impl AcceptedReading {
    pub(crate) fn of(observation: &AccountObservation) -> Self {
        let windows = match observation.report() {
            ObservationReport::Partial(set) | ObservationReport::Complete(set) => {
                set.as_slice().iter().map(ReadingWindow::of).collect()
            }
            ObservationReport::NoWindows => Vec::new(),
        };
        Self {
            observed_at: observation.observed().wall(),
            windows,
        }
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod view_tests;
