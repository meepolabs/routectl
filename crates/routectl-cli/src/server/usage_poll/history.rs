//! The one place an accepted meter reading becomes reading-history rows.

use std::time::{SystemTime, UNIX_EPOCH};

use routectl_router::{AcceptedReading, LengthClass, ReadingWindow, Started};
use routectl_usage::{
    AccountReadingRow, AccountReadingSource, AccountReadingWindow, WindowLengthClass, WindowStarted,
};

/// The history rows for one accepted poll reading of `account`: one row
/// per window, or a single `no_windows` row for a complete report that the
/// account has none.
pub(super) fn rows(account: &str, accepted: &AcceptedReading) -> Vec<AccountReadingRow> {
    let row = |window| AccountReadingRow {
        ts_ms: epoch_ms(accepted.observed_at),
        account: account.to_owned(),
        source: AccountReadingSource::Poll,
        window,
    };
    if accepted.windows.is_empty() {
        return vec![row(None)];
    }
    accepted
        .windows
        .iter()
        .map(|window| row(Some(history_window(window))))
        .collect()
}

fn history_window(window: &ReadingWindow) -> AccountReadingWindow {
    AccountReadingWindow {
        window_secs: i64::try_from(window.length_secs).unwrap_or(i64::MAX),
        scope: window.scope.clone(),
        length_class: match window.class {
            LengthClass::Short => WindowLengthClass::Short,
            LengthClass::Long => WindowLengthClass::Long,
        },
        used: window.used,
        reset_ms: window.reset.map(epoch_ms),
        started: match window.started {
            Started::Yes => WindowStarted::Yes,
            Started::No => WindowStarted::No,
            Started::Unknown => WindowStarted::Unknown,
        },
    }
}

/// `at` in epoch milliseconds, negative before the epoch, saturating.
pub(super) fn epoch_ms(at: SystemTime) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}
