//! The running windows a complete report dropped while they were in effect.
//!
//! Each one keeps binding placement and stays the contradiction baseline for
//! its key, as a stored window would, until a reading for the key is
//! accepted or the window stops being in effect. The account-wide five-hour
//! window lives only that long; any other is also let go once
//! [`OMISSION_CONFIRMATIONS`] consecutive complete reports have omitted it,
//! so a window the vendor really removed stops binding within a few polls.

use std::collections::HashMap;

use super::freshness::ObservationStamp;
use super::meter::StoredWindow;
use super::reading::{WindowKey, WindowReading};

/// How many consecutive complete reports must omit a dropped window, other
/// than the five-hour, before it stops binding placement.
pub const OMISSION_CONFIRMATIONS: u32 = 3;

/// A dropped window other than the five-hour, with its consecutive
/// omissions and the stamp of the last one counted.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct OmittedWindow {
    pub(super) stored: StoredWindow,
    pub(super) omissions: u32,
    pub(super) last_omitted: ObservationStamp,
}

/// Every kept window of one account.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct KeptWindows {
    /// The account-wide five-hour window, kept until its reset.
    pub(super) dropped_fast: Option<StoredWindow>,
    /// Per key, every other kept window, counted against omissions too.
    pub(super) omitted: HashMap<WindowKey, OmittedWindow>,
}

impl KeptWindows {
    /// Every kept window, in no particular order.
    pub(super) fn iter(&self) -> impl Iterator<Item = &StoredWindow> {
        self.dropped_fast
            .iter()
            .chain(self.omitted.values().map(|kept| &kept.stored))
    }

    /// The kept window for `key` when it is in effect at `observed`.
    pub(super) fn baseline(
        &self,
        key: &WindowKey,
        observed: &ObservationStamp,
    ) -> Option<&StoredWindow> {
        self.dropped_fast
            .as_ref()
            .filter(|kept| kept.reading().key() == key)
            .or_else(|| self.omitted.get(key).map(|kept| &kept.stored))
            .filter(|kept| kept.is_effective(observed))
    }

    /// Let go of every kept window no longer in effect at `now`.
    pub(super) fn lapse(&mut self, now: &ObservationStamp) {
        self.dropped_fast.take_if(|kept| has_lapsed(kept, now));
        self.omitted
            .retain(|_, kept| !has_lapsed(&kept.stored, now));
    }

    /// Let go of the kept window for `key`, a reading for it being accepted.
    pub(super) fn release(&mut self, key: &WindowKey) {
        self.dropped_fast
            .take_if(|kept| kept.reading().key() == key);
        self.omitted.remove(key);
    }

    /// Keep `stored`, dropped by a complete report at `observed`; `is_fast`
    /// says it is the account-wide five-hour window.
    pub(super) fn keep(
        &mut self,
        stored: StoredWindow,
        observed: &ObservationStamp,
        is_fast: bool,
    ) {
        if is_fast {
            self.dropped_fast = Some(stored);
            return;
        }
        self.omitted.insert(
            stored.reading().key().clone(),
            OmittedWindow {
                stored,
                omissions: 1,
                last_omitted: *observed,
            },
        );
    }

    /// Count a complete report at `observed` against each kept window other
    /// than the five-hour: one listing its key restarts the count, one
    /// omitting it and newer than the last counted adds one, and the window
    /// is let go at [`OMISSION_CONFIRMATIONS`]. A repeat of a counted stamp
    /// counts nothing, so one report delivered twice is one omission.
    pub(super) fn count_omissions(
        &mut self,
        observed: &ObservationStamp,
        reported: &[WindowReading],
    ) {
        self.omitted.retain(|key, kept| {
            if reported.iter().any(|w| w.key() == key) {
                kept.omissions = 0;
                return true;
            }
            if observed.monotonic() <= kept.last_omitted.monotonic() {
                return true;
            }
            kept.omissions = kept.omissions.saturating_add(1);
            kept.last_omitted = *observed;
            kept.omissions < OMISSION_CONFIRMATIONS
        });
    }
}

/// Whether `kept`, observed no later than `now`, is no longer in effect at
/// it. A kept window stamped after `now` is judged by a later merge.
fn has_lapsed(kept: &StoredWindow, now: &ObservationStamp) -> bool {
    kept.observed().monotonic() <= now.monotonic() && !kept.is_effective(now)
}
