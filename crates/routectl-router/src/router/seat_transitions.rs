//! The operator's two seat-transition lines: one WARN `seat_unusable` when a
//! credential account becomes unusable (or its cause changes), one INFO
//! `seat_recovered` when a reset or a successful refresh clears the mark.
//! Logged at the mark / reset API only, never per request, so a seat that
//! refuses a thousand requests still logs once.
//!
//! Each line names the pool and member the account serves with the pool's
//! live seat counts read after the change; an account no pool holds (a
//! direct model's seat) logs once with its account and cause alone.

use std::time::Instant;

use routectl_core::sanitize_for_log;

use super::Router;
use super::status::PoolEligibility;
use crate::runtime_state::{AccountReset, UnusableCause};

/// One pool seat on a credential account, with its pool's live counts.
struct SeatOnAccount {
    pool: String,
    member: String,
    eligible: usize,
    configured: usize,
}

impl Router {
    /// Every pool seat whose credential account is `account`.
    fn seats_on_account(&self, account: &str) -> Vec<SeatOnAccount> {
        let counts = self.pool_eligibility(Instant::now());
        let counts_for = |pool: &str| counts.iter().find(|c| c.pool == pool);
        let mut out = Vec::new();
        for report in &self.pool_reports {
            let Some(seats) = report
                .models
                .first()
                .and_then(|nickname| self.resolved_models.get(nickname))
                .and_then(|model| model.seats.as_ref())
            else {
                continue;
            };
            let (eligible, configured) = counts_for(&report.pool)
                .map_or((0, seats.len()), |c: &PoolEligibility| {
                    (c.eligible, c.configured)
                });
            out.extend(
                seats
                    .iter()
                    .filter(|seat| seat.account() == Some(account))
                    .map(|seat| SeatOnAccount {
                        pool: report.pool.clone(),
                        member: seat.provider_name.clone(),
                        eligible,
                        configured,
                    }),
            );
        }
        out
    }

    /// Log that `account` became unusable for `cause`.
    pub(super) fn log_seat_unusable(&self, account: &str, cause: UnusableCause) {
        self.log_seat_transition(SeatTransition::Unusable, account, cause);
    }

    /// Log that `account`'s unusable mark for `cause` was cleared.
    pub(super) fn log_seat_recovered(&self, account: &str, cause: UnusableCause) {
        self.log_seat_transition(SeatTransition::Recovered, account, cause);
    }

    /// One `kind` line per pool seat on `account`, or one line naming the
    /// account alone when no pool holds it.
    fn log_seat_transition(&self, kind: SeatTransition, account: &str, cause: UnusableCause) {
        let seats = self.seats_on_account(account);
        if seats.is_empty() {
            emit_seat_transition(kind, account, cause, None);
        }
        for seat in &seats {
            emit_seat_transition(kind, account, cause, Some(seat));
        }
    }

    /// Log a `seat_recovered` line for every reset that cleared a mark.
    pub(super) fn log_recovered_resets(&self, resets: &[AccountReset]) {
        for reset in resets {
            if let Some(cause) = reset.outcome.unusable_cleared {
                self.log_seat_recovered(&reset.account, cause);
            }
        }
    }
}

/// Which seat-transition line to emit: `Unusable` logs at WARN,
/// `Recovered` at INFO, each with its own `event` token and message.
#[derive(Clone, Copy)]
enum SeatTransition {
    Unusable,
    Recovered,
}

impl SeatTransition {
    const fn event(self) -> &'static str {
        match self {
            Self::Unusable => "seat_unusable",
            Self::Recovered => "seat_recovered",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::Unusable => "seat credential unusable; its models refuse until it recovers",
            Self::Recovered => "seat credential recovered",
        }
    }
}

/// Emit one `kind` line for `account`, with the pool seat's name and live
/// counts when `seat` is given.
fn emit_seat_transition(
    kind: SeatTransition,
    account: &str,
    cause: UnusableCause,
    seat: Option<&SeatOnAccount>,
) {
    let account = sanitize_for_log(account);
    let event = kind.event();
    let message = kind.message();
    let cause = cause.as_str();
    // `tracing` fixes an event's level per callsite, so each level is its own
    // macro invocation over the same field list.
    macro_rules! emit {
        ($level:expr) => {
            match seat {
                None => tracing::event!(
                    $level,
                    event,
                    account = %account,
                    cause,
                    "{message}",
                ),
                Some(seat) => tracing::event!(
                    $level,
                    event,
                    pool = %sanitize_for_log(&seat.pool),
                    member = %sanitize_for_log(&seat.member),
                    account = %account,
                    cause,
                    eligible = seat.eligible,
                    configured = seat.configured,
                    "{message}",
                ),
            }
        };
    }
    match kind {
        SeatTransition::Unusable => emit!(tracing::Level::WARN),
        SeatTransition::Recovered => emit!(tracing::Level::INFO),
    }
}

#[cfg(test)]
#[path = "seat_transitions_tests.rs"]
mod seat_transitions_tests;
