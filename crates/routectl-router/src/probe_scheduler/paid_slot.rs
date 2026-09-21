//! The RAII slot a paid probe holds while it is authorized or in flight.
//!
//! # Why the paid class shares the FREE concurrency ceiling
//!
//! `PROBE_MAX_CONCURRENCY` bounds how much upstream concurrency
//! background validation may take at once, and a paid call is background
//! validation that also costs money. A separate paid counter would let the real
//! simultaneous load reach the sum of two ceilings while every counter still
//! read inside its own bound -- so the paid slot is counted by the SAME
//! `in_flight` reading that [`super::ProbeScheduler::lease_due`] refuses at and
//! that the snapshot reports, and a held paid slot displaces a free lease
//! exactly as another free lease would.
//!
//! # Why it holds an `Arc` rather than a borrow
//!
//! A free lease lives inside one worker call and can borrow the scheduler. A
//! paid slot is acquired BEFORE the reservation await and then travels inside
//! the authorization, outliving the function that took it, so it owns a
//! refcount instead. That is also what makes the release unavoidable on every
//! exit: drop, cancellation, or a later completion all run `Drop`.
//!
//! Releasing the slot is NOT a refund. A committed accounting unit is spent
//! whatever happens to the slot -- the slot bounds LOAD, the ledger bounds
//! SPEND, and conflating them is how a cancelled call would hand a day's budget
//! back (see `crate::router::paid_probe_ledger`).

use std::sync::Arc;

use super::ProbeScheduler;

/// One held paid-probe concurrency slot.
///
/// Constructible only by [`ProbeScheduler::try_acquire_paid_slot`], so a slot
/// cannot exist without the ceiling check that admitted it. Deliberately NOT
/// `Clone`: a cloned slot would release twice and let the next acquisition see
/// capacity that is still held.
pub struct PaidProbeSlot {
    scheduler: Arc<ProbeScheduler>,
}

impl PaidProbeSlot {
    /// Wrap an already-counted slot. Private to the module pair: the count is
    /// incremented inside the scheduler's own critical section, so minting a
    /// guard anywhere else would release a slot nobody took.
    pub(super) const fn new(scheduler: Arc<ProbeScheduler>) -> Self {
        Self { scheduler }
    }
}

/// HAND-WRITTEN, and it prints nothing about the scheduler.
///
/// A derive would recurse into `Arc<ProbeScheduler>` and render the WHOLE job
/// table: every other lane's `FieldVerdictKey`, every queued job's
/// `ProbePayload` with its captured beta tokens, and each row's phase. A slot is
/// held on a money-spending path, so it is exactly the value a diagnostic is
/// most likely to print -- one `{:?}` would put every concurrent lane's captured
/// request context into a log line that never asked for it.
///
/// What a reader needs from a slot is that one is held, which the type name
/// already says. There is no per-slot state worth printing: the scheduler-side
/// count is on the snapshot, which is the surface built for reporting it.
impl std::fmt::Debug for PaidProbeSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Named, field-less. Deliberately does NOT reach through `self.scheduler`
        // for a count either: that would take the scheduler lock from inside a
        // `Debug` impl, which a formatter may be called under.
        f.write_str("PaidProbeSlot(held)")
    }
}

impl Drop for PaidProbeSlot {
    fn drop(&mut self) {
        self.scheduler.release_paid_slot();
    }
}
