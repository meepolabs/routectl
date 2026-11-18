//! The account meter's reading model: one validated window per reported
//! `(length, scope)` pair, and the observation that carries a set of them.
//!
//! A window is identified by what the upstream REPORTED about it -- its length
//! and an optional vendor scope -- never by a role guessed from its name. The
//! length alone decides its class, so two vendors reporting a five-hour window
//! classify it the same way without a per-vendor table.
//!
//! Every constructor validates, and every refusal is a [`RejectionReason`] the
//! store can count. There is no way to hold a [`WindowReading`] whose length is
//! unclassifiable, whose used fraction is out of scale, whose reset was not
//! checked against the window's own length, or whose started state contradicts
//! its other fields; and no way to hold an observation carrying the same key
//! twice. The derive lists follow the module rule in `quota/mod.rs`: no
//! `Default`, no `Serialize`, no `Ord`.

use std::time::{Duration, SystemTime};

use super::curation::RESET_TOLERANCE;
use super::freshness::{ObservationStamp, ValidatedReset, accept_reset};
use super::reduce::RejectionReason;
use super::window::Utilization;

/// Longest window length classified `Short`, in seconds (24 hours, inclusive).
pub const SHORT_WINDOW_MAX_SECS: u64 = 86_400;

/// Longest window length accepted at all, in seconds (31 days, inclusive). A
/// longer one is far more likely a unit error than a real quota window.
pub const MAX_WINDOW_SECS: u64 = 31 * 86_400;

/// Most windows one observation may carry. Bounds what one upstream payload
/// can make the store hold per account.
pub const MAX_WINDOWS_PER_OBSERVATION: usize = 16;

/// Longest scope tag accepted, in bytes. The scope is persisted and served, so
/// it is held to one short canonical layout.
pub const MAX_SCOPE_BYTES: usize = 32;

/// Which horizon a window describes, read from its reported length alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthClass {
    /// At most [`SHORT_WINDOW_MAX_SECS`].
    Short,
    /// Longer than [`SHORT_WINDOW_MAX_SECS`], at most [`MAX_WINDOW_SECS`].
    Long,
}

impl LengthClass {
    /// Classify a reported window length, or refuse it as `UnknownLength`
    /// when it is zero or longer than [`MAX_WINDOW_SECS`].
    pub const fn of(length_secs: u64) -> Result<Self, RejectionReason> {
        if length_secs == 0 || length_secs > MAX_WINDOW_SECS {
            Err(RejectionReason::UnknownLength)
        } else if length_secs <= SHORT_WINDOW_MAX_SECS {
            Ok(Self::Short)
        } else {
            Ok(Self::Long)
        }
    }
}

/// A vendor tag narrowing a window below the whole account, such as a
/// per-model window: non-empty, at most [`MAX_SCOPE_BYTES`], `[a-z0-9_]` only.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scope(String);

impl Scope {
    /// Accept a scope tag in its one canonical layout, or refuse it as
    /// `MalformedBody`. Never normalizes: an uppercase or padded tag is a
    /// different upstream shape, not a spelling of a known one.
    pub fn new(raw: &str) -> Result<Self, RejectionReason> {
        let canonical = !raw.is_empty()
            && raw.len() <= MAX_SCOPE_BYTES
            && raw
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if canonical {
            Ok(Self(raw.to_owned()))
        } else {
            Err(RejectionReason::MalformedBody)
        }
    }

    /// The canonical tag.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One window's identity within an account: its reported length and, for a
/// window narrower than the whole account, its scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowKey {
    length_secs: u64,
    scope: Option<Scope>,
}

impl WindowKey {
    /// A key for a window of `length_secs`, refused as `UnknownLength` when the
    /// length cannot be classified.
    pub fn new(length_secs: u64, scope: Option<Scope>) -> Result<Self, RejectionReason> {
        LengthClass::of(length_secs)?;
        Ok(Self { length_secs, scope })
    }

    /// The reported window length, in seconds.
    pub const fn length_secs(&self) -> u64 {
        self.length_secs
    }

    /// The window's length as a `Duration`.
    pub const fn length(&self) -> Duration {
        Duration::from_secs(self.length_secs)
    }

    /// The scope, or `None` for an account-wide window.
    pub const fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }

    /// The window's class. Total, because construction refused every length
    /// that has none.
    pub const fn class(&self) -> LengthClass {
        if self.length_secs <= SHORT_WINDOW_MAX_SECS {
            LengthClass::Short
        } else {
            LengthClass::Long
        }
    }
}

/// Whether the window's clock is running.
///
/// `Unknown` is a source that does not say (the response headers); it is not
/// evidence either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// The window is running and resets at its reported reset.
    Yes,
    /// The window has not started: nothing used and no reset yet.
    No,
    /// The source does not report whether the window has started.
    Unknown,
}

/// Where a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingSource {
    /// Quota headers on a proxied response.
    Response,
    /// The background usage poll.
    Poll,
}

/// One validated window reading.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowReading {
    key: WindowKey,
    used: Utilization,
    reset: Option<ValidatedReset>,
    started: Started,
}

impl WindowReading {
    /// Validate one reported window observed at `observed`.
    ///
    /// Refused `InvalidUtilization` for a non-finite or negative `used` (a
    /// value above 1.0 saturates to exhausted). Refused `InvalidStarted` when
    /// the started state disagrees with the rest: an unstarted window with any
    /// usage or with a reset, or a window that is or may be running without a
    /// reset. A reset is accepted only through [`accept_reset`] against the
    /// window's OWN length, and refused with that check's reason otherwise.
    pub fn new(
        key: WindowKey,
        used: f64,
        reset_at: Option<SystemTime>,
        started: Started,
        observed: &ObservationStamp,
    ) -> Result<Self, RejectionReason> {
        let used = Utilization::new(used).ok_or(RejectionReason::InvalidUtilization)?;
        let reset = match (started, reset_at) {
            (Started::No, None) if used.fraction() == 0.0 => None,
            (Started::No, _) | (Started::Yes | Started::Unknown, None) => {
                return Err(RejectionReason::InvalidStarted);
            }
            (Started::Yes | Started::Unknown, Some(at)) => Some(
                accept_reset(at, observed, key.length(), RESET_TOLERANCE)
                    .map_err(|rejection| RejectionReason::from_reset(&rejection))?,
            ),
        };
        Ok(Self {
            key,
            used,
            reset,
            started,
        })
    }

    /// The window's identity.
    pub const fn key(&self) -> &WindowKey {
        &self.key
    }

    /// The window's class.
    pub const fn class(&self) -> LengthClass {
        self.key.class()
    }

    /// The used fraction.
    pub const fn used(&self) -> &Utilization {
        &self.used
    }

    /// The validated reset; `None` only for an unstarted window.
    pub const fn reset(&self) -> Option<ValidatedReset> {
        self.reset
    }

    /// Whether the window is running.
    pub const fn started(&self) -> Started {
        self.started
    }
}

/// A non-empty set of windows with distinct keys, at most
/// [`MAX_WINDOWS_PER_OBSERVATION`] of them.
///
/// Non-empty so a complete observation with no windows can only be spelled
/// [`ObservationReport::NoWindows`]: an empty set must never read as a second
/// way to say "no cap".
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSet(Vec<WindowReading>);

impl WindowSet {
    /// Accept a set of windows, or refuse it as `MalformedBody` when it is
    /// empty, over the count bound, or carries one key twice.
    pub fn new(windows: Vec<WindowReading>) -> Result<Self, RejectionReason> {
        if windows.is_empty() || windows.len() > MAX_WINDOWS_PER_OBSERVATION {
            return Err(RejectionReason::MalformedBody);
        }
        let has_duplicate = windows
            .iter()
            .enumerate()
            .any(|(i, w)| windows[..i].iter().any(|earlier| earlier.key == w.key));
        if has_duplicate {
            return Err(RejectionReason::MalformedBody);
        }
        Ok(Self(windows))
    }

    /// The windows, in the order the source reported them.
    pub fn as_slice(&self) -> &[WindowReading] {
        &self.0
    }
}

/// What one observation says about an account's windows.
#[derive(Debug, Clone, PartialEq)]
pub enum ObservationReport {
    /// Some windows; a window this set omits is unchanged, never erased.
    Partial(WindowSet),
    /// Every window the account has; one this set omits no longer applies.
    Complete(WindowSet),
    /// A complete, schema-valid report that the account has no windows.
    NoWindows,
}

/// One source-tagged, stamped observation of an account's windows.
///
/// Every window in it must have been validated against `observed`, the same
/// stamp the observation carries, so each reset was bounded from the instant
/// the store will age it from.
#[derive(Debug, Clone)]
pub struct AccountObservation {
    source: ReadingSource,
    observed: ObservationStamp,
    report: ObservationReport,
}

impl AccountObservation {
    /// An observation from `source`, read at `observed`.
    pub const fn new(
        source: ReadingSource,
        observed: ObservationStamp,
        report: ObservationReport,
    ) -> Self {
        Self {
            source,
            observed,
            report,
        }
    }

    /// Where the observation came from.
    pub const fn source(&self) -> ReadingSource {
        self.source
    }

    /// When the observation was read, on both clocks.
    pub const fn observed(&self) -> &ObservationStamp {
        &self.observed
    }

    /// What the observation reports.
    pub const fn report(&self) -> &ObservationReport {
        &self.report
    }
}

#[cfg(test)]
#[path = "reading_tests.rs"]
mod reading_tests;
