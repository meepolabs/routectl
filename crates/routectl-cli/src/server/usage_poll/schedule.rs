//! When each managed seat's usage is polled.
//!
//! A pure state machine: the caller passes the wall clock (epoch ms) and,
//! per seat, when the meter last accepted a reading from any source; the
//! jitter source is injected. Nothing here sleeps, reads a clock or does
//! I/O. All time arithmetic saturates, because a vendor-supplied wait or a
//! skewed clock must never overflow (release builds abort on panic).
//!
//! A seat is due when `now >= max(last accepted reading, last attempt) +
//! 5 min + its jitter (0-60 s, re-drawn after each attempt)`, its error
//! backoff has elapsed and its vendor is not paused. A seat the schedule has
//! not seen before, with no reading and no attempt, is due at a random point
//! in its first 5 minutes so a fresh daemon does not poll every seat at
//! once. A sweep returns at most one seat per vendor, so a long suspend
//! never ends in a burst.

use std::collections::BTreeMap;
use std::time::Duration;

use rand::RngExt;
use rand::rngs::SmallRng;
use routectl_router::UsageVendor;

use crate::server::seat_driver::SeatVerdict;

const MINUTE_MS: i64 = 60_000;

/// How long after the last reading or attempt a seat is next polled.
const POLL_INTERVAL_MS: i64 = 5 * MINUTE_MS;
/// The largest per-seat jitter added to [`POLL_INTERVAL_MS`].
const MAX_JITTER_MS: u64 = 60_000;
/// First-sight due times are spread over this span.
const BOOT_SPREAD_MS: u64 = 5 * 60_000;
/// Each consecutive error adds this much backoff, up to [`MAX_BACKOFF_MS`].
const BACKOFF_STEP_MS: i64 = 5 * MINUTE_MS;
const MAX_BACKOFF_MS: i64 = 15 * MINUTE_MS;
/// The longest vendor pause a `Retry-After` can ask for.
const MAX_VENDOR_PAUSE_MS: u64 = 60 * 60_000;
/// The vendor pause after a 429 that names no wait.
const DEFAULT_VENDOR_PAUSE_MS: i64 = 15 * MINUTE_MS;

/// The closed result of one seat's poll, as the poll job lands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PollOutcome {
    /// The meter accepted a reading with windows.
    Accepted,
    /// The meter accepted a complete report that the account has no windows.
    NoWindows,
    /// The seat had no usable access token to send.
    TokenNotReady,
    /// 429: the vendor asked to be left alone, for the wait it named if any.
    Paused(Option<Duration>),
    /// Any 3xx; never followed.
    Redirect,
    /// 401 or 403.
    Auth,
    /// Any other non-200 status.
    Http(u16),
    Timeout,
    Transport,
    /// The body exceeded the size cap.
    TooLarge,
    /// The body did not reduce, or the meter refused the observation.
    Malformed,
    /// The seat's credential changed while the poll was in flight.
    Superseded,
}

impl PollOutcome {
    /// A stable lowercase token naming the outcome kind.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::NoWindows => "no_windows",
            Self::TokenNotReady => "token_not_ready",
            Self::Paused(_) => "paused",
            Self::Redirect => "redirect",
            Self::Auth => "auth",
            Self::Http(_) => "http",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::TooLarge => "too_large",
            Self::Malformed => "malformed",
            Self::Superseded => "superseded",
        }
    }

    /// How the outcome counts toward the driver's health. Never
    /// `Definitive`: a usage poll failure may not judge a login, and a 401
    /// from a usage endpoint is the refresh job's business, not this one's.
    pub(super) const fn verdict(self) -> SeatVerdict {
        match self {
            Self::Accepted | Self::NoWindows | Self::TokenNotReady | Self::Superseded => {
                SeatVerdict::Quiet
            }
            Self::Paused(_)
            | Self::Redirect
            | Self::Auth
            | Self::Http(_)
            | Self::Timeout
            | Self::Transport
            | Self::TooLarge
            | Self::Malformed => SeatVerdict::Transient,
        }
    }

    const fn is_success(self) -> bool {
        matches!(self, Self::Accepted | Self::NoWindows)
    }

    /// Whether the outcome extends the seat's error backoff. A 429 pauses
    /// the vendor instead; a missing token or a re-login race says nothing
    /// about the endpoint.
    const fn is_error(self) -> bool {
        matches!(
            self,
            Self::Redirect
                | Self::Auth
                | Self::Http(_)
                | Self::Timeout
                | Self::Transport
                | Self::TooLarge
                | Self::Malformed
        )
    }
}

/// Where per-seat jitter comes from.
pub(super) trait JitterSource: Send {
    /// A value in `0..=max_ms`.
    fn draw_ms(&mut self, max_ms: u64) -> u64;
}

/// The production jitter source: a small non-cryptographic generator
/// seeded from the OS. Jitter only staggers polls; it guards nothing.
pub(super) struct RandomJitter(SmallRng);

impl RandomJitter {
    pub(super) fn from_os() -> Self {
        Self(rand::make_rng())
    }

    #[cfg(test)]
    pub(super) fn seeded(seed: u64) -> Self {
        Self(rand::SeedableRng::seed_from_u64(seed))
    }
}

impl JitterSource for RandomJitter {
    fn draw_ms(&mut self, max_ms: u64) -> u64 {
        self.0.random_range(0..=max_ms)
    }
}

/// One managed seat as a sweep sees it.
#[derive(Debug, Clone, Copy)]
pub(super) struct SeatCandidate<'a> {
    pub(super) account: &'a str,
    pub(super) vendor: UsageVendor,
    /// When the meter last accepted a reading for the seat, from any
    /// source, in epoch ms.
    pub(super) last_accepted_ms: Option<i64>,
}

/// One seat's poll state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SeatState {
    first_due_ms: i64,
    jitter_ms: i64,
    next_due_ms: i64,
    last_attempt_ms: Option<i64>,
    last_outcome: Option<PollOutcome>,
    error_streak: u32,
}

impl SeatState {
    /// When the seat was next due as of the last sweep, in epoch ms.
    pub(super) const fn next_due_ms(&self) -> i64 {
        self.next_due_ms
    }

    pub(super) const fn last_attempt_ms(&self) -> Option<i64> {
        self.last_attempt_ms
    }

    pub(super) const fn last_outcome(&self) -> Option<PollOutcome> {
        self.last_outcome
    }

    pub(super) const fn error_streak(&self) -> u32 {
        self.error_streak
    }

    fn due_at(&self, last_accepted_ms: Option<i64>) -> i64 {
        let cadence_due = match last_accepted_ms.max(self.last_attempt_ms) {
            None => self.first_due_ms,
            Some(base) => base
                .saturating_add(POLL_INTERVAL_MS)
                .saturating_add(self.jitter_ms),
        };
        let backoff_due = self.last_attempt_ms.map_or(i64::MIN, |attempt| {
            attempt.saturating_add(backoff_ms(self.error_streak))
        });
        cadence_due.max(backoff_due)
    }
}

/// The error backoff after `streak` consecutive errors: 5, 10, then 15
/// minutes.
fn backoff_ms(streak: u32) -> i64 {
    i64::from(streak)
        .saturating_mul(BACKOFF_STEP_MS)
        .min(MAX_BACKOFF_MS)
}

/// The vendor pause a 429 asks for, capped at one hour.
fn vendor_pause_ms(retry_after: Option<Duration>) -> i64 {
    retry_after.map_or(DEFAULT_VENDOR_PAUSE_MS, |wait| {
        let capped = u64::try_from(wait.as_millis())
            .unwrap_or(u64::MAX)
            .min(MAX_VENDOR_PAUSE_MS);
        i64::try_from(capped).unwrap_or(i64::MAX)
    })
}

fn draw_ms_capped(jitter: &mut dyn JitterSource, max_ms: u64) -> i64 {
    let drawn = jitter.draw_ms(max_ms).min(max_ms);
    i64::try_from(drawn).unwrap_or(i64::MAX)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct VendorPauses {
    anthropic: Option<i64>,
    codex: Option<i64>,
}

impl VendorPauses {
    const fn slot(&mut self, vendor: UsageVendor) -> &mut Option<i64> {
        match vendor {
            UsageVendor::Anthropic => &mut self.anthropic,
            UsageVendor::Codex => &mut self.codex,
        }
    }

    const fn until(&self, vendor: UsageVendor) -> Option<i64> {
        match vendor {
            UsageVendor::Anthropic => self.anthropic,
            UsageVendor::Codex => self.codex,
        }
    }

    fn expire(&mut self, now_ms: i64) {
        for vendor in [UsageVendor::Anthropic, UsageVendor::Codex] {
            let slot = self.slot(vendor);
            if slot.is_some_and(|until| now_ms >= until) {
                *slot = None;
            }
        }
    }
}

/// Every managed seat's poll state and each vendor's pause.
pub(super) struct PollSchedule<J: JitterSource> {
    jitter: J,
    seats: BTreeMap<String, SeatState>,
    pauses: VendorPauses,
}

impl<J: JitterSource> PollSchedule<J> {
    pub(super) const fn new(jitter: J) -> Self {
        Self {
            jitter,
            seats: BTreeMap::new(),
            pauses: VendorPauses {
                anthropic: None,
                codex: None,
            },
        }
    }

    /// Bring the state in line with `seats` (the current managed set) at
    /// `now_ms` and return the indices into `seats` to poll, in input order:
    /// per vendor, the due seat that has waited longest, unless the vendor
    /// is paused. State for a seat not in `seats` is dropped; a seat seen
    /// for the first time gets a random first due time within 5 minutes.
    pub(super) fn sweep(&mut self, now_ms: i64, seats: &[SeatCandidate<'_>]) -> Vec<usize> {
        self.seats
            .retain(|account, _| seats.iter().any(|seat| seat.account == account));
        self.pauses.expire(now_ms);
        let mut picks = PerVendorPick::default();
        for (index, seat) in seats.iter().enumerate() {
            let due_at = self.refresh_due(now_ms, seat);
            if now_ms >= due_at && self.pauses.until(seat.vendor).is_none() {
                picks.offer(seat.vendor, due_at, index);
            }
        }
        picks.into_sorted_indices()
    }

    fn refresh_due(&mut self, now_ms: i64, seat: &SeatCandidate<'_>) -> i64 {
        let jitter = &mut self.jitter;
        let state = self
            .seats
            .entry(seat.account.to_string())
            .or_insert_with(|| SeatState {
                first_due_ms: now_ms.saturating_add(draw_ms_capped(jitter, BOOT_SPREAD_MS)),
                jitter_ms: draw_ms_capped(jitter, MAX_JITTER_MS),
                next_due_ms: now_ms,
                last_attempt_ms: None,
                last_outcome: None,
                error_streak: 0,
            });
        state.next_due_ms = state.due_at(seat.last_accepted_ms);
        state.next_due_ms
    }

    /// Land one poll's `outcome` for `account` on `vendor`, attempted at
    /// `now_ms`. A 429 pauses the whole vendor even when the seat has since
    /// left the managed set.
    pub(super) fn record(
        &mut self,
        account: &str,
        vendor: UsageVendor,
        now_ms: i64,
        outcome: PollOutcome,
    ) {
        if let PollOutcome::Paused(retry_after) = outcome {
            *self.pauses.slot(vendor) = Some(now_ms.saturating_add(vendor_pause_ms(retry_after)));
        }
        let Some(state) = self.seats.get_mut(account) else {
            return;
        };
        state.last_attempt_ms = Some(now_ms);
        state.last_outcome = Some(outcome);
        state.jitter_ms = draw_ms_capped(&mut self.jitter, MAX_JITTER_MS);
        if outcome.is_success() {
            state.error_streak = 0;
        } else if outcome.is_error() {
            state.error_streak = state.error_streak.saturating_add(1);
        }
    }

    /// The poll state for `account`, if it is in the managed set.
    pub(super) fn seat(&self, account: &str) -> Option<&SeatState> {
        self.seats.get(account)
    }

    /// When `vendor`'s pause ends, in epoch ms, if it was paused at the
    /// last sweep or paused since.
    pub(super) const fn pause_until(&self, vendor: UsageVendor) -> Option<i64> {
        self.pauses.until(vendor)
    }
}

/// The earliest-due seat per vendor seen so far in a sweep.
#[derive(Default)]
struct PerVendorPick {
    anthropic: Option<(i64, usize)>,
    codex: Option<(i64, usize)>,
}

impl PerVendorPick {
    fn offer(&mut self, vendor: UsageVendor, due_at: i64, index: usize) {
        let slot = match vendor {
            UsageVendor::Anthropic => &mut self.anthropic,
            UsageVendor::Codex => &mut self.codex,
        };
        if slot.is_none_or(|(kept_due, _)| due_at < kept_due) {
            *slot = Some((due_at, index));
        }
    }

    fn into_sorted_indices(self) -> Vec<usize> {
        let mut indices: Vec<usize> = [self.anthropic, self.codex]
            .into_iter()
            .flatten()
            .map(|(_, index)| index)
            .collect();
        indices.sort_unstable();
        indices
    }
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod tests;
