//! Value-level time correctness for a quota observation.
//!
//! A quota reading is only meaningful together with WHEN it was taken, and
//! this module owns that pairing at the value level: stamping the instant a
//! response's metadata was read, refusing a reset instant that cannot belong
//! to the window it claims, and answering whether a stamped reading is still
//! effective at read time. It holds no state -- the per-seat store and its
//! merge rule are a separate concern -- so every function here is pure and
//! every rejection is a value the caller can count.
//!
//! # Why an implausible reset is refused rather than clamped
//!
//! The `quota_reset` family is epoch SECONDS. A source that hands over
//! milliseconds is off by a factor of a thousand, which places the reset tens
//! of thousands of years out -- and a reset far in the future reads as
//! PERMANENTLY VALID to any expiry check. The failure is silent and
//! self-reinforcing: a seat with a low utilization and an immortal reset
//! attracts every new session forever. So a reset is accepted only inside the
//! window it claims to belong to, which rejects that whole class at the door
//! instead of storing it and hoping a later check notices.
//!
//! # Why two independent bounds
//!
//! A window expires at its own `reset_at` in wall-clock terms, and a reading
//! also ages out on a monotonic clock. The wall-clock bound is the real
//! semantic one; the monotonic bound is the backstop, because wall time can
//! step backwards and a reading whose seat stopped receiving traffic must not
//! stay authoritative forever. A reading is effective only while BOTH hold,
//! and any arithmetic that cannot be performed answers not-fresh -- the
//! direction that falls back to no-evidence rather than to false confidence.

use std::time::{Duration, Instant, SystemTime};

/// When an observation was taken, on both clocks.
///
/// Both are captured at the same point because neither is sufficient alone:
/// the wall clock is what a `reset_at` is expressed in and can step
/// backwards, while the monotonic clock cannot be compared against an
/// upstream timestamp but does age reliably.
#[derive(Debug, Clone, Copy)]
pub struct ObservationStamp {
    /// Wall clock at observation, comparable against an upstream reset.
    pub wall: SystemTime,
    /// Monotonic clock at observation, for the age ceiling.
    pub monotonic: Instant,
}

impl ObservationStamp {
    /// Stamp the current instant on both clocks.
    ///
    /// Called where the response metadata is read, never later: a stamp taken
    /// downstream of the read would date the reading to when routectl got
    /// around to it rather than to when the upstream reported it.
    pub fn now() -> Self {
        Self {
            wall: SystemTime::now(),
            monotonic: Instant::now(),
        }
    }
}

/// Why a reported reset instant was refused.
///
/// Distinct variants rather than one failure, so a diagnostic can report
/// which class of bad input a seat is producing: an expired reset suggests a
/// slow or replayed response, an implausible one suggests a unit or parsing
/// error, and an overflow suggests a value far outside the representable
/// range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResetRejection {
    /// The reset had already passed at the observation instant, so the
    /// reading described a window that was already over.
    Expired,
    /// The reset fell further beyond the observation instant than the
    /// window's own duration plus tolerance allows, so it cannot belong to
    /// the window it claims.
    Implausible,
    /// The plausibility bound could not be computed for these inputs.
    Overflow,
}

/// Accept a reported reset instant for a window of `window_duration`, or
/// refuse it with the reason.
///
/// Accepted only when the reset is STRICTLY later than the observation
/// instant and no later than the observation plus the window's own duration
/// plus `tolerance`. The tolerance absorbs upstream clock skew and rounding;
/// it is not a way to admit a reset from a longer window.
pub fn accept_reset(
    reset_at: SystemTime,
    observed: &ObservationStamp,
    window_duration: Duration,
    tolerance: Duration,
) -> Result<SystemTime, ResetRejection> {
    if reset_at <= observed.wall {
        return Err(ResetRejection::Expired);
    }
    let Some(span) = window_duration.checked_add(tolerance) else {
        return Err(ResetRejection::Overflow);
    };
    let Some(latest_plausible) = observed.wall.checked_add(span) else {
        return Err(ResetRejection::Overflow);
    };
    if reset_at > latest_plausible {
        return Err(ResetRejection::Implausible);
    }
    Ok(reset_at)
}

/// Whether a reading stamped at `observed` and resetting at `reset_at` is
/// still effective when read at `now`.
///
/// Pure, and false-by-default on anything it cannot establish: a reset
/// already reached on the wall clock, a monotonic age above `max_age`, and a
/// `now` whose monotonic component precedes the stamp's all answer
/// not-fresh. The caller supplies `max_age` because how long a reading stays
/// authoritative is a policy of whatever holds the readings, not a property
/// of one value.
pub fn is_fresh(
    reset_at: SystemTime,
    observed: &ObservationStamp,
    now: &ObservationStamp,
    max_age: Duration,
) -> bool {
    let Some(age) = now.monotonic.checked_duration_since(observed.monotonic) else {
        return false;
    };
    age <= max_age && now.wall < reset_at
}

#[cfg(test)]
#[path = "freshness_tests.rs"]
mod freshness_tests;
