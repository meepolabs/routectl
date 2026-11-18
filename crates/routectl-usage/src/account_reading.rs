//! One `account_readings` row: an accepted account-meter observation as the
//! history table stores it.
//!
//! Each token enum here is the source of truth for one closed CHECK set in
//! `schema::CREATE_ACCOUNT_READINGS_TABLE`: its `as_str` values are exactly the
//! tokens that column accepts. Add a variant only alongside a schema step that
//! widens the CHECK.

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

#[cfg(test)]
#[path = "account_reading_tests.rs"]
mod tests;
