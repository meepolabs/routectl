//! Test-only, thread-confined allocation counter.
//!
//! Installed as the lib-test binary's `#[global_allocator]` so a test can
//! assert that a hot-path predicate allocates NOTHING. The tally lives in
//! thread-local state rather than in process-global atomics (the shape
//! `routectl_testkit::bench_alloc` uses) because the test harness runs cases
//! in parallel: a process-global counter would tally every other thread's
//! allocations into the measured window and make the assertion flaky.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// Whether the calling thread is currently inside [`count_allocs`].
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// Allocations tallied on this thread since [`count_allocs`] armed it.
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

/// A [`System`]-backed allocator that tallies allocations made on an armed
/// thread.
pub struct ProbeAllocator;

// SAFETY: every method forwards its arguments unchanged to the corresponding
// `System` method, so the safety contract is exactly `System`'s. The added
// tally touches only const-initialized, non-`Drop` thread-local `Cell`s, which
// cannot themselves allocate and so cannot recurse into this allocator.
unsafe impl GlobalAlloc for ProbeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        tally();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        tally();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Record one allocation if this thread is armed. `try_with` rather than
/// `with`, because an allocation during thread teardown must not panic.
fn tally() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = COUNT.try_with(|count| count.set(count.get() + 1));
        }
    });
}

/// Run `f` on this thread, returning its value plus the number of
/// allocations it made. `f`'s return value must not itself allocate, or the
/// tally counts that too. Not re-entrant.
pub fn count_allocs<T>(f: impl FnOnce() -> T) -> (T, u64) {
    COUNT.with(|count| count.set(0));
    ARMED.with(|armed| armed.set(true));
    let out = f();
    ARMED.with(|armed| armed.set(false));
    (out, COUNT.with(Cell::get))
}
