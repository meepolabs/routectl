//! Process-wide count of upstream streams that ended without their lane's
//! terminal event, beside the per-request count in the request's observation
//! slot. Both advance at the one place a lane decides a stream was cut, so the
//! global total and the sum of the per-request counts move together.
//!
//! The global is a live view for the current process only; the per-request
//! count is what persists.

#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-responses",
    feature = "openai-compat"
))]
use routectl_core::RequestObservation;

/// Streams ended without a terminal event since process start.
pub fn missing_terminal_total() -> u64 {
    read_total()
}

/// Count one stream that ended without its terminal event, on the global
/// and on `slot` when the request carries one.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-responses",
    feature = "openai-compat"
))]
pub(crate) fn record_missing_terminal(slot: Option<&RequestObservation>) {
    bump_total();
    if let Some(slot) = slot {
        slot.add_missing_terminal();
    }
}

#[cfg(not(test))]
static TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(not(test))]
fn read_total() -> u64 {
    TOTAL.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(all(
    not(test),
    any(
        feature = "anthropic-api",
        feature = "openai-responses",
        feature = "openai-compat"
    )
))]
fn bump_total() {
    TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

// The unit-test build keeps the total PER THREAD, so a test reads back
// exactly the increments its own streams caused. A `current_thread` tokio
// test polls its streams on the test's thread; integration tests and
// dependent crates link the non-test build and share the process total.
#[cfg(test)]
thread_local! {
    static TOTAL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn read_total() -> u64 {
    TOTAL.get()
}

#[cfg(all(
    test,
    any(
        feature = "anthropic-api",
        feature = "openai-responses",
        feature = "openai-compat"
    )
))]
fn bump_total() {
    TOTAL.set(TOTAL.get().saturating_add(1));
}
