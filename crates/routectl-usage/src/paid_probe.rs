//! Durable key and value codec for paid-probe reservations.
//!
//! A reservation is one unit of paid-probe budget spent against a
//! configured provider on one UTC day. It is recorded in the existing
//! key/value control storage, so the whole contract is two strings: the
//! key that names the (day, provider) bucket and the value that carries
//! the committed count.
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
//! Nothing outside the tests reads this module yet: the codec is fixed
//! before the accounting path that stores its output, so the durable
//! encoding is settled independently of any caller. Hence the crate-local
//! dead-code allowance below, which the first production caller removes.
//! The four entry points and the refusal enum are `pub(super)` (visible to
//! the sibling modules that will store and read a reservation, no wider) so
//! the crate's published surface does not grow for an internal encoding.
//! Everything they are built from stays private to this module.

#![cfg_attr(not(test), allow(dead_code))]
#![expect(
    clippy::redundant_pub_crate,
    reason = "this module is private, so a crate-wide visibility reads as \
              redundant -- but the sibling modules that will store and read a \
              reservation have to name these items, and widening them to `pub` \
              to satisfy the lint would publish an internal encoding"
)]

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

/// Lowercase hex alphabet for the provider field.
const HEX_DIGITS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

#[cfg(test)]
#[path = "paid_probe_tests.rs"]
mod tests;
