//! Publication and shutdown: the short serialized transitions that move the
//! probe lifecycle forward, and the predicate everything else reads it by.
//!
//! Split from `probe_lifecycle` for file size; the two halves are one concern
//! seen from opposite ends. That module owns the per-incarnation WORK (what is
//! queued, run, and settled); this one owns the GENERATION that work is valid
//! under, and the terminal state that ends it.
//!
//! A publication draws a ticket value, stamps it, retires superseded work,
//! and (through the callback form) stores the replacement
//! -- all synchronously, all under the shared lifecycle transition, and all
//! bounded by in-memory work.

use std::sync::Arc;

use super::Router;

impl Router {
    /// Advance the scheduler incarnation and retire every job from the
    /// previous one, returning how many were cancelled.
    ///
    /// Called at the PUBLICATION COMMIT POINT -- immediately BEFORE the
    /// `ArcSwap` store, once every failure and abandonment path has already
    /// returned. Ordering matters in both directions: doing it at carry-over
    /// time would strand a previous router that an abandoned reload leaves
    /// live, while doing it AFTER the store leaves a window in which the
    /// published router still carries the outgoing incarnation, so a request
    /// landing there queues work this call immediately retires.
    ///
    /// Also clears terminal tombstones and any tombstone saturation: a new
    /// incarnation must be free to ask questions the previous one settled.
    ///
    /// SYNCHRONOUS: it awaits nothing.
    ///
    /// SERIALIZED against the shutdown and against other publications by the
    /// shared lifecycle state, which is held for this whole short transition. The
    /// ticket alone orders the GENERATION but says nothing about whether the rest
    /// of a transition has run, so two of them could otherwise interleave their
    /// stamp, retire, and clear steps.
    ///
    /// A NO-OP after shutdown, returning zero. Past that point the daemon is going
    /// away: re-arming a scheduler the shutdown just cleared would leave work
    /// nothing will run, and drawing a ticket value would move the terminal
    /// generation that every liveness read depends on.
    pub fn publish_probe_incarnation(&self) -> usize {
        let Some(live) = self.probe_lifecycle_state.begin_publication() else {
            return 0;
        };
        self.publish_within(&live)
    }

    /// The publication transition proper, performed under a LIVE lifecycle token.
    ///
    /// STRUCTURALLY UNREACHABLE without liveness: the only producer of a
    /// `LiveLifecycleTransition` is `begin_publication`, which performs the
    /// terminal check inside its own acquisition. So "checked terminal" and
    /// "published" cannot be two different acquisitions -- a caller that dropped
    /// the token and re-acquired would have to call `begin_publication` again and
    /// take a fresh check. That is why the guard is a parameter rather than
    /// something this function re-derives.
    fn publish_within(
        &self,
        _live: &super::probe_lifecycle_state::LiveLifecycleTransition<'_>,
    ) -> usize {
        // Draw from the SHARED ticket so the value is strictly greater than
        // every previously published one, then stamp it on THIS Router
        // only -- the outgoing Router keeps its own, so its activations and
        // settlements are refused as stale from this moment on.
        let next = self
            .probe_incarnation_ticket
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        self.probe_incarnation
            .store(next, std::sync::atomic::Ordering::Release);
        // Retire everything below the new incarnation. Outstanding leases
        // settle as stale: they release nothing they no longer hold and
        // schedule no follow-up work.
        self.probe_scheduler.retire_before(next)
    }

    /// Cancel every queued probe job for shutdown, returning how many were
    /// cancelled. Outstanding leases settle as stale and schedule nothing.
    ///
    /// ADVANCES THE SHARED TICKET FIRST, to a generation NO router is stamped
    /// with, and that ordering is the whole of the shutdown protocol. The
    /// terminal generation is UNSTAMPED, so nothing can become current again: a
    /// shutdown that merely advanced the ticket to a value some router then
    /// stamped would be an ABA that reopened the resurrection window.
    ///
    /// Synchronous, like `publish_probe_incarnation`.
    pub fn shutdown_probe_work(&self) -> usize {
        // Sets the terminal bit inside its own acquisition, so a publication that
        // has not yet started reads terminal and does nothing at all rather than
        // racing the clear below. `None` only on re-entry from this thread (a
        // store callback calling back in), where doing nothing is correct.
        let Some(_shutting_down) = self.probe_lifecycle_state.begin_shutdown() else {
            return 0;
        };
        // Past every stamped incarnation, and never stamped on any router. The
        // ticket is the shared authority, so one `fetch_add` retires every
        // generation at once -- there is no per-router state to sweep.
        let terminal = self
            .probe_incarnation_ticket
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        // ATOMIC WITH THE CLEAR, and the terminal generation is what it retires
        // to. Clearing alone is not enough: no router is stamped with `terminal`,
        // so admitted traffic racing this shutdown still carries the OLD generation
        // -- and an activation at that generation would be admitted by a scheduler
        // whose retirement floor had not moved, re-arming work after the sweep.
        // Raising the floor to `terminal` inside the same critical section refuses
        // every such activation instead.
        self.probe_scheduler.cancel_all_and_retire_to(terminal)
    }

    /// Stamp this Router's incarnation and hand it to `store` BEFORE returning.
    ///
    /// THE production publication entry point. The stamp-then-store ordering is
    /// the reason it exists as one call rather than two: between a store and a
    /// later stamp the published router still carries the OUTGOING incarnation, so
    /// a request landing in that window queues work the stamp immediately retires,
    /// leaving the lane un-probed with nothing recording why. Passing the store as
    /// a CALLBACK puts that ordering inside this function, where it is a property
    /// of the protocol rather than a convention two call sites have to remember.
    ///
    /// A CLOSURE rather than a publication-handle type, and rather than taking the
    /// container itself: this crate must not depend on the caller's swap primitive
    /// (an `arc_swap` dependency here would invert the layering), and a handle the
    /// caller holds across its own store is a token whose correct USE is again a
    /// convention. The closure cannot be forgotten -- it is invoked here or the
    /// function does not complete.
    ///
    /// Takes `&Arc<Self>` because the callback receives an owned `Arc<Self>` to
    /// store, and clones from this one rather than requiring the caller to keep a
    /// second handle in step with the router being stamped.
    ///
    /// # The callback contract
    ///
    /// `store` runs while the short lifecycle transition is HELD, which places
    /// three requirements on it. They are requirements on the caller because this
    /// crate cannot enforce the first two:
    ///
    /// 1. ONE STORE, and nothing else. A pointer write into whatever container the
    ///    caller publishes through.
    /// 2. NON-BLOCKING. It must not wait on I/O, a channel, another lock, or
    ///    anything a different thread must do first -- that would hold the
    ///    lifecycle transition for as long as the wait, which is exactly the
    ///    availability property this design exists to preserve.
    /// 3. NO LIFECYCLE RE-ENTRY. It must not call any publication or shutdown
    ///    entry point. Re-entry is REFUSED rather than trusted: a thread-local
    ///    marker is checked before the mutex is acquired, so a nested call returns
    ///    a no-op immediately instead of deadlocking on a non-reentrant lock. The
    ///    marker is cleared on drop, so a panicking callback does not poison later
    ///    publications on this thread -- though a callback SHOULD NOT panic, since
    ///    unwinding out of it abandons the publication mid-transition.
    ///
    /// THE STORE IS INSIDE THE LIFECYCLE TRANSITION, which is the property the
    /// callback form exists for beyond ordering. A publication whose commit point
    /// is reached while a shutdown is running would otherwise install a
    /// replacement router into a daemon that is going away -- and because the
    /// store is the caller's, nothing downstream could undo it.
    ///
    /// After a shutdown this performs NOTHING: no ticket draw, no stamp, and the
    /// callback is NOT invoked, so no store happens. The caller's `Arc` is simply
    /// dropped. Returns zero, like the plain form.
    pub fn publish_probe_incarnation_into(
        self: &Arc<Self>,
        store: impl FnOnce(Arc<Self>),
    ) -> usize {
        let Some(live) = self.probe_lifecycle_state.begin_publication() else {
            // Deliberately does not call `store`. A publication that stored here
            // would hand the daemon a router to serve from after it had already
            // stopped, and re-arm state the shutdown just cleared. Also the
            // re-entry path: a store callback that called back in gets a no-op
            // rather than a deadlock.
            return 0;
        };
        let retired = self.publish_within(&live);
        // INSIDE the call, after the stamp, before the return, and still under the
        // lifecycle transition. A caller cannot reorder these two or drop the
        // store, and a shutdown cannot interleave between them.
        store(Arc::clone(self));
        retired
    }
}
