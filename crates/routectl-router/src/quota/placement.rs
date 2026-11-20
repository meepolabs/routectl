//! The pace rule: which seat a NEW conversation is born on, from each
//! account's usage readings.
//!
//! Each candidate seat gets a [`Standing`] from the placement-safe view of its
//! account ([`super::view::AccountReading`]): a tier, a score, and the reason
//! that decided them. [`best_bucket`] keeps the candidates sharing the best
//! tier and the best score rounded to two decimals; the caller breaks that tie
//! with its unchanged headroom pick and anti-herd rotation, so a burst of new
//! conversations spreads across seats that are equally good.
//!
//! A seat's score is its slowest pace: for each relevant window, the room left
//! divided by the fraction of the window still to run. A seat that has spent
//! little of a window that resets soon scores high; one that has spent most of
//! a window with days left scores low. The worst relevant window decides, so a
//! nearly exhausted weekly window is never hidden by a fresh five-hour one.
//!
//! An unread seat (no fresh reading) ranks below every read one but still
//! places when nothing better is eligible: the rule orders seats, it never
//! refuses a request. Nothing here reads a pin, so an established conversation
//! never moves because its seat's reading changed.
//!
//! A pinned conversation moves only when [`move_trigger`] says its seat is out
//! for long enough to cost more than the warm prompt cache a move gives up;
//! the move then ranks the other seats by this same rule.

use std::time::Duration;

use routectl_core::EligibilityReason;

use crate::runtime_state::CapacitySnapshot;

use super::freshness::ObservationStamp;
use super::meter::ReportState;
use super::reading::Started;
use super::view::{AccountReading, AccountWindow};
use super::window::Billing;

/// A seat's placement tier. Declared worst first, so the derived order ranks
/// `Roomy` highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// No fresh reading to rank on.
    Unread,
    /// Read, and close to a cap or billing overage.
    LowRoom,
    /// Read, with room under every relevant window.
    Roomy,
}

impl Tier {
    /// The stable snake_case token for this tier, as the placement record
    /// and the counters name it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::LowRoom => "low_room",
            Self::Roomy => "roomy",
        }
    }
}

/// Why a seat stands where it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The account is billing beyond its included budget.
    Overage,
    /// The account reported that it has no usage windows.
    NoCap,
    /// No relevant window is held.
    NoReading,
    /// A relevant window's reading is stale.
    StaleReading,
    /// A relevant window has less than the near-cap room left.
    LowRoom,
    /// Ranked on pace.
    Pace,
    /// Every relevant window is unstarted.
    Unstarted,
}

/// One seat's standing for a birth placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Standing {
    /// The tier.
    pub tier: Tier,
    /// The unrounded score, or `None` for an unread seat.
    pub score: Option<f64>,
    /// What decided the tier and score.
    pub reason: Reason,
}

/// The used fraction past which a window leaves too little room to rank
/// `Roomy`. Room is compared as `1.0 - used` against `1.0 - NEAR_CAP`, the same
/// floating-point expression on both sides, so exactly 10% room is roomy.
const NEAR_CAP: f64 = 0.90;

/// The smallest fraction of a window counted as still to run, so a window at
/// or past its reset scores a large finite pace instead of dividing by zero.
const MIN_REMAINING_FRACTION: f64 = 1e-6;

/// Decimal places the near-tie set is bucketed to.
const TIE_DECIMALS: usize = 2;

/// `reading`'s standing for a conversation on upstream model `model` at
/// `now`. `None` (no account behind the seat) is unread.
pub fn standing(reading: Option<&AccountReading>, model: &str, now: &ObservationStamp) -> Standing {
    let Some(reading) = reading else {
        return unread(Reason::NoReading);
    };
    if reading.billing_fresh && reading.billing == Billing::Overage {
        return Standing {
            tier: Tier::LowRoom,
            score: Some(0.0),
            reason: Reason::Overage,
        };
    }
    if reading.report == ReportState::NoWindows
        && reading.last_accepted.is_some_and(|last| last.fresh)
    {
        return Standing {
            tier: Tier::Roomy,
            score: Some(1.0),
            reason: Reason::NoCap,
        };
    }
    let relevant: Vec<&AccountWindow> = reading
        .windows
        .iter()
        .filter(|window| is_relevant(window, model))
        .collect();
    if relevant.is_empty() {
        return unread(Reason::NoReading);
    }
    if relevant.iter().any(|window| window.stale) {
        return unread(Reason::StaleReading);
    }
    let score = relevant
        .iter()
        .map(|window| pace(window, now))
        .fold(f64::INFINITY, f64::min);
    let least_room = relevant
        .iter()
        .map(|window| 1.0 - window.window.used)
        .fold(f64::INFINITY, f64::min);
    let (tier, reason) = if least_room < 1.0 - NEAR_CAP {
        (Tier::LowRoom, Reason::LowRoom)
    } else if relevant
        .iter()
        .all(|window| window.window.started == Started::No)
    {
        (Tier::Roomy, Reason::Unstarted)
    } else {
        (Tier::Roomy, Reason::Pace)
    };
    Standing {
        tier,
        score: Some(score),
        reason,
    }
}

const fn unread(reason: Reason) -> Standing {
    Standing {
        tier: Tier::Unread,
        score: None,
        reason,
    }
}

/// Whether `window` limits a conversation on `model`: every account-wide
/// window does, a scoped one only through the closed scope table.
fn is_relevant(window: &AccountWindow, model: &str) -> bool {
    window
        .window
        .scope
        .as_deref()
        .is_none_or(|scope| super::model_scope::binds(scope, model))
}

/// Room left per fraction of the window still to run. An unstarted window
/// scores 1.0 whatever its length; a window with no reset scores its room.
fn pace(window: &AccountWindow, now: &ObservationStamp) -> f64 {
    let reading = &window.window;
    if reading.started == Started::No {
        return 1.0;
    }
    let room = 1.0 - reading.used;
    let Some(reset) = reading.reset else {
        return room;
    };
    let left = reset
        .duration_since(now.wall())
        .map_or(0.0, |left| left.as_secs_f64());
    let to_run = left / reading.length_secs as f64;
    room / to_run.max(MIN_REMAINING_FRACTION)
}

/// `score` rounded to [`TIE_DECIMALS`] places as Python's `round(x, 2)`
/// rounds it: on the exact decimal expansion of the binary value, with only
/// an exact tie going to the even digit. Rust's fixed-precision formatting is
/// exact in the same way, so `2.675` (stored just below the tie) rounds to
/// `2.67` and `0.125` (an exact tie) to `0.12`.
fn rounded(score: f64) -> f64 {
    format!("{score:.TIE_DECIMALS$}").parse().unwrap_or(score)
}

/// The rounded score `standing` ranks on within its tier. An unread seat has
/// none and ranks below every scored one.
fn bucket_score(standing: &Standing) -> f64 {
    standing.score.map_or(f64::NEG_INFINITY, rounded)
}

/// The best tier among `candidates` and every candidate sharing it and its
/// rounded score, or `None` when `candidates` is empty. `standings` is
/// index-aligned with the pool; a candidate past its end reads as unread.
pub fn best_bucket(standings: &[Standing], candidates: &[usize]) -> Option<(Tier, Vec<usize>)> {
    let missing = unread(Reason::NoReading);
    let of = |idx: usize| standings.get(idx).copied().unwrap_or(missing);
    let key = |idx: usize| {
        let standing = of(idx);
        (standing.tier, bucket_score(&standing))
    };
    let best = candidates
        .iter()
        .map(|&idx| key(idx))
        .max_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))?;
    let tied = candidates
        .iter()
        .copied()
        .filter(|&idx| key(idx) == best)
        .collect();
    Some((best.0, tied))
}

/// The shortest outage worth moving a warm conversation for: the shortest
/// prompt-cache lifetime, so a seat back within it would still have served
/// the cache the move throws away.
pub const MOVE_FLOOR: Duration = Duration::from_mins(5);

/// Why a pinned conversation leaves its seat, `home`, or `None` when it
/// stays.
///
/// An unusable credential always moves it. A parked account or an open
/// breaker moves it only while at least [`MOVE_FLOOR`] of the outage is
/// known to remain. Everything else stays: an empty local RPM bucket, a
/// half-open breaker ready to re-test, a re-test or probe already in flight,
/// and any outage whose end is unknown or nearer than the floor.
pub fn move_trigger(home: &CapacitySnapshot) -> Option<EligibilityReason> {
    let reason = home.reason()?;
    let lasting = home
        .outage_remaining
        .is_some_and(|remaining| remaining >= MOVE_FLOOR);
    match reason {
        EligibilityReason::CredentialUnusable => Some(reason),
        EligibilityReason::AccountParked | EligibilityReason::BreakerOpen if lasting => {
            Some(reason)
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "placement_tests.rs"]
mod placement_tests;

#[cfg(test)]
#[path = "placement_move_tests.rs"]
mod placement_move_tests;
