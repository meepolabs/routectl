//! The per-request placement record and the router's placement counters.
//!
//! A placement is a pick the pace rule made and won: a keyed birth whose pin
//! claim won, a move whose repin won, or a keyless request on a pool with a
//! fresh reading. A warm stay, a turn that lost its pin write to a concurrent
//! turn, a dormant pool and a keyless request that fell back to the fill-first
//! walk make none. The record rides every dispatch target of the placing pool
//! into [`DispatchMeta`](super::DispatchMeta), where the usage ledger stores it
//! as `extra.placement`.
//!
//! Content-free by construction: closed tokens plus the seat's credential
//! account key, which comes from operator config.

use std::sync::atomic::{AtomicU64, Ordering};

use routectl_core::EligibilityReason;
use serde_json::{Map, Value};

use crate::quota::placement::Tier;

/// The rule id every record carries, so a reader can tell records of a later
/// rule apart.
pub const PLACEMENT_RULE: &str = "pace_v1";

/// What kind of placement a record describes.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementEvent {
    /// A new keyed conversation was pinned to the seat.
    Birth,
    /// A pinned conversation moved to the seat off a home with a lasting
    /// outage.
    Move,
    /// A request with no session key was led by the seat.
    Keyless,
}

impl PlacementEvent {
    /// The stable token for this event.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Birth => "birth",
            Self::Move => "move",
            Self::Keyless => "keyless",
        }
    }
}

/// One won placement: which seat the pace rule put the request on, from
/// which tier, and why a move left its home.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRecord {
    /// The kind of placement.
    pub event: PlacementEvent,
    /// Token of the chosen seat's tier (`roomy`, `low_room` or `unread`).
    pub arm: &'static str,
    /// The chosen seat's credential account key.
    pub seat: String,
    /// The eligibility reason token that moved the conversation; `None`
    /// unless `event` is [`PlacementEvent::Move`].
    pub trigger: Option<&'static str>,
}

impl PlacementRecord {
    pub(super) fn new(
        event: PlacementEvent,
        arm: Tier,
        seat: &str,
        trigger: Option<EligibilityReason>,
    ) -> Self {
        Self {
            event,
            arm: arm.as_str(),
            seat: seat.to_string(),
            trigger: trigger.map(EligibilityReason::as_str),
        }
    }

    /// The record as the JSON object the usage ledger stores: `rule`,
    /// `event`, `arm`, `seat`, and `trigger` on a move only.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("rule".to_string(), Value::from(PLACEMENT_RULE));
        map.insert("event".to_string(), Value::from(self.event.as_str()));
        map.insert("arm".to_string(), Value::from(self.arm));
        map.insert("seat".to_string(), Value::from(self.seat.as_str()));
        if let Some(trigger) = self.trigger {
            map.insert("trigger".to_string(), Value::from(trigger));
        }
        Value::Object(map)
    }
}

/// Every tier, in the order [`PlacementTotals::births`] lists them.
pub(super) const ARMS: [Tier; 3] = [Tier::Roomy, Tier::LowRoom, Tier::Unread];

/// Running placement counters for one router instance (shared across a hot
/// reload with the rest of the metrics).
#[derive(Debug, Default)]
pub(super) struct PlacementCounters {
    births: [AtomicU64; ARMS.len()],
    moves: [AtomicU64; EligibilityReason::ALL.len()],
    cap_forced_moves: AtomicU64,
}

/// A copy of the placement counters, read once so the fields agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlacementTotals {
    /// Births, keyless placements included, by arm in [`ARMS`] order.
    pub(super) births: [u64; ARMS.len()],
    /// Moves by trigger, in [`EligibilityReason::ALL`] order.
    pub(super) moves: [u64; EligibilityReason::ALL.len()],
    /// Moves off an open breaker whose home stood `low_room` at the move.
    pub(super) cap_forced_moves: u64,
}

impl PlacementTotals {
    pub(super) fn births_of(&self, arm: Tier) -> u64 {
        ARMS.iter()
            .position(|a| *a == arm)
            .map_or(0, |i| self.births[i])
    }

    pub(super) fn moves_of(&self, trigger: EligibilityReason) -> u64 {
        EligibilityReason::ALL
            .iter()
            .position(|r| *r == trigger)
            .map_or(0, |i| self.moves[i])
    }
}

impl PlacementCounters {
    /// Count one won birth or keyless placement on a seat of tier `arm`.
    pub(super) fn incr_birth(&self, arm: Tier) {
        if let Some(i) = ARMS.iter().position(|a| *a == arm) {
            self.births[i].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Count one won move for `trigger`; `cap_forced` when the trigger was an
    /// open breaker and the home stood `low_room` at the move.
    pub(super) fn incr_move(&self, trigger: EligibilityReason, cap_forced: bool) {
        if let Some(i) = EligibilityReason::ALL.iter().position(|r| *r == trigger) {
            self.moves[i].fetch_add(1, Ordering::Relaxed);
        }
        if cap_forced {
            self.cap_forced_moves.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn totals(&self) -> PlacementTotals {
        PlacementTotals {
            births: std::array::from_fn(|i| self.births[i].load(Ordering::Relaxed)),
            moves: std::array::from_fn(|i| self.moves[i].load(Ordering::Relaxed)),
            cap_forced_moves: self.cap_forced_moves.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
#[path = "placement_record_tests.rs"]
mod placement_record_tests;
