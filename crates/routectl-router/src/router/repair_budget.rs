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
//! All three walks thread it, and all three are covered BEHAVIORALLY -- by
//! two different fixtures, because the two repair kinds differ in which
//! walks they can reach. The envelope-field kind acts on the lane the
//! token-count walk admits, so `field_repair_tests` measures the ceiling in
//! every walk including an N-SEAT token-count test. The reasoning-replay
//! kind cannot reach that walk (its capable provider kinds and the
//! classifier's fixture-backed kinds are disjoint -- see `count_tokens`), so
//! `repair_budget_cross_walk_tests` measures it on the two messages walks
//! and pins the disjointness rather than asserting a count that would be
//! zero for an unrelated reason.

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

    /// Whether an allowance remains, WITHOUT spending it.
    ///
    /// Exists so a repair arm can order its steps as "check, mutate, then
    /// charge": a caller that must not charge for a mutation it failed to make
    /// cannot use [`Self::draw`] as its gate, because a draw it then abandons
    /// is a charge for a repair the wire never saw. Pairing this read with a
    /// later draw keeps the check cheap and the charge truthful.
    ///
    /// Read-only, so nothing about it can be forgotten or double-counted; the
    /// draw remains the only mutating operation.
    pub(super) const fn can_draw(&self) -> bool {
        self.remaining > 0
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

    /// The remaining allowance, read WITHOUT spending it.
    ///
    /// Test-only, and deliberately not a `Clone` on this type: a cloneable
    /// budget is one a caller can accidentally spend a copy of, which is the
    /// per-seat reset this type exists to remove. A test that needs to show a
    /// refusal charged nothing compares this before and after instead of
    /// draining a copy.
    #[cfg(test)]
    pub(super) const fn remaining(&self) -> u8 {
        self.remaining
    }
}

#[cfg(test)]
#[path = "repair_budget_tests.rs"]
mod repair_budget_tests;
