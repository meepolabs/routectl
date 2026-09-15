//! The per-request reactive-repair ceiling shared by every dispatch walk.
//!
//! A repair re-dispatches one target with a corrected request after the
//! upstream rejected the original. Each repair kind is at most once per
//! target by its own gate; this budget is the SECOND, request-scoped
//! bound: it is declared above a walk's chain loop, threaded into every
//! per-target arm that can repair, and never reset by a fallback hop. Two
//! bounds are needed because they answer different questions -- the
//! per-target gate stops a repair nesting inside one target's retries,
//! while this one stops an N-target chain from paying N repairs for ONE
//! logical client request.
//!
//! The budget is drawn LAST, after a repair arm's own conditions already
//! hold, so a request that never had a repairable rejection spends
//! nothing and an exhausted budget leaves the ordinary error path
//! untouched (no breaker debit, no extra upstream call).
//!
//! All three walks thread it, but only two can currently be OBSERVED
//! spending it: the token-count walk's repair arm cannot reach today's only
//! repair class (its capable provider kinds and the classifier's
//! fixture-backed kinds are disjoint -- see `count_tokens`), so its share of
//! this ceiling is covered by threading and source guards until a reachable
//! repair kind exists.

/// Reactive repair re-dispatches one client request may spend, summed
/// across repair kinds and across every target in its fallback chain.
pub(super) const REPAIRS_PER_REQUEST: u8 = 2;

/// The remaining repair allowance for one logical client request.
///
/// Constructed once per request, above the chain loop, beside the other
/// request-scoped dispatch locals. Threaded by `&mut` into per-seat
/// helpers rather than re-created there: a fresh budget per seat is
/// exactly the per-target reset this type exists to remove.
#[derive(Debug)]
pub(super) struct RepairBudget {
    remaining: u8,
}

impl RepairBudget {
    /// The full per-request allowance.
    pub(super) const fn per_request() -> Self {
        Self {
            remaining: REPAIRS_PER_REQUEST,
        }
    }

    /// Spend one repair, or refuse when the request's allowance is gone.
    ///
    /// Call it as the LAST condition of a repair arm: it mutates, so a
    /// caller that draws before checking its own gates silently charges
    /// requests that never repair.
    pub(super) const fn draw(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

#[cfg(test)]
#[path = "repair_budget_tests.rs"]
mod repair_budget_tests;
