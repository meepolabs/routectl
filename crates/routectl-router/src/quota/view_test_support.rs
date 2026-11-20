//! Test reads of an account's windows through the placement-safe view.

use super::freshness::ObservationStamp;
use super::key::SeatKey;
use super::store::QuotaStore;
use super::view::{AccountReading, AccountWindow};

pub const FIVE_HOURS_SECS: u64 = 5 * 3_600;
pub const SEVEN_DAYS_SECS: u64 = 7 * 86_400;

/// `key`'s reading at `now`, or `None` when the account is not configured.
pub fn reading(
    store: &QuotaStore,
    key: &SeatKey,
    now: &ObservationStamp,
) -> Option<AccountReading> {
    store
        .readings(now)
        .accounts
        .into_iter()
        .find(|reading| reading.account == key.as_str())
}

/// `key`'s account-wide window of `length_secs` as read at `now`, stale or
/// not, or `None` when none is held.
pub fn account_wide(
    store: &QuotaStore,
    key: &SeatKey,
    now: &ObservationStamp,
    length_secs: u64,
) -> Option<AccountWindow> {
    reading(store, key, now)?
        .windows
        .into_iter()
        .find(|window| window.window.length_secs == length_secs && window.window.scope.is_none())
}

/// The used fraction held for `key`'s account-wide five-hour window, whether
/// or not it is still fresh at `now`.
pub fn held_five_hour(store: &QuotaStore, key: &SeatKey, now: &ObservationStamp) -> Option<f64> {
    account_wide(store, key, now, FIVE_HOURS_SECS).map(|window| window.window.used)
}

/// The used fraction placement reads for `key`'s account-wide five-hour
/// window at `now`: `None` when none is held or it is stale.
pub fn fresh_five_hour(store: &QuotaStore, key: &SeatKey, now: &ObservationStamp) -> Option<f64> {
    account_wide(store, key, now, FIVE_HOURS_SECS)
        .filter(|window| !window.stale)
        .map(|window| window.window.used)
}

/// The used fraction held for `key`'s account-wide seven-day window, whether
/// or not it is still fresh at `now`.
pub fn held_seven_day(store: &QuotaStore, key: &SeatKey, now: &ObservationStamp) -> Option<f64> {
    account_wide(store, key, now, SEVEN_DAYS_SECS).map(|window| window.window.used)
}
