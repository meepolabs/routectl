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
        let seats = self.seats_on_account(account);
        if seats.is_empty() {
            tracing::warn!(
                event = "seat_unusable",
                account = %sanitize_for_log(account),
                cause = cause.as_str(),
                "seat credential unusable; its models refuse until it recovers",
            );
        }
        for seat in seats {
            tracing::warn!(
                event = "seat_unusable",
                pool = %sanitize_for_log(&seat.pool),
                member = %sanitize_for_log(&seat.member),
                account = %sanitize_for_log(account),
                cause = cause.as_str(),
                eligible = seat.eligible,
                configured = seat.configured,
                "seat credential unusable; its models refuse until it recovers",
            );
        }
    }

    /// Log that `account`'s unusable mark for `cause` was cleared.
    pub(super) fn log_seat_recovered(&self, account: &str, cause: UnusableCause) {
        let seats = self.seats_on_account(account);
        if seats.is_empty() {
            tracing::info!(
                event = "seat_recovered",
                account = %sanitize_for_log(account),
                cause = cause.as_str(),
                "seat credential recovered",
            );
        }
        for seat in seats {
            tracing::info!(
                event = "seat_recovered",
                pool = %sanitize_for_log(&seat.pool),
                member = %sanitize_for_log(&seat.member),
                account = %sanitize_for_log(account),
                cause = cause.as_str(),
                eligible = seat.eligible,
                configured = seat.configured,
                "seat credential recovered",
            );
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

#[cfg(test)]
#[path = "seat_transitions_tests.rs"]
mod seat_transitions_tests;
