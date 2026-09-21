//! Durable key/value codec for paid-probe reservations, and the atomic
//! reservation that stores one.
//!
//! A reservation is one unit of paid-probe budget spent against a
//! configured provider on one UTC day. It is recorded in the existing
//! key/value control storage, so the whole durable contract is two strings:
//! the key that names the (day, provider) bucket and the value that carries
//! the committed count. [`reserve_one_unit`] is the one operation over that
//! pair -- check the day's count against the caller's cap and commit
//! exactly one more unit, atomically.
//!
//! Both halves are pure and durable -- a key written today must be read
//! back by a later build, so the encodings here are fixed, not derived
//! from a formatting convenience. Three properties carry that weight:
//!
//! * The UTC day is a floored division of epoch milliseconds, so the
//!   crate needs no calendar dependency and pre-epoch timestamps still
//!   land on a single day.
//! * The provider name is hex-encoded, so a name carrying the key's own
//!   separator (or any non-ASCII byte) can neither collide with another
//!   provider nor inject key structure.
//! * A value that is not canonical decimal is refused rather than read
//!   as zero: malformed accounting state must block a paid call, and a
//!   silent zero would license one.
//!
//! Nothing outside the tests reads this module yet: the codec and the
//! reservation it feeds are fixed before the writer path that submits one,
//! so the durable encoding and the accounting rule are settled
//! independently of any caller. Hence the crate-local dead-code allowance
//! below, which the first production caller removes. The entry points and
//! the outcome enums are `pub(super)` (visible to the sibling modules that
//! will submit and report a reservation, no wider) so the crate's published
//! surface does not grow for an internal encoding. Everything they are
//! built from stays private to this module.
//!
//! THERE IS NO RELEASE, REFUND, OR DECREMENT, and the absence is the
//! design: a committed unit is spent. Releasing a unit whose paid call may
//! already have reached the upstream is how a crash loop spends a day's cap
//! several times over, so the only operation is the reservation itself.

#![cfg_attr(not(test), allow(dead_code))]
#![expect(
    clippy::redundant_pub_crate,
    reason = "this module is private, so a crate-wide visibility reads as \
              redundant -- but the sibling modules that will store and read a \
              reservation have to name these items, and widening them to `pub` \
              to satisfy the lint would publish an internal encoding"
)]

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

/// Milliseconds in one UTC day. Leap seconds are not represented in
/// epoch-millisecond time, so every day is exactly this long.
const MS_PER_UTC_DAY: i64 = 86_400_000;
/// Fixed prefix of every reservation key, carrying the codec version.
const RESERVATION_KEY_PREFIX: &str = "paid_probe_reservation:v1";

/// Why a stored reservation value could not be read as a unit count.
///
/// A reservation reader that cannot classify its own state must fail
/// closed, so every refusal is explicit and none of them is a zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MalformedUnits {
    /// No digits at all.
    Empty,
    /// A digit run with a redundant leading zero (`00`, `01`).
    LeadingZero,
    /// A byte outside `0-9` (sign, whitespace, decimal point, letter).
    NonDigit,
    /// A canonical digit run naming a value above `u32::MAX`.
    Overflow,
}

/// The UTC day containing `epoch_ms`, as a floored day index.
pub(super) const fn utc_day_from_epoch_ms(epoch_ms: i64) -> i64 {
    epoch_ms.div_euclid(MS_PER_UTC_DAY)
}

/// The reservation key for one provider on one UTC day.
///
/// `provider` is hex-encoded byte by byte, so the returned key always
/// has exactly four `:`-separated fields whatever the name contains.
pub(super) fn reservation_key(utc_day: i64, provider: &str) -> String {
    let mut key = String::with_capacity(RESERVATION_KEY_PREFIX.len() + 24 + provider.len() * 2);
    key.push_str(RESERVATION_KEY_PREFIX);
    key.push(':');
    key.push_str(&utc_day.to_string());
    key.push(':');
    for byte in provider.as_bytes() {
        key.push(HEX_DIGITS[usize::from(byte >> 4)]);
        key.push(HEX_DIGITS[usize::from(byte & 0x0f)]);
    }
    key
}

/// Render a committed unit count in the one form the parser accepts.
pub(super) fn format_units(units: u32) -> String {
    units.to_string()
}

/// Read a stored unit count, refusing anything the formatter would not
/// have written.
pub(super) fn parse_units(raw: &str) -> Result<u32, MalformedUnits> {
    if raw.is_empty() {
        return Err(MalformedUnits::Empty);
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(MalformedUnits::NonDigit);
    }
    if raw.len() > 1 && raw.starts_with('0') {
        return Err(MalformedUnits::LeadingZero);
    }
    raw.parse::<u32>().map_err(|_| MalformedUnits::Overflow)
}

/// What one storage-level reservation attempt established.
///
/// Only `Committed` means a unit is durably spent; every other variant
/// means nothing was written and no paid call may follow. The refusals stay
/// distinct because an exhausted cap is the budget working as configured
/// while malformed state or a failed write is accounting the operator needs
/// to know is broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoredReservation {
    /// One unit is committed and visible to every later reader.
    Committed {
        /// The UTC day the unit landed in, as a floored day index.
        utc_day: i64,
        /// Units committed for that provider-day, INCLUDING this one.
        used: u32,
        /// The cap this reservation was checked against.
        cap: u32,
    },
    /// The cap is zero, or the day's units are already spent. Nothing
    /// written.
    CapExhausted,
    /// The stored value is not a canonical count. Refused rather than read
    /// as zero, and left byte-identical.
    MalformedState,
    /// A storage operation failed, so no unit can be claimed as spent.
    WriteFailed,
}

/// Reserve one paid-probe unit for `provider` against `cap`, in the UTC day
/// containing `now_epoch_ms`.
///
/// `now_epoch_ms` is sampled by the caller rather than read here, so the
/// day a reservation lands in is the day the caller decided on and cannot
/// drift between the cap gate and the commit.
///
/// The whole check-and-increment runs inside ONE immediate transaction,
/// which takes the write lock BEFORE the read. That ordering is the
/// correctness of the cap: with a deferred transaction two connections can
/// both read the same count, and one of them either double-spends a unit or
/// loses its own write. `cap` is compared against the count the same
/// transaction read, and the stored value becomes exactly that count plus
/// one -- never an unconditional increment, so a full day cannot be pushed
/// past its cap.
///
/// Refusals write nothing and leave the stored bytes untouched. A missing
/// row means no unit has been spent for that provider-day; a value the
/// codec would not have written is [`StoredReservation::MalformedState`],
/// never a zero, because reading corrupt accounting as "nothing spent"
/// licenses a full cap's worth of paid calls every time it is read.
pub(super) fn reserve_one_unit(
    conn: &mut Connection,
    provider: &str,
    cap: u32,
    now_epoch_ms: i64,
) -> StoredReservation {
    reserve_one_unit_instrumented(conn, provider, cap, now_epoch_ms, &|| ())
}

/// The whole reservation body, with one synchronization point.
///
/// `after_read` runs inside the transaction, immediately after the stored
/// count has been read and before anything is written. It exists so a test
/// can act at exactly that boundary and observe whether the write lock was
/// already held; production calls this same body through
/// [`reserve_one_unit`] with a hook that does nothing, so there is no
/// second implementation to drift.
fn reserve_one_unit_instrumented(
    conn: &mut Connection,
    provider: &str,
    cap: u32,
    now_epoch_ms: i64,
    after_read: &dyn Fn(),
) -> StoredReservation {
    let utc_day = utc_day_from_epoch_ms(now_epoch_ms);
    let key = reservation_key(utc_day, provider);

    let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate) else {
        return StoredReservation::WriteFailed;
    };

    let stored: Option<String> = match tx
        .query_row(SELECT_UNITS_SQL, [&key], |row| row.get(0))
        .optional()
    {
        Ok(stored) => stored,
        Err(_) => return StoredReservation::WriteFailed,
    };

    after_read();

    let used = match stored.as_deref().map(parse_units) {
        None => 0,
        Some(Ok(units)) => units,
        Some(Err(_)) => return StoredReservation::MalformedState,
    };

    let Some(reserved) = used.checked_add(1) else {
        return StoredReservation::CapExhausted;
    };
    if reserved > cap {
        return StoredReservation::CapExhausted;
    }

    if tx
        .execute(UPSERT_UNITS_SQL, [&key, &format_units(reserved)])
        .is_err()
    {
        return StoredReservation::WriteFailed;
    }
    if tx.commit().is_err() {
        return StoredReservation::WriteFailed;
    }

    StoredReservation::Committed {
        utc_day,
        used: reserved,
        cap,
    }
}

/// Read one provider-day's committed count out of the control storage.
const SELECT_UNITS_SQL: &str = "SELECT value FROM meta WHERE key = ?1";

/// Write one provider-day's committed count, creating the row on the first
/// unit of the day.
const UPSERT_UNITS_SQL: &str = "\
INSERT INTO meta (key, value) VALUES (?1, ?2) \
ON CONFLICT(key) DO UPDATE SET value = ?2";

/// Lowercase hex alphabet for the provider field.
const HEX_DIGITS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

#[cfg(test)]
#[path = "paid_probe_tests.rs"]
mod tests;
