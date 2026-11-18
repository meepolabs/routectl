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
//! never ends in a burst. Every deadline (cadence, backoff, the end of a
//! vendor pause, first sight) carries the seat's jitter, so no two seats
//! and no two attempts share a fixed beat.
//!
//! Each seat keeps when its current run of failed polls began (an error or a
//! 429, counted from the first; cleared by an accepted poll; a missing token
//! or a superseded poll changes nothing), which is what a "poll failing for
//! N minutes" check reads.
//!
//! The clock is wall time and may step backward. A last attempt or failure
//! run start later than `now` is pulled back to `now`, a reading stamped
//! later than `now` counts from `now` (from the first sweep that saw the
//! reading), and a first-sight due time or pause end further
//! out than its longest span from `now` is pulled in to that span, so a
//! step back delays a seat or vendor by at most its own longest wait, never
//! until the old epoch comes round again. A deadline that saturates at the
//! clock ceiling is never due.

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
/// A deadline that saturated at the clock ceiling: never due, since it
/// cannot be told apart from a real deadline that has passed.
const UNREACHABLE_MS: i64 = i64::MAX;

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

    /// Whether the outcome counts as the endpoint failing for this seat: every
    /// error, and a 429 that paused the vendor. A missing token and a
    /// re-login race say nothing about the endpoint.
    const fn is_failure(self) -> bool {
        self.is_error() || matches!(self, Self::Paused(_))
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
    /// When the current run of failed polls began; `None` while the last
    /// poll that said anything about the endpoint succeeded.
    failure_since_ms: Option<i64>,
    /// The end of the last vendor pause this seat was held by.
    pause_end_ms: Option<i64>,
    /// A last-accepted reading stamped ahead of the clock, and the sweep
    /// time it is counted from instead.
    future_reading: Option<(i64, i64)>,
}

impl SeatState {
    fn fresh(now_ms: i64, jitter: &mut dyn JitterSource) -> Self {
        Self {
            first_due_ms: now_ms.saturating_add(draw_ms_capped(jitter, BOOT_SPREAD_MS)),
            jitter_ms: draw_ms_capped(jitter, MAX_JITTER_MS),
            next_due_ms: now_ms,
            last_attempt_ms: None,
            last_outcome: None,
            error_streak: 0,
            failure_since_ms: None,
            pause_end_ms: None,
            future_reading: None,
        }
    }

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

    /// When the current run of failed polls began, in epoch ms.
    pub(super) const fn failure_since_ms(&self) -> Option<i64> {
        self.failure_since_ms
    }

    #[cfg(test)]
    pub(super) const fn error_streak(&self) -> u32 {
        self.error_streak
    }

    fn cadence_due(&self, last_accepted_ms: Option<i64>) -> i64 {
        match last_accepted_ms.max(self.last_attempt_ms) {
            None => self.first_due_ms,
            Some(base) => base
                .saturating_add(POLL_INTERVAL_MS)
                .saturating_add(self.jitter_ms),
        }
    }

    fn due_at(&self, cadence_due: i64) -> i64 {
        let backoff_due = self.last_attempt_ms.map_or(i64::MIN, |attempt| {
            attempt
                .saturating_add(backoff_ms(self.error_streak))
                .saturating_add(self.jitter_ms)
        });
        let pause_due = self
            .pause_end_ms
            .map_or(i64::MIN, |end| end.saturating_add(self.jitter_ms));
        cadence_due.max(backoff_due).max(pause_due)
    }

    /// Hold the seat until `pause_until` plus its jitter while the vendor
    /// is paused, and forget a pause end it has already waited out.
    fn track_pause(&mut self, now_ms: i64, pause_until: Option<i64>) {
        if pause_until.is_some() {
            self.pause_end_ms = pause_until;
        } else if self
            .pause_end_ms
            .is_some_and(|end| now_ms >= end.saturating_add(self.jitter_ms))
        {
            self.pause_end_ms = None;
        }
    }

    /// Re-anchor every stored time later than `now_ms` to `now_ms`.
    fn rebase_after_clock_step(&mut self, now_ms: i64) {
        let boot_bound = now_ms.saturating_add(draw_bound(BOOT_SPREAD_MS));
        self.first_due_ms = self.first_due_ms.min(boot_bound);
        self.last_attempt_ms = self.last_attempt_ms.map(|attempt| attempt.min(now_ms));
        self.failure_since_ms = self.failure_since_ms.map(|since| since.min(now_ms));
        let pause_bound = now_ms.saturating_add(draw_bound(MAX_VENDOR_PAUSE_MS));
        self.pause_end_ms = self.pause_end_ms.map(|end| end.min(pause_bound));
    }

    /// The last-accepted reading time to count from: a reading stamped
    /// ahead of the clock counts from the sweep that first saw it.
    fn reading_base(&mut self, now_ms: i64, last_accepted_ms: Option<i64>) -> Option<i64> {
        let reading = last_accepted_ms?;
        if reading <= now_ms {
            self.future_reading = None;
            return Some(reading);
        }
        match self.future_reading {
            Some((seen, anchor)) if seen == reading => Some(anchor.min(now_ms)),
            _ => {
                self.future_reading = Some((reading, now_ms));
                Some(now_ms)
            }
        }
    }
}

fn draw_bound(max_ms: u64) -> i64 {
    i64::try_from(max_ms).unwrap_or(i64::MAX)
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

    fn rebase_after_clock_step(&mut self, now_ms: i64) {
        let bound = now_ms.saturating_add(draw_bound(MAX_VENDOR_PAUSE_MS));
        for vendor in [UsageVendor::Anthropic, UsageVendor::Codex] {
            let slot = self.slot(vendor);
            *slot = slot.map(|until| until.min(bound));
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
    /// A paused vendor's seats come due at the pause end plus each seat's
    /// own jitter.
    pub(super) fn sweep(&mut self, now_ms: i64, seats: &[SeatCandidate<'_>]) -> Vec<usize> {
        self.seats
            .retain(|account, _| seats.iter().any(|seat| seat.account == account));
        self.pauses.rebase_after_clock_step(now_ms);
        let mut picks = PerVendorPick::default();
        for (index, seat) in seats.iter().enumerate() {
            let pause_until = self.pauses.until(seat.vendor);
            let rank = self.refresh_due(now_ms, seat, pause_until);
            if rank.due_at != UNREACHABLE_MS && now_ms >= rank.due_at {
                picks.offer(seat.vendor, rank, index);
            }
        }
        self.pauses.expire(now_ms);
        picks.into_sorted_indices()
    }

    fn refresh_due(
        &mut self,
        now_ms: i64,
        seat: &SeatCandidate<'_>,
        pause_until: Option<i64>,
    ) -> DueRank {
        let jitter = &mut self.jitter;
        let state = self
            .seats
            .entry(seat.account.to_string())
            .or_insert_with(|| SeatState::fresh(now_ms, jitter));
        state.track_pause(now_ms, pause_until);
        state.rebase_after_clock_step(now_ms);
        let reading = state.reading_base(now_ms, seat.last_accepted_ms);
        let cadence_due = state.cadence_due(reading);
        state.next_due_ms = state.due_at(cadence_due);
        DueRank {
            due_at: state.next_due_ms,
            cadence_due,
        }
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
            state.failure_since_ms = None;
        } else if outcome.is_failure() {
            state.failure_since_ms.get_or_insert(now_ms);
            if outcome.is_error() {
                state.error_streak = state.error_streak.saturating_add(1);
            }
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

/// How a due seat ranks for its vendor's one pick: earliest due first,
/// and among seats a shared floor (a vendor pause) made equally due, the
/// one whose own cadence came due first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DueRank {
    due_at: i64,
    cadence_due: i64,
}

/// The earliest-due seat per vendor seen so far in a sweep.
#[derive(Default)]
struct PerVendorPick {
    anthropic: Option<(DueRank, usize)>,
    codex: Option<(DueRank, usize)>,
}

impl PerVendorPick {
    fn offer(&mut self, vendor: UsageVendor, rank: DueRank, index: usize) {
        let slot = match vendor {
            UsageVendor::Anthropic => &mut self.anthropic,
            UsageVendor::Codex => &mut self.codex,
        };
        if slot.is_none_or(|(kept, _)| rank < kept) {
            *slot = Some((rank, index));
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
