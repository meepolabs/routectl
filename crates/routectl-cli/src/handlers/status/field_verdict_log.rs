//! Shared live-visibility log line for the envelope-field fidelity surface,
//! called from both the health and doctor panels so a field repair, a pre-flight
//! rewrite, a durably purged verdict, or a paid-probe budget stays observable at
//! INFO without a response-body change.
//!
//! # Why INFO, and why not the response body
//!
//! A response-body annotation is refused by design -- a routectl key in the
//! flattened extras is itself a fidelity leak -- so the observability floor is
//! satisfied operator-side. It must be readable at the LIVE log level, which is
//! why this is INFO and not the 60-second DEBUG metrics snapshot: a floor
//! satisfied only there would be a floor no operator can read.
//!
//! # Why one line
//!
//! The facts are only interpretable together. A confirmation count without its
//! required quorum cannot be read as sufficient or short; an
//! unconfirmed-exposure count without its lifetime sibling reads as the other; a
//! used budget without its cap says nothing at all. Splitting them across lines
//! would make a reader correlate by timestamp.
//!
//! # Why ONE line per REQUEST, not per panel build
//!
//! The `/status` aggregate builds the health panel and the doctor panel in one
//! request, and both carry this surface. Emitting from each would put two
//! identical snapshots in the log per poll -- which is not merely noise: a reader
//! counting lines to estimate poll rate would read double, and two lines from one
//! request differ in their timestamps while describing one moment. So the
//! aggregate emits from the first builder and SUPPRESSES the second, while each
//! standalone panel endpoint still emits exactly once. The choice is an explicit
//! argument rather than a heuristic, so no builder has to infer who else ran.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use routectl_router::FidelitySnapshot;

/// The structured payload the fidelity INFO line carries.
///
/// Built once, consumed by the `cfg(test)` observers.
///
/// Every field here is ASSERTED by an observer test. The process-global accounting
/// facts deliberately are NOT carried: in every daemon fixture that can reach an
/// endpoint they hold their defaults (`writer_degraded: false`,
/// `consumed_unauthorized_total: 0`), so an endpoint assertion on them would read the
/// same against a field wired to nothing -- and there is no test seam that forges a
/// write error on a real writer. They are pinned where they CAN be discriminated
/// instead: `the_global_fields_carry_the_globals_values` drives the emitter with
/// `true`/`41` and asserts the emitted LINE, which is the surface an operator reads.
///
/// `cfg(test)` on the TYPE, not merely on its construction: its only constructor is
/// the gated `emit_observer_event`, so outside a test build it would be a struct
/// nothing can build -- which is what an `allow(dead_code)` was previously papering
/// over. Gating it makes the absence structural rather than silenced.
///
/// `pub(crate)` rather than module-private: the assembled-daemon sidecar lives in
/// `crate::server` and drains these off the daemon-scoped observer, so it must be
/// able to name the type. Nothing outside this crate can.
#[cfg(test)]
#[derive(Debug, Clone)]
#[expect(
    clippy::redundant_pub_crate,
    reason = "the module is private, but this is a \
    test-only seam named from a sibling module tree, so the crate visibility is \
    load-bearing rather than cosmetic -- `pub` would widen it past the crate"
)]
pub(crate) struct FidelityEvent {
    pub(crate) snapshot: FidelitySnapshot,
    pub(crate) verdict_rows_total: usize,
    /// Budget rows the line carried. Discriminating at the endpoint: a daemon with a
    /// configured `[fidelity.paid_probe_daily_caps]` entry emits one, while the
    /// no-config doctor branch legitimately emits none.
    pub(crate) budget_rows_total: usize,
}

/// A per-call observer slot.
#[cfg(test)]
type EventObserver = std::sync::Mutex<Option<tokio::sync::oneshot::Sender<FidelityEvent>>>;

use super::fidelity_log::{rendered_budgets, rendered_verdicts};
use super::paid_probe_budget::{AccountingGlobals, PaidProbeBudget};
use super::router_view::StatusRouterView;

/// Message of the snapshot line. Stable and greppable, and named so a test can
/// assert on it without restating the string.
pub(super) const FIDELITY_SNAPSHOT_MESSAGE: &str = "envelope field verdict snapshot";

/// A request-scoped, one-shot claim on the shared fidelity line.
///
/// # Why a CLAIM rather than a pre-assigned role
///
/// The previous shape named one builder the emitter up front, which is wrong for
/// the reason the aggregate exists: each panel is built through `guard_panel`, which
/// degrades a failing data source to an unavailable panel. So a pre-assigned
/// emitter that failed emitted nothing, and its designated-suppressed sibling --
/// which built fine -- stayed silent too. One failing panel silently removed the
/// whole observability floor from that request.
///
/// Here every fidelity-carrying builder ATTEMPTS the claim and the first one to
/// reach the logger wins it. Exactly one line per request, and health failing lets
/// doctor emit.
///
/// A `Clone`d handle rather than a borrow, because the builders run inside
/// `spawn_blocking` closures that take ownership of what they capture.
#[derive(Debug, Clone)]
pub(super) struct FidelityEmission {
    /// `None` on a standalone panel endpoint: it is the only fidelity-carrying
    /// builder in its request, so there is nothing to arbitrate and it always
    /// emits. `Some` in the aggregate, shared by both builders.
    claim: Option<Arc<AtomicBool>>,
    /// Per-call event observer, installed by a unit test that drives the emitter
    /// directly.
    #[cfg(test)]
    observer: Option<Arc<EventObserver>>,
    /// DAEMON-scoped event observer, carried from `StatusState`.
    ///
    /// Separate from the per-call slot above rather than replacing it, because the
    /// two answer different questions and neither can answer the other's. The
    /// per-call slot is installed by a test that CONSTRUCTS the emission, which a
    /// test over a running daemon cannot do -- the daemon's handlers construct their
    /// own. This one rides the state the handlers read, so it sees what the real
    /// endpoints emit, and it is multi-event so a per-request COUNT is checkable
    /// (see `test_hooks::FidelityObserver`).
    #[cfg(test)]
    daemon_observer: Option<Arc<super::test_hooks::FidelityObserver>>,
}

impl FidelityEmission {
    /// The standalone case: one builder, no contention, always emits.
    pub(super) const fn always() -> Self {
        Self {
            claim: None,
            #[cfg(test)]
            observer: None,
            #[cfg(test)]
            daemon_observer: None,
        }
    }

    /// A fresh request-scoped claim, shared by every builder in one request.
    ///
    /// Cloned into each builder; whichever reaches the logger first takes it.
    pub(super) fn shared() -> Self {
        Self {
            claim: Some(Arc::new(AtomicBool::new(false))),
            #[cfg(test)]
            observer: None,
            #[cfg(test)]
            daemon_observer: None,
        }
    }

    /// Install a per-call observer. Returns the receiver the test awaits.
    #[cfg(test)]
    pub(super) fn observe(&mut self) -> tokio::sync::oneshot::Receiver<FidelityEvent> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.observer = Some(Arc::new(std::sync::Mutex::new(Some(tx))));
        rx
    }

    /// Attach the daemon's own fidelity observer, if this daemon carries one.
    ///
    /// Called by every fidelity-carrying builder as it constructs its emission, so
    /// the observer reaches the emitter through the state the real handlers read
    /// rather than through anything a request could pass.
    #[cfg(test)]
    pub(super) fn with_daemon_observer(mut self, state: &super::StatusState) -> Self {
        self.daemon_observer = state.test_hooks.fidelity_observer.clone();
        self
    }

    /// Whether THIS caller may emit, taking the claim if so.
    ///
    /// `compare_exchange` rather than a read-then-set: the aggregate awaits its
    /// builders sequentially today, but a future concurrent join must not be able to
    /// let two builders both observe the claim free. The atomic makes the
    /// exactly-once property independent of that scheduling choice.
    fn claim(&self) -> bool {
        self.claim.as_ref().is_none_or(|flag| {
            flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        })
    }
}

/// Emit the single aggregated fidelity INFO log for one request: the live
/// process-lifetime counters, every resident field verdict's own row, the probe
/// scheduler state, the paid-probe budgets, and the process-global accounting
/// facts. Log-only -- never crosses onto a panel's response body.
///
/// `budgets` and `globals` are passed IN rather than read here. The budgets need a
/// ledger open, which belongs on the blocking thread the caller already runs on;
/// the globals are counter reads on a handle this module does not hold. Keeping
/// both out makes this function a pure projection over its inputs, which is what
/// lets a status poll be asserted pure.
pub(super) fn log_field_verdict_snapshot(
    view: &StatusRouterView,
    budgets: &[PaidProbeBudget],
    globals: AccountingGlobals,
    emission: FidelityEmission,
) {
    // The CLAIM decides, not a pre-assigned role: whichever fidelity-carrying
    // builder reaches this first emits, so a failing sibling cannot take the line
    // down with it.
    if !emission.claim() {
        return;
    }
    // ONE router read for the whole line. The acting rows and the detailed rows
    // come from one learned snapshot inside it, so the two counts can differ only
    // by their documented filters -- never by timing, which a reader reconciling
    // them could not tell apart from a real filter.
    let snapshot: FidelitySnapshot = view.fidelity_snapshot();
    // The observer event is built ONLY in a test build, and the gate is about cost
    // rather than tidiness: the event owns a whole `FidelitySnapshot`, whose verdict
    // and acting vectors grow with the number of (target, field) identities a
    // deployment has learned -- operator- and traffic-driven, not fixed. Building it
    // unconditionally meant every status poll of every daemon deep-cloned that
    // unbounded structure for a consumer no release build has, on a surface polled
    // every few seconds. `emit_observer_event` is where the construction lives, so
    // nothing in this function body allocates a second snapshot outside `cfg(test)`.
    #[cfg(test)]
    emit_observer_event(&emission, &snapshot, budgets);
    let verdicts = rendered_verdicts(&snapshot.verdicts);
    let rendered_budgets = rendered_budgets(budgets);
    let probes = &snapshot.probes;
    tracing::info!(
        rc_field_repair_attempted_total = snapshot.counters.repair_attempted,
        rc_field_repair_succeeded_total = snapshot.counters.repair_succeeded,
        rc_field_verdicts_learned_total = snapshot.counters.verdicts_learned,
        rc_field_outstanding_unconfirmed_total = snapshot.counters.outstanding_unconfirmed,
        rc_field_disproved_requests_total = snapshot.counters.disproved_requests,
        rc_field_preflight_actions_total = snapshot.counters.preflight_actions,
        rc_parser_unlocalized_total = snapshot.counters.parser_unlocalized,
        rc_acting_field_verdicts_total = snapshot.acting.len(),
        rc_acting_field_verdicts = ?bounded_acting(&snapshot),
        rc_acting_field_verdicts_omitted = acting_omitted(&snapshot),
        rc_field_verdict_rows_total = verdicts.total,
        rc_field_verdict_rows_omitted = verdicts.omitted,
        rc_field_verdict_rows = ?verdicts.rows,
        rc_paid_probe_budgets_total = rendered_budgets.total,
        rc_paid_probe_budgets_omitted = rendered_budgets.omitted,
        rc_paid_probe_budgets = ?rendered_budgets.rows,
        // PROCESS-GLOBAL, emitted once and never copied onto a provider row: the
        // writer is one actor and the unauthorized counter has no provider
        // dimension, so per-row copies would make a summed column read as a
        // multiple of the truth. Resets on restart -- see `AccountingGlobals`.
        rc_usage_writer_degraded = globals.writer_degraded,
        rc_paid_probe_consumed_unauthorized_total = globals.consumed_unauthorized_total,
        // Probe state from the scheduler's OWN snapshot rather than re-derived, so
        // the queue numbers here are the numbers the scheduler enforces its bounds
        // against.
        rc_probe_activations_total = probes.activations_total,
        rc_probe_queued = probes.queued,
        rc_probe_in_flight = probes.in_flight,
        rc_probe_backing_off = probes.backing_off,
        rc_probe_last_settlement = probe_settlement_token(probes),
        rc_probe_next_retry_ms = next_retry_ms(probes),
        "{FIDELITY_SNAPSHOT_MESSAGE}",
    );
}

/// Build and deliver the observer event, in a TEST BUILD ONLY.
///
/// The whole reason this is a separate `cfg(test)` function rather than an inline
/// block: the event owns a cloned `FidelitySnapshot`, and that clone is unbounded --
/// its verdict and acting vectors grow with the identities a deployment has learned.
/// Constructing it in the emitter body made every production status poll pay for a
/// deep copy no release build can observe. Here the allocation cannot exist outside a
/// test build, because the function does not.
///
/// The snapshot is cloned ONCE and moved into the event; each observer then receives
/// it by clone only if it is actually installed, so a test watching one channel does
/// not pay for the other.
#[cfg(test)]
fn emit_observer_event(
    emission: &FidelityEmission,
    snapshot: &FidelitySnapshot,
    budgets: &[PaidProbeBudget],
) {
    let per_call = emission
        .observer
        .as_ref()
        .and_then(|obs| obs.lock().expect("observer lock").take());
    let daemon = emission.daemon_observer.as_ref();
    if per_call.is_none() && daemon.is_none() {
        return;
    }
    let event = FidelityEvent {
        verdict_rows_total: snapshot.verdicts.len(),
        budget_rows_total: budgets.len(),
        snapshot: snapshot.clone(),
    };
    if let Some(obs) = daemon {
        obs.record(&event);
    }
    if let Some(tx) = per_call {
        let _ = tx.send(event);
    }
}

/// The acting rows that fit under the shared render ceiling.
///
/// Bounded for the same reason the verdict rows are: the count grows with what a
/// deployment has learned, and this line is emitted on every poll.
fn bounded_acting(snapshot: &FidelitySnapshot) -> &[routectl_router::ActingFieldVerdict] {
    let end = snapshot
        .acting
        .len()
        .min(super::fidelity_log::MAX_RENDERED_ROWS);
    &snapshot.acting[..end]
}

/// Acting rows the ceiling dropped, so a truncated field says how much it hides.
const fn acting_omitted(snapshot: &FidelitySnapshot) -> usize {
    snapshot
        .acting
        .len()
        .saturating_sub(super::fidelity_log::MAX_RENDERED_ROWS)
}

/// Rendered last-probe-settlement token, with the never-settled case spelled.
///
/// A literal rather than an absent field, like every other absence on this line:
/// "no probe has settled" is a real state -- it is what a lane that has never been
/// activated looks like -- and distinguishing it from a settled outcome is the
/// whole point of reporting it.
fn probe_settlement_token(probes: &routectl_router::ProbeSchedulerSnapshot) -> &'static str {
    probes
        .last_settlement
        .map_or(PROBE_SETTLEMENT_NONE, |settled| settled.as_str())
}

/// Rendered last-settlement for a scheduler nothing has settled on yet.
pub(super) const PROBE_SETTLEMENT_NONE: &str = "none";

/// The earliest next-retry wait in whole milliseconds, or -1 when nothing is
/// backing off.
///
/// A SENTINEL rather than an absent field, and a negative one rather than zero:
/// zero is a real answer here (a backoff that has already elapsed, leasable on the
/// next tick), so it cannot also mean "nothing to wait for". Rendered as an
/// integer rather than a `Duration` debug string so an operator can threshold on
/// it.
fn next_retry_ms(probes: &routectl_router::ProbeSchedulerSnapshot) -> i64 {
    probes.next_retry_in.map_or(NO_NEXT_RETRY, |wait| {
        i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)
    })
}

/// Rendered next-retry for a scheduler with nothing backing off.
pub(super) const NO_NEXT_RETRY: i64 = -1;

#[cfg(test)]
#[path = "field_verdict_log_tests.rs"]
mod tests;
