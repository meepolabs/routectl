//! One `account_readings` row: an accepted account-meter observation as the
//! history table stores it.
//!
//! Each token enum here is the source of truth for one closed CHECK set in
//! `schema::CREATE_ACCOUNT_READINGS_TABLE`: its `as_str` values are exactly the
//! tokens that column accepts. Add a variant only alongside a schema step that
//! widens the CHECK.
//!
//! History is best effort: the writer commits one observation's rows in one
//! transaction or none of them, counts the outcome, and never reports back to
//! the producer. Nothing reads this table to rebuild in-memory state.

use rusqlite::Connection;

use crate::handle::UsageCounters;

/// Where a reading came from. Column `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountReadingSource {
    /// Quota headers on a proxied response.
    Response,
    /// The background usage poll.
    Poll,
}

impl AccountReadingSource {
    /// Every variant, for the CHECK-token weld.
    pub const ALL: [Self; 2] = [Self::Response, Self::Poll];

    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::Poll => "poll",
        }
    }
}

/// What the observation reported. Column `report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountReadingReport {
    /// The row carries one window.
    Windows,
    /// A complete report that the account has no windows; every window column
    /// is NULL.
    NoWindows,
}

impl AccountReadingReport {
    /// Every variant, for the CHECK-token weld.
    pub const ALL: [Self; 2] = [Self::Windows, Self::NoWindows];

    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::NoWindows => "no_windows",
        }
    }
}

/// A window's horizon class. Column `length_class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowLengthClass {
    /// At most one day.
    Short,
    /// Longer than one day.
    Long,
}

impl WindowLengthClass {
    /// Every variant, for the CHECK-token weld.
    pub const ALL: [Self; 2] = [Self::Short, Self::Long];

    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Long => "long",
        }
    }
}

/// Whether a window's clock was running. Column `started`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowStarted {
    /// Running.
    Yes,
    /// Not started; nothing used.
    No,
    /// The source does not say.
    Unknown,
}

impl WindowStarted {
    /// Every variant, for the CHECK-token weld.
    pub const ALL: [Self; 3] = [Self::Yes, Self::No, Self::Unknown];

    /// The stored token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }
}

/// The window columns of a `windows` row.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountReadingWindow {
    /// Reported window length in seconds; positive.
    pub window_secs: i64,
    /// Vendor scope tag, or `None` for an account-wide window.
    pub scope: Option<String>,
    /// The class the length falls in.
    pub length_class: WindowLengthClass,
    /// Used fraction in `[0.0, 1.0]`.
    pub used: f64,
    /// Reset instant (epoch ms), or `None` when the source reported none.
    pub reset_ms: Option<i64>,
    /// Whether the window was running.
    pub started: WindowStarted,
}

/// One `account_readings` row. The `report` column is derived from `window`,
/// so a `no_windows` row cannot carry window values.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountReadingRow {
    /// When the observation was read (epoch ms).
    pub ts_ms: i64,
    /// The account the reading is for.
    pub account: String,
    /// Where it came from.
    pub source: AccountReadingSource,
    /// The window, or `None` for a `no_windows` row.
    pub window: Option<AccountReadingWindow>,
}

impl AccountReadingRow {
    /// The `report` token this row is stored with.
    #[must_use]
    pub const fn report(&self) -> AccountReadingReport {
        match self.window {
            Some(_) => AccountReadingReport::Windows,
            None => AccountReadingReport::NoWindows,
        }
    }
}

const INSERT_SQL: &str = "INSERT INTO account_readings \
     (ts_ms, account, source, report, window_secs, scope, length_class, used, reset_ms, started) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

/// Log a history write failure at most this often (every Nth failure); the
/// first one always logs.
const WRITE_ERROR_LOG_INTERVAL: u64 = 1024;

/// Insert one observation's rows in a single transaction: either every row
/// lands or none does. Returns the number of rows inserted.
pub fn insert_account_readings(
    conn: &Connection,
    rows: &[AccountReadingRow],
) -> Result<usize, rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    let mut inserted = 0;
    {
        let mut stmt = tx.prepare_cached(INSERT_SQL)?;
        for row in rows {
            let window = row.window.as_ref();
            inserted += stmt.execute(rusqlite::params![
                row.ts_ms,
                row.account,
                row.source.as_str(),
                row.report().as_str(),
                window.map(|w| w.window_secs),
                window.and_then(|w| w.scope.as_deref()),
                window.map(|w| w.length_class.as_str()),
                window.map(|w| w.used),
                window.and_then(|w| w.reset_ms),
                window.map(|w| w.started.as_str()),
            ])?;
        }
    }
    tx.commit()?;
    Ok(inserted)
}

/// The writer's handling of one observation: commit its rows and count the
/// outcome. A missing connection (the writer is running degraded) or a failed
/// insert drops the observation and counts one history write error.
///
/// Kept off the writer's shared `write_errors` on purpose: that counter gates
/// capability pre-flight and the ledger health reading, and a lost history row
/// says nothing about either.
pub fn persist_account_readings(
    conn: Option<&Connection>,
    rows: &[AccountReadingRow],
    counters: &UsageCounters,
) {
    let outcome = conn.map_or(Err(None), |conn| {
        insert_account_readings(conn, rows).map_err(Some)
    });
    match outcome {
        Ok(_) => counters.incr_account_readings_persisted(),
        Err(err) => note_write_error(err.as_ref(), rows.len(), counters),
    }
}

fn note_write_error(err: Option<&rusqlite::Error>, rows: usize, counters: &UsageCounters) {
    let prior = counters.incr_account_readings_write_errors();
    if prior == 0 || (prior + 1).is_multiple_of(WRITE_ERROR_LOG_INTERVAL) {
        tracing::warn!(
            target: "routectl_usage::account_reading",
            error = err.map(ToString::to_string).unwrap_or_default(),
            rows,
            write_errors = prior + 1,
            "account reading history write failed -- observation dropped"
        );
    }
}

#[cfg(test)]
#[path = "account_reading_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "account_reading_writer_tests.rs"]
mod writer_tests;
