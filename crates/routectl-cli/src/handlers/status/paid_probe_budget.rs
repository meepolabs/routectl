//! The paid-probe accounting HEALTH read: one row per configured provider,
//! folding the operator's cap together with what the ledger says was spent, the
//! writer's own health, and the spent-without-authorization divergence.
//!
//! # Why four facts, and why none of them substitutes for another
//!
//! An operator asking "can this lane still probe, and is its budget
//! trustworthy" needs all four, and each answers something the others cannot:
//!
//! - The CAP is the operator's own configured ceiling. Zero -- the default --
//!   means the lane never makes a paid call at all, which is a different
//!   situation from a lane that has spent its budget.
//! - The USED count is what the ledger recorded. Read from the ledger rather
//!   than from a process counter, because the budget is durable and survives
//!   restart: a process-local tally reports zero on a daemon that has been up
//!   five minutes and spent its whole day's cap before that.
//! - The WRITER's health says whether the accounting can be trusted going
//!   forward. A degraded writer cannot promise a durable unit, so the
//!   reservation fails closed -- the lane stops probing, and the used count
//!   stops moving for a reason that has nothing to do with the cap.
//! - The UNAUTHORIZED count is budget consumed for no call: units that
//!   committed while shutdown had begun, whose caller was never authorized.
//!
//! # Why the unauthorized count is its own field
//!
//! It is INVISIBLE everywhere else, and that is the whole reason this surface
//! carries it. The ledger row shows a spent unit like any other -- the unit IS
//! spent, there is no refund anywhere in the design -- so the used count above
//! cannot distinguish it. And it is deliberately NOT folded into the writer's
//! degraded state, because it is neither a storage fault (the write landed) nor
//! a healthy write (nothing was authorized). A surface reporting only the used
//! count and the writer's state therefore UNDER-REPORTS the day's true budget:
//! the units are counted as spent, but nothing says they bought nothing. This
//! field is the only place an operator can see that divergence.

use std::path::Path;
use std::sync::Arc;

use routectl_core::sanitize_for_log;
use routectl_usage::{PaidProbeUsage, UsageCounters, committed_units, open_readonly_fastfail};

/// Read-only view of the usage writer's shared health counters, for the status
/// surface.
///
/// A FACADE rather than the writer's producer handle, because `StatusState`'s whole
/// contract is that it carries no such handle: that handle can enqueue rows, and a
/// status handler holding one would be structurally capable of writing -- which is
/// why a lexical guard in this module's parent refuses its type name in every
/// status source, this one included. This type holds the shared counters `Arc` --
/// which the writer also holds -- and exposes exactly the two reads the
/// accounting-health surface needs. The `Arc` itself is private, so a panel cannot
/// reach past these methods.
pub struct UsageHealthView {
    counters: Arc<UsageCounters>,
}

impl UsageHealthView {
    /// Wrap the writer's shared counters.
    pub const fn new(counters: Arc<UsageCounters>) -> Self {
        Self { counters }
    }

    /// Whether the writer is currently unable to persist rows.
    ///
    /// Derived from the write-error count rather than from the writer's own
    /// degraded flag, because that flag lives on the writer's thread-local state
    /// and nothing outside that thread can read it. The two agree on the question
    /// that matters here: a writer that has failed a write is a writer whose
    /// durability cannot be promised.
    ///
    /// A LATCHING reading, deliberately: the counter never falls, so a writer that
    /// recovered still reports degraded for the rest of the process. That is the
    /// conservative direction for a surface about SPEND -- it over-reports a
    /// resolved problem rather than under-reporting a live one -- and the
    /// operator's recovery signal is the write-error count no longer climbing.
    fn writer_degraded(&self) -> bool {
        self.counters.write_errors() > 0
    }

    /// Paid-probe units that committed whose caller was never authorized.
    fn consumed_unauthorized(&self) -> u64 {
        self.counters.paid_probe_consumed_unauthorized()
    }
}

/// How the accounting for one provider-day reads.
///
/// A closed token set rather than a boolean, because the three answers send an
/// operator to three different places and collapsing any pair hides one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountingHealth {
    /// The stored count was read and is a canonical value.
    Healthy,
    /// A row exists and its value is not a canonical count, so the day's true
    /// spend is unknown and every paid call for this provider is failing
    /// closed. Corrupt accounting to repair.
    Malformed,
    /// The ledger could not be read at all. The stored state may be perfectly
    /// fine; the access is the problem, so an operator hunting corruption would
    /// be looking in the wrong place.
    Unreadable,
}

impl AccountingHealth {
    /// Closed-set token for the status surface.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Malformed => "malformed",
            Self::Unreadable => "unreadable",
        }
    }
}

/// One provider's paid-probe budget state, as the status/doctor surface reports
/// it.
///
/// Every field is a count, a boolean, or a closed token. The provider NAME is an
/// operator-authored config key, carried because the cap is keyed on it and a
/// row naming no provider is unattributable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PaidProbeBudget {
    /// The configured provider this budget belongs to, SANITIZED.
    ///
    /// An operator-authored config key, so it carries no request content -- but it
    /// is still a caller-supplied string of unbounded length that lands on a log
    /// line, and a control byte there is how a forged second line is injected. A
    /// `[fidelity.paid_probe_daily_caps]` key is validated for MEMBERSHIP at config
    /// load (it must name a configured provider), not for shape, and a provider
    /// name is itself only bounded by what the operator typed.
    pub provider: String,
    /// The operator's configured UTC-day cap. Zero means no paid call is ever
    /// made for this provider -- the default, and a different state from an
    /// exhausted budget.
    pub daily_cap: u32,
    /// Units the ledger records as committed for the current UTC day. `None`
    /// when the accounting could not be read as a count, in which case
    /// `accounting` says which way it failed.
    pub committed_today: Option<u32>,
    /// How the accounting read went.
    pub accounting: AccountingHealth,
}

/// The PROCESS-GLOBAL half of the accounting health, emitted once.
///
/// # Why these are not fields on the per-provider row
///
/// Because they are not per-provider facts, and copying them onto every row makes
/// the surface lie about their scope. The writer is one actor for the whole
/// process, and the unauthorized-spend counter is one atomic with no provider
/// dimension at all -- the reservation that bumps it records only that A unit
/// committed unauthorized, never which provider's. A row carrying
/// `consumed_unauthorized: 2` on each of three providers reads as six units to
/// anyone who sums the column, and reads as "two units of THIS provider's budget"
/// to anyone who does not. Both readings are wrong, and neither is detectable from
/// the line.
///
/// So the scope is expressed in the shape: one value, once, named for what it
/// describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountingGlobals {
    /// Whether the usage writer is currently unable to persist rows.
    ///
    /// The OTHER reason every provider's paid probes are refused, independent of
    /// any cap: a degraded writer cannot promise a durable unit, so the
    /// reservation fails closed for all of them.
    ///
    /// A LATCHING reading, deliberately: it is derived from a monotonic
    /// write-error count, so a writer that recovered still reports degraded for the
    /// rest of the process. That is the conservative direction for a surface about
    /// spend -- it over-reports a resolved problem rather than under-reporting a
    /// live one -- and the operator's recovery signal is the write-error count no
    /// longer climbing.
    pub writer_degraded: bool,
    /// Paid-probe units that COMMITTED but whose caller was never authorized,
    /// process-wide.
    ///
    /// Budget consumed for no call, and THE only place an operator can see that
    /// divergence. The per-provider committed count cannot show it: the unit IS
    /// spent, there is no refund anywhere in the design, and its ledger row looks
    /// like any other. `writer_degraded` cannot show it either: the write landed,
    /// so it is neither a storage fault nor a healthy write. A surface reporting
    /// only those two under-reports the day's real budget.
    ///
    /// PROCESS-LIFETIME, so it RESETS ON RESTART. Nothing persists it -- the
    /// counter is an in-memory atomic, and the condition it counts (a commit
    /// racing teardown) is by definition a shutdown-time event, so a restarted
    /// daemon legitimately reports zero while the units it counted stay spent in
    /// the ledger. An operator reconciling a day's budget across a restart must
    /// read this from the logs of the process that emitted it, not from the live
    /// one.
    pub consumed_unauthorized_total: u64,
}

/// The process-global accounting facts, read once.
pub(super) fn accounting_globals(usage: &UsageHealthView) -> AccountingGlobals {
    AccountingGlobals {
        writer_degraded: usage.writer_degraded(),
        consumed_unauthorized_total: usage.consumed_unauthorized(),
    }
}

/// One budget row per provider named in `[fidelity] paid_probe_daily_caps`.
///
/// Keyed on the CONFIGURED caps rather than on the providers table: a provider
/// with no cap entry has a cap of zero and makes no paid call, so it has no
/// budget to report and a row for it would be noise on every deployment. The
/// caps map is also exactly what the reservation gates on, so the rows an
/// operator sees are the lanes that can actually spend.
///
/// The ledger is opened read-only ONCE for the whole set rather than per row: a
/// per-row open would be one connection per provider on every status poll, and
/// the rows would each read a different instant.
pub(super) fn paid_probe_budgets(
    caps: &std::collections::BTreeMap<String, u32>,
    db_path: &Path,
    now_epoch_ms: i64,
) -> Vec<PaidProbeBudget> {
    if caps.is_empty() {
        return Vec::new();
    }

    // An open failure is a property of the LEDGER, so it lands on every row
    // rather than being reported once: an operator reads one provider's row, and
    // a row silently omitting its accounting state would read as healthy.
    // FAST-FAIL open, and the choice is load-bearing on this surface. A status
    // panel runs under a bounded-concurrency gate with a small permit pool, and
    // the ordinary read-only open waits out a busy timeout -- so a busy or
    // WAL-checkpointing ledger would hold a panel permit for that whole wait,
    // every poll, and a few concurrent polls can then wedge the whole status
    // subtree. Failing fast degrades ONE field to `unreadable` instead, which the
    // row already has a token for.
    let db = open_readonly_fastfail(db_path).ok();
    caps.iter()
        .map(|(provider, &daily_cap)| {
            let usage_read = db.as_ref().map_or(PaidProbeUsage::Unreadable, |db| {
                committed_units(db.conn(), provider, now_epoch_ms)
            });
            PaidProbeBudget {
                provider: sanitize_for_log(provider),
                daily_cap,
                // The rest arm is REQUIRED by the read's `#[non_exhaustive]`
                // vocabulary, and it fails in the safe direction: a reading this
                // build does not know is NOT a count, so it reports no count rather
                // than a zero that would claim nothing was spent.
                committed_today: match usage_read {
                    PaidProbeUsage::Committed(units) => Some(units),
                    _ => None,
                },
                // Same arm, same direction: an unrecognized reading is reported as
                // unreadable rather than healthy. A future variant that fell
                // through to `healthy` would show a clean budget for accounting
                // this build cannot interpret.
                accounting: match usage_read {
                    PaidProbeUsage::Committed(_) => AccountingHealth::Healthy,
                    PaidProbeUsage::Malformed => AccountingHealth::Malformed,
                    _ => AccountingHealth::Unreadable,
                },
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "paid_probe_budget_tests.rs"]
mod tests;
