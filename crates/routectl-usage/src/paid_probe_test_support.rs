//! Cross-crate test fixtures for the paid-probe budget: seams a consumer in another
//! crate cannot build for itself, since it cannot see this crate's test cfg.
//!
//! NEVER CALL THESE AGAINST A LIVE LEDGER. The planting seam writes spend state
//! directly and can therefore LOWER a committed count, and a committed unit is never
//! given back. That is also why this file carries the repo's test-support suffix and a
//! `test-utils` gate: the no-refund scan over production sources must not find it, and
//! no release build may compile it.

use rusqlite::Connection;

use crate::paid_probe::{reservation_key, reserve_one_unit, utc_day_from_epoch_ms};

/// Reserve one paid-probe unit directly, reporting whether it committed.
///
/// The cross-crate seam for a consumer that needs a provider-day whose committed
/// count is REAL: planted bytes would be a second copy of a durable private
/// encoding, and the surface under test is one whose whole contract is agreeing
/// with the writer.
///
/// Returns only a BOOLEAN. The reservation's own outcome vocabulary stays
/// internal, so nothing here hands a caller a value that could be mistaken for an
/// authorization to dial.
#[doc(hidden)]
pub fn reserve_paid_probe_unit_for_tests(
    conn: &mut Connection,
    provider: &str,
    cap: u32,
    now_epoch_ms: i64,
) -> bool {
    matches!(
        reserve_one_unit(conn, provider, cap, now_epoch_ms),
        crate::paid_probe::StoredReservation::Committed { .. }
    )
}

/// Plant a RAW accounting value for one provider-day, bypassing the reservation.
///
/// The one fixture a consumer cannot build any other way: the point is a value the
/// writer would NEVER have produced, so there is nothing the writer could be
/// driven to do that would create it. Goes through this crate's own key encoding
/// so the planted row lands exactly where a reader looks.
///
/// This CAN lower a committed count, which is why it lives in a test-support
/// module rather than beside the reader it supports -- see the module docs.
#[doc(hidden)]
pub fn plant_paid_probe_units_for_tests(
    path: &std::path::Path,
    provider: &str,
    now_epoch_ms: i64,
    raw: &str,
) {
    let key = reservation_key(utc_day_from_epoch_ms(now_epoch_ms), provider);
    let db = crate::db::open(path).expect("migrating open");
    db.conn()
        .execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            rusqlite::params![&key, raw],
        )
        .expect("plant raw accounting value");
}
