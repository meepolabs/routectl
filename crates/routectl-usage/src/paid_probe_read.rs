//! The read side of the paid-probe budget: one provider-day's committed unit count,
//! for the accounting-health surface. A pure read that authorizes nothing -- it takes
//! no cap, refuses nothing, and cannot commit.
//!
//! Two durable invariants:
//!
//! * Malformed stored state is its own answer, never a zero. The reservation REFUSES
//!   on an unparseable count, so reporting zero would show a clean budget for a
//!   provider whose paid calls are all failing closed.
//! * The UTC day is sampled by the caller, so a read cannot land on the other side of
//!   a rollover from the cap gate it is being reconciled against.

use rusqlite::{Connection, OptionalExtension};

use crate::paid_probe::{parse_units, reservation_key, utc_day_from_epoch_ms};

/// What one provider-day's committed count reads as.
///
/// Three answers rather than an `Option<u32>`, because "no units spent" and "the
/// stored accounting is unreadable" are opposite operational facts and an
/// `Option` would force the caller to pick one of them to mean `None`.
/// `#[non_exhaustive]`: a public read-outcome vocabulary that may GROW -- a future
/// accounting state (a day whose row is locked, a schema the reader does not know)
/// has to be addable without breaking every foreign match. The attribute forces a
/// `..` arm at foreign call sites, which is what makes a new variant land as a
/// deliberate decision there rather than as a silent fall-through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PaidProbeUsage {
    /// The provider-day's committed count, as stored. Zero for a day with no
    /// row -- nothing has been spent, which is a real reading rather than an
    /// absence.
    Committed(u32),
    /// A row exists and its value is not a canonical count, so the day's true
    /// spend is unknown. Reported distinctly because the reservation REFUSES on
    /// this state: a health surface showing zero here would show a clean budget
    /// for a provider whose paid probes are all failing closed.
    Malformed,
    /// The read itself failed. Distinct from [`Self::Malformed`]: the stored
    /// state may be perfectly fine and the QUERY did not land, so an operator
    /// looking for corrupt accounting would be looking in the wrong place.
    Unreadable,
}

/// Read the units committed for `provider` on the UTC day containing
/// `now_epoch_ms`.
///
/// Goes through the reservation's OWN key encoding rather than restating it: the
/// key is a durable private format, and a second copy here would drift and then
/// read a day the writer never wrote to -- reporting a clean budget for a
/// provider that had spent its cap. The same reason the value goes through the
/// reservation's parser: a reader accepting a form the writer would not have
/// written would disagree with the cap gate about what is stored.
pub fn committed_units(conn: &Connection, provider: &str, now_epoch_ms: i64) -> PaidProbeUsage {
    let key = reservation_key(utc_day_from_epoch_ms(now_epoch_ms), provider);
    let stored: Option<String> = match conn
        .query_row(SELECT_UNITS_SQL, [&key], |row| row.get(0))
        .optional()
    {
        Ok(stored) => stored,
        Err(_) => return PaidProbeUsage::Unreadable,
    };
    match stored.as_deref().map(parse_units) {
        None => PaidProbeUsage::Committed(0),
        Some(Ok(units)) => PaidProbeUsage::Committed(units),
        Some(Err(_)) => PaidProbeUsage::Malformed,
    }
}

/// The read, identical to the reservation's own SELECT.
///
/// Spelled here rather than shared, deliberately: the reservation's constant is
/// part of a transaction whose behavior depends on the surrounding immediate
/// lock, and importing it would couple a pure read to that transaction's shape.
/// What must not drift is the KEY and the VALUE FORMAT, and both of those ARE
/// shared -- the statement is one column of one table either way.
const SELECT_UNITS_SQL: &str = "SELECT value FROM meta WHERE key = ?1";

#[cfg(test)]
#[path = "paid_probe_read_tests.rs"]
mod tests;
