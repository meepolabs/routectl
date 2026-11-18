//! One account's meter state and the rule that merges an observation into it.
//!
//! An account holds one window per reported [`WindowKey`], each paired with the
//! observation it came from and that observation's source, plus the account's
//! last accepted reading and its billing state. Every change goes through
//! [`AccountMeter::merged`], which returns a new state or refuses the whole
//! observation; a refusal leaves the stored state exactly as it was.
//!
//! # The merge rule
//!
//! - Newest wins PER KEY, on the monotonic clock: a window older than the one
//!   stored for its key is discarded, whatever its source, because responses
//!   and polls complete out of order and a late arrival describes an earlier
//!   moment. Ordering per key rather than per account keeps one window's newer
//!   reading when a different window was updated in between.
//! - A partial observation never erases: a key it omits keeps its own reading
//!   and keeps aging on its own stamp, so read-time expiry still bites.
//! - A complete observation (windows or `NoWindows`) drops every stored window
//!   it omits that is not newer than itself, and sets a floor: a partial window
//!   observed at or before that floor is discarded, so a late partial cannot
//!   bring back a window the complete report said no longer applies. Stamps
//!   are not unique, so the tie goes to the complete report: an equal-stamp
//!   window it omits is dropped and an equal-stamp partial is barred, while
//!   its own windows are taken. A later window supersedes `NoWindows`.
//! - A response header that names a window unknown speaks for it when the
//!   stored window is no longer in effect (or none is stored): it leaves an
//!   ordering mark at its stamp, and a reading observed before the mark is
//!   discarded, so a delayed older reading cannot revive a lapsed window.
//!   Over a window still in effect it is no evidence and leaves no mark.
//! - Contradiction: a newer reading for a running window whose reset matches
//!   the stored one within [`RESET_TOLERANCE`] but whose used fraction is LOWER
//!   describes the same window going backwards. Within
//!   [`CONTRADICTION_SLACK`] that is rounding (header fractions floor, the poll
//!   endpoint rounds) and the reading is accepted at the higher of the two;
//!   further than that it is refused as `Contradictory` and nothing advances.
//!   An unstarted window's reset is provisional (it slides with each poll), so
//!   it never takes part in this comparison.
//!
//! # Why the window map is bounded
//!
//! Response headers produce at most the curated keys, and a poll is complete,
//! so each poll replaces the window set it reported last time; a key no source
//! still reports does not accumulate.

use std::collections::HashMap;
use std::time::Duration;

use super::curation::{ANTHROPIC_PROVIDER_KIND, RESET_TOLERANCE, row_for};
use super::freshness::{ObservationStamp, is_fresh, reset_after};
use super::reading::{
    AccountObservation, ObservationReport, ReadingSource, Started, WindowKey, WindowReading,
    WindowSet,
};
use super::reduce::{QuotaSnapshot, RejectionReason};
use super::store::MAX_OBSERVATION_AGE;
use super::view::{STALE_AFTER, within_stale_after};
use super::window::{Billing, QuotaWindow, WindowRole};

/// How far, as a fraction of the window, a same-reset reading may fall below
/// the stored one and still be read as rounding rather than a contradiction.
pub const CONTRADICTION_SLACK: f64 = 0.02;

/// Absorbs the binary representation of two decimal fractions, so a drop of
/// exactly [`CONTRADICTION_SLACK`] (`0.47` to `0.45`) is not refused for the
/// last bit of its difference.
const SLACK_EPSILON: f64 = 1e-9;

/// What an account has reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    /// No window and no complete report has been accepted.
    NoReading,
    /// At least one window is held.
    Windows,
    /// The latest complete report said the account has no windows.
    NoWindows,
}

/// When the account's latest accepted reading was observed, and from where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastAccepted {
    /// The accepted observation's stamp.
    pub observed: ObservationStamp,
    /// Where it came from.
    pub source: ReadingSource,
}

/// One stored window: the reading, the stamp it ages from, and its source.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredWindow {
    reading: WindowReading,
    observed: ObservationStamp,
    source: ReadingSource,
}

impl StoredWindow {
    /// The validated reading.
    pub const fn reading(&self) -> &WindowReading {
        &self.reading
    }

    /// The stamp of the observation this window came from.
    pub const fn observed(&self) -> &ObservationStamp {
        &self.observed
    }

    /// Where this window came from.
    pub const fn source(&self) -> ReadingSource {
        self.source
    }

    /// Whether this window is running and inside both freshness bounds at
    /// `now`. An unstarted window is never effective: its reset is where it
    /// would end if it began, not an expiry.
    fn is_effective(&self, now: &ObservationStamp) -> bool {
        self.reading.started() != Started::No
            && self
                .reading
                .reset()
                .is_some_and(|reset| is_fresh(reset.at(), &self.observed, now, MAX_OBSERVATION_AGE))
    }

    /// This window as today's placement value at `now`, in `role`.
    ///
    /// A running window holds until its reset. An unstarted FAST window says
    /// the account has used nothing of it, but only as of its observation: it
    /// reads as known zero while no older than [`STALE_AFTER`], with that
    /// bound as its reset, and as unknown after.
    fn projected(&self, role: &WindowRole, now: &ObservationStamp) -> QuotaWindow {
        if self.reading.started() == Started::No {
            return match role {
                WindowRole::Fast => self.projected_unstarted(now),
                WindowRole::Slow => QuotaWindow::Unknown,
            };
        }
        match self.reading.reset() {
            Some(reset_at) if self.is_effective(now) => QuotaWindow::Known {
                utilization: self.reading.used().clone(),
                reset_at,
            },
            _ => QuotaWindow::Unknown,
        }
    }

    fn projected_unstarted(&self, now: &ObservationStamp) -> QuotaWindow {
        if !within_stale_after(&self.observed, now) {
            return QuotaWindow::Unknown;
        }
        reset_after(&self.observed, STALE_AFTER, self.reading.key().length()).map_or(
            QuotaWindow::Unknown,
            |reset_at| QuotaWindow::Known {
                utilization: self.reading.used().clone(),
                reset_at,
            },
        )
    }
}

/// One account's meter state. Every field is private, so [`Self::merged`] is
/// the only way to change one.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountMeter {
    /// Stamp of the most recent observation merged, whatever it carried.
    /// Orders nothing; reported as the projection's observation instant.
    latest: ObservationStamp,
    windows: HashMap<WindowKey, StoredWindow>,
    /// Stamp of the newest accepted complete report; partial windows observed
    /// at or before it are not taken.
    complete_floor: Option<ObservationStamp>,
    /// Per key, the stamp of the newest unknown header that spoke for a window
    /// not in effect; readings observed before it are not taken. Holds at most
    /// the curated header keys.
    unknown_marks: HashMap<WindowKey, ObservationStamp>,
    last_accepted: Option<LastAccepted>,
    billing: Billing,
    billing_observed: ObservationStamp,
}

impl AccountMeter {
    /// An account first seen at `observed`, holding nothing yet.
    pub(super) fn new(observed: ObservationStamp) -> Self {
        Self {
            latest: observed,
            windows: HashMap::new(),
            complete_floor: None,
            unknown_marks: HashMap::new(),
            last_accepted: None,
            billing: Billing::Unknown,
            billing_observed: observed,
        }
    }

    /// What the account has reported.
    pub fn report(&self) -> ReportState {
        if !self.windows.is_empty() {
            ReportState::Windows
        } else if self.complete_floor.is_some() {
            ReportState::NoWindows
        } else {
            ReportState::NoReading
        }
    }

    /// The latest accepted reading's time and source, if any.
    pub const fn last_accepted(&self) -> Option<LastAccepted> {
        self.last_accepted
    }

    /// Every stored window, in no particular order.
    pub fn windows(&self) -> impl Iterator<Item = &StoredWindow> {
        self.windows.values()
    }

    /// The stored window for `key`.
    #[cfg(test)]
    pub fn window(&self, key: &WindowKey) -> Option<&StoredWindow> {
        self.windows.get(key)
    }

    /// This state merged with `observation`, or the whole observation refused
    /// as `Contradictory` with nothing advanced.
    pub(super) fn merged(&self, observation: &AccountObservation) -> Result<Self, RejectionReason> {
        let observed = *observation.observed();
        let source = observation.source();
        let mut next = self.clone();
        next.latest = newer(self.latest, observed);
        let (incoming, is_complete) = match observation.report() {
            ObservationReport::Partial(set) => (set.as_slice(), false),
            ObservationReport::Complete(set) => {
                next.apply_complete(&observed, set.as_slice());
                (set.as_slice(), true)
            }
            ObservationReport::NoWindows => {
                next.apply_complete(&observed, &[]);
                (&[][..], true)
            }
        };
        for reading in incoming {
            if let Some(stored) = self.merged_window(reading, &observed, source, is_complete)? {
                next.unknown_marks.remove(reading.key());
                next.windows.insert(reading.key().clone(), stored);
            }
        }
        if self
            .last_accepted
            .is_none_or(|last| last.observed.monotonic() <= observed.monotonic())
        {
            next.last_accepted = Some(LastAccepted { observed, source });
        }
        Ok(next)
    }

    /// Drop every window a complete report at `observed` omits and is not
    /// older than, and raise the floor to `observed`.
    fn apply_complete(&mut self, observed: &ObservationStamp, reported: &[WindowReading]) {
        if self
            .complete_floor
            .is_some_and(|floor| floor.monotonic() > observed.monotonic())
        {
            return;
        }
        self.windows.retain(|key, stored| {
            reported.iter().any(|w| w.key() == key)
                || stored.observed.monotonic() > observed.monotonic()
        });
        self.complete_floor = Some(*observed);
    }

    /// The window to store for `reading`, `None` to keep what is stored, or
    /// `Contradictory`. `is_complete` says `reading` belongs to a complete
    /// report, which takes an equal-stamp tie against the floor.
    fn merged_window(
        &self,
        reading: &WindowReading,
        observed: &ObservationStamp,
        source: ReadingSource,
        is_complete: bool,
    ) -> Result<Option<StoredWindow>, RejectionReason> {
        let below_floor = self.complete_floor.is_some_and(|floor| {
            observed.monotonic() < floor.monotonic()
                || (!is_complete && observed.monotonic() == floor.monotonic())
        });
        let below_mark = self
            .unknown_marks
            .get(reading.key())
            .is_some_and(|mark| observed.monotonic() < mark.monotonic());
        let stored = self.windows.get(reading.key());
        if below_floor
            || below_mark
            || stored.is_some_and(|s| observed.monotonic() < s.observed.monotonic())
        {
            return Ok(None);
        }
        let reading = match stored {
            Some(stored) => reconciled(stored, reading, observed)?,
            None => reading.clone(),
        };
        Ok(Some(StoredWindow {
            reading,
            observed: *observed,
            source,
        }))
    }

    /// This state merged with one response-header snapshot.
    ///
    /// The snapshot's known FAST and SLOW windows become a partial
    /// observation keyed by their curated lengths, account-wide, with an
    /// unknown started state (headers do not say); an unknown window is
    /// omitted, so it never erases, and marks its key when the stored window
    /// is no longer in effect (see the module's merge rule). A snapshot with
    /// no known window still records its arrival and its billing state.
    pub(super) fn merged_header(&self, snapshot: &QuotaSnapshot) -> Result<Self, RejectionReason> {
        let mut next = match header_observation(snapshot)? {
            Some(observation) => self.merged(&observation)?,
            None => {
                let mut next = self.clone();
                next.latest = newer(self.latest, snapshot.observed);
                next
            }
        };
        for key in self.unknown_header_keys(snapshot) {
            let mark = next
                .unknown_marks
                .get(&key)
                .map_or(snapshot.observed, |mark| newer(*mark, snapshot.observed));
            next.unknown_marks.insert(key, mark);
        }
        next.billing = self.merged_billing(snapshot);
        if snapshot.billing != Billing::Unknown
            && snapshot.observed.monotonic() >= self.billing_observed.monotonic()
        {
            next.billing_observed = snapshot.observed;
        }
        Ok(next)
    }

    /// The curated keys `snapshot` names unknown whose stored window, if any,
    /// is older than the snapshot and no longer in effect at it.
    fn unknown_header_keys(&self, snapshot: &QuotaSnapshot) -> Vec<WindowKey> {
        let observed = &snapshot.observed;
        [
            (WindowRole::Fast, &snapshot.fast),
            (WindowRole::Slow, &snapshot.slow),
        ]
        .into_iter()
        .filter(|(_, window)| matches!(window, QuotaWindow::Unknown))
        .filter_map(|(role, _)| role_key(&role))
        .filter(|key| {
            self.windows.get(key).is_none_or(|stored| {
                stored.observed.monotonic() <= observed.monotonic()
                    && !stored.is_effective(observed)
            })
        })
        .collect()
    }

    /// The billing state to keep. A known incoming state wins unless it is
    /// older than the stored one; an unknown incoming state (a missing claim,
    /// never evidence a seat became cheaper) keeps a known stored one only
    /// while it is inside the age ceiling.
    fn merged_billing(&self, snapshot: &QuotaSnapshot) -> Billing {
        if snapshot.billing != Billing::Unknown {
            if snapshot.observed.monotonic() < self.billing_observed.monotonic() {
                return self.billing.clone();
            }
            return snapshot.billing.clone();
        }
        if within_ceiling(&self.billing_observed, &snapshot.observed) {
            self.billing.clone()
        } else {
            Billing::Unknown
        }
    }

    /// Today's placement value at `now`: FAST is the account-wide window of
    /// the curated five-hour length, SLOW the account-wide window of the
    /// curated long length, each `Unknown` once past its reset or the age
    /// ceiling. An unstarted window is `Unknown`, except a FAST one no older
    /// than [`STALE_AFTER`], which is known zero.
    pub(super) fn projection(&self, now: &ObservationStamp) -> QuotaSnapshot {
        QuotaSnapshot {
            observed: self.latest,
            fast: self.projected_role(&WindowRole::Fast, now),
            slow: self.projected_role(&WindowRole::Slow, now),
            // Billing ages on the same ceiling as a window: it has no reset of
            // its own, and an overage claim must not outlive the capacity
            // signal it arrived beside.
            billing: if within_ceiling(&self.billing_observed, now) {
                self.billing.clone()
            } else {
                Billing::Unknown
            },
        }
    }

    fn projected_role(&self, role: &WindowRole, now: &ObservationStamp) -> QuotaWindow {
        role_key(role)
            .and_then(|key| self.windows.get(&key))
            .map_or(QuotaWindow::Unknown, |stored| stored.projected(role, now))
    }
}

/// `incoming` reconciled against the window stored for its key: itself, the
/// same reading lifted to the stored used fraction when it is lower by no
/// more than the slack, or `Contradictory` when it is lower by more.
fn reconciled(
    stored: &StoredWindow,
    incoming: &WindowReading,
    observed: &ObservationStamp,
) -> Result<WindowReading, RejectionReason> {
    let (Some(stored_reset), Some(incoming_reset)) = (stored.reading.reset(), incoming.reset())
    else {
        return Ok(incoming.clone());
    };
    let same_reset = abs_diff(stored_reset.at(), incoming_reset.at()) <= RESET_TOLERANCE;
    let stored_used = stored.reading.used().fraction();
    let drop = stored_used - incoming.used().fraction();
    if incoming.started() == Started::No
        || !stored.is_effective(observed)
        || !same_reset
        || drop <= 0.0
    {
        return Ok(incoming.clone());
    }
    if drop > CONTRADICTION_SLACK + SLACK_EPSILON {
        return Err(RejectionReason::Contradictory);
    }
    WindowReading::new(
        incoming.key().clone(),
        stored_used,
        Some(incoming_reset.at()),
        incoming.started(),
        observed,
    )
}

/// The account-wide key of the curated window in `role`. Every curated row in
/// one role shares one length (pinned by a test), so the header snapshot,
/// which names a role but no provider, maps onto one key.
pub(super) fn role_key(role: &WindowRole) -> Option<WindowKey> {
    let row = row_for(ANTHROPIC_PROVIDER_KIND, role)?;
    WindowKey::new(row.duration.as_secs(), None).ok()
}

/// The partial observation a header snapshot carries, or `None` when it
/// carries no known window.
fn header_observation(
    snapshot: &QuotaSnapshot,
) -> Result<Option<AccountObservation>, RejectionReason> {
    let mut windows = Vec::new();
    for (role, window) in [
        (WindowRole::Fast, &snapshot.fast),
        (WindowRole::Slow, &snapshot.slow),
    ] {
        let QuotaWindow::Known {
            utilization,
            reset_at,
        } = window
        else {
            continue;
        };
        let key = role_key(&role).ok_or(RejectionReason::UnknownLength)?;
        windows.push(WindowReading::new(
            key,
            utilization.fraction(),
            Some(reset_at.at()),
            Started::Unknown,
            &snapshot.observed,
        )?);
    }
    if windows.is_empty() {
        return Ok(None);
    }
    AccountObservation::new(
        ReadingSource::Response,
        snapshot.observed,
        ObservationReport::Partial(WindowSet::new(windows)?),
    )
    .map(Some)
}

/// The later of two stamps on the monotonic clock.
fn newer(current: ObservationStamp, incoming: ObservationStamp) -> ObservationStamp {
    if incoming.monotonic() < current.monotonic() {
        current
    } else {
        incoming
    }
}

/// Whether `now` is within the monotonic age ceiling of `observed`. False when
/// the two cannot be ordered, so an unorderable pair ages out.
fn within_ceiling(observed: &ObservationStamp, now: &ObservationStamp) -> bool {
    now.monotonic()
        .checked_duration_since(observed.monotonic())
        .is_some_and(|age| age <= MAX_OBSERVATION_AGE)
}

fn abs_diff(a: std::time::SystemTime, b: std::time::SystemTime) -> Duration {
    a.duration_since(b)
        .or_else(|_| b.duration_since(a))
        .unwrap_or(Duration::MAX)
}

#[cfg(test)]
#[path = "meter_tests.rs"]
mod meter_tests;
