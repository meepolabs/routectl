//! Allocation-counting global allocator plus a deterministic measurement
//! harness shared by the perf benches.
//!
//! Each bench binary installs [`CountingAllocator`] as its
//! `#[global_allocator]` and builds a list of [`BenchCase`]s. The same
//! closures drive two paths:
//!
//! - the criterion wall-time path (`bench_function`), and
//! - the allocation-count path ([`run_alloc_count`]), taken when the
//!   `BENCH_ALLOC_COUNT=1` environment variable is set.
//!
//! A case is either *simple* (one closure that both sets up and performs
//! the measured work) or *batched* (a `setup` closure that produces an
//! input, separate from a `measured` closure that consumes it). The
//! batched shape exists so per-iteration setup cost -- e.g. cloning an
//! input the callee consumes by value -- stays OUT of both the wall-time
//! and the allocation tally: criterion drives batched cases with
//! `iter_batched` (setup untimed), and the allocation path runs `setup`
//! BEFORE [`reset`] so only `measured` is counted.
//!
//! The allocation path resets the counters, runs each case's measured
//! work ONCE single-threaded, and prints `<name> allocs=<n> bytes=<n>`.
//! Because every bench closure is fed a byte-stable fixture and no clock
//! or entropy source participates, two separate process runs print
//! identical counts -- the property the before/after baseline comparison
//! relies on.
//!
//! The counter adds a small constant per-allocation cost to the criterion
//! path too. That overhead is uniform across a before/after comparison and
//! does not affect the allocation tallies (which are exact), so it is
//! accepted rather than gated behind a separate build.

use std::alloc::{GlobalAlloc, Layout, System};
use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};

/// Number of allocations observed since the last [`reset`].
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
/// Total bytes requested across those allocations.
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

/// A [`System`]-backed global allocator that tallies every allocation and
/// reallocation into process-global atomic counters.
///
/// `Ordering::Relaxed` is sufficient: the allocation-count pass is
/// single-threaded and criterion runs its benches serially, so there is no
/// cross-thread happens-before relationship to establish -- only the tally
/// itself must be atomic. `alloc_zeroed` is intentionally not overridden;
/// its default implementation delegates to `alloc`, so zeroed allocations
/// are counted through that path.
pub struct CountingAllocator;

// SAFETY: every method forwards its arguments unchanged to the
// corresponding `System` allocator method, so the safety contract is
// exactly `System`'s. The only added behavior is incrementing atomic
// counters, which has no bearing on allocation soundness.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Zero both counters. Call immediately before running a bench closure in
/// the allocation-count pass.
pub fn reset() {
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
}

/// Current `(allocations, bytes)` tally since the last [`reset`].
pub fn snapshot() -> (u64, u64) {
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

/// Whether the allocation-count measurement mode was requested via
/// `BENCH_ALLOC_COUNT=1`.
pub fn alloc_count_mode() -> bool {
    matches!(std::env::var("BENCH_ALLOC_COUNT").as_deref(), Ok("1"))
}

/// Whether the dhat heap-profile pass was requested via `BENCH_DHAT=1`.
///
/// The dhat allocator and profiler live behind each bench crate's `dhat`
/// feature (this testkit carries no dhat dependency); this is only the
/// shared env-flag check, so all three bench binaries parse the request
/// identically -- mirroring [`alloc_count_mode`].
pub fn dhat_profile_mode() -> bool {
    matches!(std::env::var("BENCH_DHAT").as_deref(), Ok("1"))
}

/// One benchmark case: a stable name plus the closures performing a
/// single unit of the measured work. The same case feeds both the
/// criterion timing path and the allocation-count pass, so it is defined
/// once. A case is either *simple* (one closure, via [`BenchCase::new`])
/// or *batched* (untimed setup plus a measured closure, via
/// [`BenchCase::new_batched`]).
pub struct BenchCase<'a> {
    name: String,
    kind: CaseKind<'a>,
}

/// The two case shapes. Batched carries type-erased closures: the concrete
/// input type is known only at [`BenchCase::new_batched`] and monomorphized
/// there, then boxed as `dyn Any` so heterogeneous cases share one list.
enum CaseKind<'a> {
    Simple(Box<dyn Fn() + 'a>),
    Batched {
        setup: Box<dyn Fn() -> Box<dyn Any> + 'a>,
        measured: Box<dyn Fn(Box<dyn Any>) + 'a>,
    },
}

impl std::fmt::Debug for BenchCase<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BenchCase")
            .field("name", &self.name)
            .finish()
    }
}

impl<'a> BenchCase<'a> {
    /// Build a simple case from its stable bench name and its measured
    /// closure. Use this when the measured work owns everything it needs
    /// and has no per-iteration setup to keep out of the measurement.
    pub fn new(name: impl Into<String>, run: impl Fn() + 'a) -> Self {
        Self {
            name: name.into(),
            kind: CaseKind::Simple(Box::new(run)),
        }
    }

    /// Build a batched case: `setup` produces one fresh input per
    /// iteration and `measured` consumes it. Setup cost stays out of the
    /// measurement -- criterion runs `setup` between timed samples and the
    /// allocation pass runs it before [`reset`]. Use this when the callee
    /// takes its input by value (so each iteration must clone or rebuild
    /// it) and that setup cost would otherwise contaminate the tally.
    pub fn new_batched<T: 'static>(
        name: impl Into<String>,
        setup: impl Fn() -> T + 'a,
        measured: impl Fn(T) + 'a,
    ) -> Self {
        let setup = Box::new(move || -> Box<dyn Any> { Box::new(setup()) });
        let measured = Box::new(move |input: Box<dyn Any>| {
            let value = input
                .downcast::<T>()
                .expect("bench setup output type matches measured input");
            measured(*value);
        });
        Self {
            name: name.into(),
            kind: CaseKind::Batched { setup, measured },
        }
    }

    /// The stable bench name (`<stage>__<profile>__<dialect>`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this case separates untimed setup from the measured work.
    /// The criterion driver must feed batched cases through `iter_batched`
    /// (a plain `iter(|| run())` would time the setup too).
    pub const fn is_batched(&self) -> bool {
        matches!(self.kind, CaseKind::Batched { .. })
    }

    /// Produce one fresh setup value to feed [`Self::run_measured`].
    /// A simple case has no setup, so this yields a zero-sized placeholder
    /// (which does not allocate).
    pub fn setup(&self) -> Box<dyn Any> {
        match &self.kind {
            CaseKind::Simple(_) => Box::new(()),
            CaseKind::Batched { setup, .. } => setup(),
        }
    }

    /// Run the measured unit of work, consuming a value from
    /// [`Self::setup`]. For a simple case the input is the unused
    /// placeholder and the case's own closure runs.
    pub fn run_measured(&self, input: Box<dyn Any>) {
        match &self.kind {
            CaseKind::Simple(run) => run(),
            CaseKind::Batched { measured, .. } => measured(input),
        }
    }

    /// Perform one unit of the measured work with its setup inline. For a
    /// simple case this is just the closure; for a batched case it runs
    /// `setup` then `measured` together, so it is NOT suitable for timing
    /// a batched case -- criterion drives those through `iter_batched`.
    pub fn run(&self) {
        match &self.kind {
            CaseKind::Simple(run) => run(),
            CaseKind::Batched { setup, measured } => measured(setup()),
        }
    }
}

/// Run each case once single-threaded, printing `<name> allocs=<n>
/// bytes=<n>` per case. A batched case's `setup` runs BEFORE [`reset`] so
/// only the measured closure's allocations are tallied. The counters are
/// read BEFORE the print so the reporting line's own allocations never
/// contaminate a case's tally.
pub fn run_alloc_count(cases: &[BenchCase<'_>]) {
    for case in cases {
        let (allocs, bytes) = match &case.kind {
            CaseKind::Simple(run) => {
                reset();
                run();
                snapshot()
            }
            CaseKind::Batched { setup, measured } => {
                let input = setup();
                reset();
                measured(input);
                snapshot()
            }
        };
        println!("{} allocs={allocs} bytes={bytes}", case.name());
    }
}
