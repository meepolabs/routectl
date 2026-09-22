//! The daemon's bounded probe driver.
//!
//! Its own module rather than a block in the reload coordinator: the
//! coordinator decides WHETHER to publish a replacement router, while this
//! module owns HOW background probe work is driven. The publication step the
//! coordinator uses lives beside it in `router_publish.rs`.

use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_router::Router;
use tokio::sync::watch;

/// How often the probe driver looks for due free-validator work and attempts
/// a paid probe.
///
/// A code CONSTANT, not an operator knob and never read from the
/// environment: it is one of the bounds that keeps background validation
/// from competing with served traffic, and a parameter a process can widen
/// is an exemption that leaves no diff. Deliberately slow -- probe work is
/// a best-effort side channel, and a lane's answer is not time-critical.
pub(super) const PROBE_DRIVER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Drive the live `Router`'s probe pass -- the due free batch, then at most
/// one paid probe -- until `shutdown` fires, then cancel whatever is still
/// queued.
///
/// Starts IDLE and stays idle: each tick asks the currently published
/// router for its due work, and with nothing queued that is a lock, a
/// length check, and a return -- zero network work until an admitted real
/// request has activated a lane. Startup, config parsing, reload, and a
/// status read all leave the queue empty, so none of them can cause a call.
///
/// One `run_probe_pass` call per tick, never a second task or interval: the
/// free-before-paid ordering and the at-most-one-paid-candidate bound both
/// live inside that single call, so this driver only needs to schedule it.
///
/// Reads `router_swap` per tick rather than capturing one `Arc`, matching
/// the metrics driver above: a publication switches the driver onto the
/// replacement cleanly, and because the scheduler is shared across the swap
/// an outstanding lease still settles against the table the published
/// router reads.
///
/// Never blocks a request: this is its own task, and the pass it calls
/// bounds itself by the scheduler's queue depth, concurrency ceiling, and
/// per-operation timeout.
pub(super) async fn run_probe_driver(
    router_swap: Arc<ArcSwap<Router>>,
    mut shutdown: watch::Receiver<()>,
) {
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + PROBE_DRIVER_INTERVAL,
        PROBE_DRIVER_INTERVAL,
    );
    loop {
        tokio::select! {
            // BIASED, shutdown first. `select!` picks randomly among ready
            // branches by default, so on a tick that lands in the same poll
            // as the shutdown signal an unbiased select would start a fresh
            // batch of probe dials after shutdown was already ready. `biased`
            // makes the shutdown branch win that race deterministically.
            biased;
            _ = shutdown.changed() => {
                cancel_probe_work_at_shutdown(&router_swap);
                return;
            }
            _ = tick.tick() => {
                // The run is selected AGAINST the shutdown signal, not
                // awaited to completion first. A probe operation can sit for
                // the whole per-operation timeout, so awaiting the run and
                // only then checking shutdown would hold the graceful drain
                // for that long -- past the server's own wait bound. Losing
                // the `select!` DROPS the run future, which cancels every
                // validator future inside it; each lease then releases its
                // slot on drop, so nothing is stranded.
                // `load_full` rather than `load`: a `Guard` is documented as
                // suited to a local on the stack, not to something held for a
                // long time, because the cheap guard slots are limited per
                // thread and a guard held past them degrades to `load_full`'s
                // cost anyway. This borrow spans the whole probe batch -- up to
                // the per-operation timeout -- so taking the owned `Arc` up
                // front is both the documented shape and one predictable
                // refcount bump per tick.
                let live = router_swap.load_full();
                let run = live.run_probe_pass();
                tokio::select! {
                    // Biased the other way ROUND, shutdown still first: the
                    // run is already in flight here, so checking shutdown
                    // first means a signal that arrives while the batch is
                    // running cancels it rather than waiting for whichever
                    // branch the scheduler happens to poll.
                    biased;
                    _ = shutdown.changed() => {
                        cancel_probe_work_at_shutdown(&router_swap);
                        return;
                    }
                    ran = run => {
                        if ran.free_validators_run > 0 || ran.paid_probe_attempted {
                            tracing::debug!(
                                probe_validators_run = ran.free_validators_run,
                                paid_probe_attempted = ran.paid_probe_attempted,
                                "probe driver ran its due free batch and paid attempt",
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Cancel queued probe work on the way out. Outstanding leases settle as
/// stale and schedule no follow-up work.
///
/// Cancel rather than drain: the daemon is going away, so a job that has not
/// run will not run, and draining would hold the shutdown path for as long
/// as the upstream takes.
fn cancel_probe_work_at_shutdown(router_swap: &Arc<ArcSwap<Router>>) {
    // SYNCHRONOUS, and it never waits on an in-flight paid probe. `shutdown_probe_work`
    // advances the shared publication generation to a terminal value no router is
    // stamped with BEFORE clearing, so an in-flight authorization abandons itself
    // at its next generation check and an in-flight refusal cannot requeue --
    // rather than this path waiting for a reservation that may never answer.
    let cancelled = router_swap.load().shutdown_probe_work();
    if cancelled > 0 {
        tracing::debug!(
            cancelled_probe_jobs = cancelled,
            "probe driver cancelled queued work at shutdown",
        );
    }
}
