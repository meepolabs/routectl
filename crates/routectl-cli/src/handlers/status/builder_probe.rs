//! Observation point for the blocking-builder concurrency invariant.
//!
//! `STATUS_MAX_INFLIGHT` caps admitted `/status*` requests, and that cap is
//! only the cap on concurrent blocking panel builders because no handler fans
//! one admitted request into several builders. Nothing about that property is
//! visible from the wire: a fan-out would still answer 200 on every panel. So
//! the invariant needs a seam inside [`super::guard_panel`], the one chokepoint
//! every panel builder passes through, where a test can count builders that
//! have actually STARTED and hold them there.
//!
//! The cancellation half of the invariant needs one more observation point.
//! Whether a detached builder still holds capacity is invisible from the wire
//! AND from a builder count alone: the assertion is about a builder that did
//! NOT run, and "it has not run yet" is never distinguishable from "it is about
//! to" by waiting. [`capacity_exhausted`] turns that negative into a positive.
//! It fires only when a builder actually found the capacity pool empty and had
//! to wait, which -- while every permit is held by a parked builder no test has
//! released -- is a state nothing can leave without the test acting. So a test
//! can wait for it, snapshot, and assert, with the timeout serving only as a
//! hang bound.
//!
//! Outside a test build the seam compiles to nothing: [`current`] yields a
//! zero-sized token and the hooks have empty bodies.

#[cfg(not(test))]
pub use inert::{capacity_exhausted, current, park, submitted};

#[cfg(test)]
pub use active::{BUILDER_PROBE, BuilderProbe, capacity_exhausted, current, park, submitted};

#[cfg(not(test))]
mod inert {
    /// Zero-sized stand-in for the test probe handle.
    #[derive(Clone, Copy)]
    pub struct Probe;

    pub const fn current() -> Probe {
        Probe
    }

    pub const fn capacity_exhausted(_probe: &Probe) {}

    pub const fn submitted(_probe: &Probe) {}

    pub const fn park(_probe: &Probe) {}
}

#[cfg(test)]
mod active {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::{Condvar, Mutex};

    tokio::task_local! {
        /// Installed per REQUEST TASK, not globally: only builders reached
        /// from a future the test wrapped in `BUILDER_PROBE.scope(..)` are
        /// observed, so a probe test never parks a builder belonging to some
        /// other test running concurrently in the same binary.
        pub static BUILDER_PROBE: Arc<BuilderProbe>;
    }

    /// Counts blocking builders that reached their `spawn_blocking` worker and
    /// parks each one there until [`BuilderProbe::release`]. Same idiom as the
    /// gate tests' `HoldState` (arrival counter + explicit release), but the
    /// park has to be a BLOCKING wait: it happens on a blocking worker, off
    /// the runtime, where an async semaphore cannot be awaited.
    #[derive(Default)]
    pub struct BuilderProbe {
        capacity_exhausted: AtomicUsize,
        submitted: AtomicUsize,
        started: AtomicUsize,
        released: Mutex<bool>,
        wakeup: Condvar,
    }

    impl BuilderProbe {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// How many blocking builders found the capacity pool EMPTY and had to
        /// wait for a permit.
        ///
        /// This is the signal a cancellation test waits on. "No new blocking
        /// work started" is an absence, and no amount of waiting distinguishes
        /// an absence from a not-yet; this counter is the positive event that
        /// says the request reached the gate and the gate held it. While every
        /// permit is owned by a parked builder, that is a state the test alone
        /// can end.
        pub fn capacity_exhausted(&self) -> usize {
            self.capacity_exhausted.load(Ordering::SeqCst)
        }

        /// How many blocking builders have been SUBMITTED -- counted on the
        /// async side, once the capacity permit is in hand and before
        /// `spawn_blocking` hands the job to a worker.
        ///
        /// This is the number a capacity assertion must read, not [`started`]:
        /// submission is what the handler DECIDES to do, whereas starting also
        /// depends on how fast the blocking pool picks jobs up. A fan-out
        /// submits all four of a request's builders before any of them can be
        /// observed to start, so a test that waits for arrivals has to guess
        /// how long to wait for stragglers, while a test that reads submissions
        /// sees the full decision the instant the handler has made it.
        ///
        /// [`started`]: Self::started
        pub fn submitted(&self) -> usize {
            self.submitted.load(Ordering::SeqCst)
        }

        /// How many blocking builders have started. While nothing has been
        /// released yet this is also the CONCURRENT builder count.
        pub fn started(&self) -> usize {
            self.started.load(Ordering::SeqCst)
        }

        /// Let every parked builder -- and every builder that starts later --
        /// run its real work.
        pub fn release(&self) {
            *self.released.lock() = true;
            self.wakeup.notify_all();
        }

        fn exhaust(&self) {
            self.capacity_exhausted.fetch_add(1, Ordering::SeqCst);
        }

        fn submit(&self) {
            self.submitted.fetch_add(1, Ordering::SeqCst);
        }

        fn enter(&self) {
            self.started.fetch_add(1, Ordering::SeqCst);
            let mut released = self.released.lock();
            while !*released {
                self.wakeup.wait(&mut released);
            }
        }
    }

    /// Probe handle for the CURRENT request task, captured on the async side
    /// because a blocking worker does not inherit task-locals.
    pub type Probe = Option<Arc<BuilderProbe>>;

    pub fn current() -> Probe {
        BUILDER_PROBE.try_with(Arc::clone).ok()
    }

    /// Record that a builder found no free capacity permit and is about to wait
    /// for one. Called only on the contended path, so the counter is a positive
    /// observation of the gate holding a builder back.
    pub fn capacity_exhausted(probe: &Probe) {
        if let Some(probe) = probe {
            probe.exhaust();
        }
    }

    /// Record that a builder is about to be handed to `spawn_blocking`. Called
    /// on the ASYNC side so the count reflects the handler's decision rather
    /// than blocking-pool scheduling latency.
    pub fn submitted(probe: &Probe) {
        if let Some(probe) = probe {
            probe.submit();
        }
    }

    pub fn park(probe: &Probe) {
        if let Some(probe) = probe {
            probe.enter();
        }
    }
}
