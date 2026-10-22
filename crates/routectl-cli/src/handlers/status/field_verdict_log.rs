//! Shared live-visibility log line for the envelope-field fidelity surface,
//! called from both the health and doctor panels so a field repair, a pre-flight
//! rewrite, or a durably purged verdict stays observable at INFO without a
//! response-body change.
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
//! unconfirmed-exposure count without its lifetime sibling reads as the other.
//! Splitting them across lines would make a reader correlate by timestamp.
//!
//! # Why at most ONE line per REQUEST, not per panel build
//!
//! The `/status` aggregate builds the health panel and the doctor panel in one
//! request, and both carry this surface. Emitting from each would put two
//! identical snapshots in the log per poll -- which is not merely noise: a reader
//! counting lines to estimate poll rate would read double, and two lines from one
//! request differ in their timestamps while describing one moment. So the
//! aggregate emits from the first builder and SUPPRESSES the second, while each
//! standalone panel endpoint still emits at most once. The choice is an explicit
//! argument rather than a heuristic, so no builder has to infer who else ran.
//!
//! # Why a change gate on top of the claim
//!
//! The surface is poll-driven, and a dashboard polls every few seconds, so one line
//! per request floods the log with identical snapshots. A daemon-scoped
//! [`FidelityGate`] therefore admits a line only when its content fingerprint
//! changed or [`SNAPSHOT_HEARTBEAT`] has passed since the last admitted line. No
//! poller means no line: nothing emits on a timer of its own.

use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use routectl_router::FidelitySnapshot;

/// The structured payload the fidelity INFO line carries.
///
/// Built once, consumed by the `cfg(test)` observers.
///
/// Every field here is ASSERTED by an observer test. The process-global writer
/// health deliberately is NOT carried: in every daemon fixture that can reach an
/// endpoint it holds its default (`false`), so an endpoint assertion on it would read
/// the same against a field wired to nothing -- and there is no test seam that forges
/// a write error on a real writer. It is pinned where it CAN be discriminated
/// instead: `the_writer_health_field_carries_the_passed_value` drives the emitter with
/// `true` and asserts the emitted LINE, which is the surface an operator reads.
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
}

/// A per-call observer slot.
#[cfg(test)]
type EventObserver = std::sync::Mutex<Option<tokio::sync::oneshot::Sender<FidelityEvent>>>;

use super::fidelity_log::{
    Bounded, RenderedVerdict, rendered_exhausted_lanes, rendered_lossy_lanes, rendered_verdicts,
};
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
/// reach the logger wins it. At most one line per request, and health failing lets
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
/// scheduler state, and the process-global usage-writer health. Log-only -- never
/// crosses onto a panel's response body.
///
/// `writer_degraded` is passed IN rather than read here: it is a counter read on a
/// handle this module does not hold. Keeping it out makes this function a pure
/// projection over its inputs, which is what lets a status poll be asserted pure.
pub(super) fn log_field_verdict_snapshot(
    view: &StatusRouterView,
    writer_degraded: bool,
    emission: FidelityEmission,
    gate: &FidelityGate,
) {
    // The CLAIM decides first, not a pre-assigned role: whichever fidelity-carrying
    // builder reaches this first may emit, so a failing sibling cannot take the line
    // down with it, and the gate below can only narrow one request to zero lines,
    // never widen it to two.
    if !emission.claim() {
        return;
    }
    // ONE router read for the whole line. The acting rows and the detailed rows
    // come from one learned snapshot inside it, so the two counts can differ only
    // by their documented filters -- never by timing, which a reader reconciling
    // them could not tell apart from a real filter.
    let snapshot: FidelitySnapshot = view.fidelity_snapshot();
    emit_if_admitted(&snapshot, writer_degraded, &emission, gate, Instant::now());
}

/// The gated half of [`log_field_verdict_snapshot`], with the clock passed in.
///
/// Takes the snapshot rather than the view so a test can vary one field of it at a
/// time, and `now` so the heartbeat is checkable without waiting an hour.
fn emit_if_admitted(
    snapshot: &FidelitySnapshot,
    writer_degraded: bool,
    #[cfg_attr(
        not(test),
        expect(unused_variables, reason = "read only by the test-build observer")
    )]
    emission: &FidelityEmission,
    gate: &FidelityGate,
    now: Instant,
) {
    let verdicts = rendered_verdicts(&snapshot.verdicts);
    let lossy_lanes = rendered_lossy_lanes(&snapshot.lossy_by_lane);
    let exhausted_lanes = rendered_exhausted_lanes(&snapshot.exhausted_by_lane);
    let lanes = LaneRenders {
        lossy: &lossy_lanes,
        exhausted: &exhausted_lanes,
    };
    let fingerprint = line_fingerprint(snapshot, &verdicts, &lanes, writer_degraded);
    if !gate.admit(fingerprint, now) {
        return;
    }
    // The observer event is built ONLY in a test build, and the gate is about cost
    // rather than tidiness: the event owns a whole `FidelitySnapshot`, whose verdict
    // and acting vectors grow with the number of (target, field) identities a
    // deployment has learned -- operator- and traffic-driven, not fixed. Building it
    // unconditionally meant every status poll of every daemon deep-cloned that
    // unbounded structure for a consumer no release build has, on a surface polled
    // every few seconds. `emit_observer_event` is where the construction lives, so
    // nothing in this function body allocates a second snapshot outside `cfg(test)`.
    #[cfg(test)]
    emit_observer_event(emission, snapshot);
    let probes = &snapshot.probes;
    tracing::info!(
        rc_field_repair_attempted_total = snapshot.counters.repair_attempted,
        rc_field_repair_lossy_total = snapshot.counters.repair_lossy,
        rc_field_repair_lossy_lanes_total = lossy_lanes.total,
        rc_field_repair_lossy_lanes_omitted = lossy_lanes.omitted,
        rc_field_repair_lossy_by_lane = ?lossy_lanes.rows,
        rc_field_repair_succeeded_total = snapshot.counters.repair_succeeded,
        rc_field_verdicts_learned_total = snapshot.counters.verdicts_learned,
        rc_field_outstanding_unconfirmed_total = snapshot.counters.outstanding_unconfirmed,
        rc_field_disproved_requests_total = snapshot.counters.disproved_requests,
        rc_field_preflight_actions_total = snapshot.counters.preflight_actions,
        rc_parser_unlocalized_total = snapshot.counters.parser_unlocalized,
        rc_field_chain_steps_committed_total = snapshot.counters.chain_steps_committed,
        rc_field_chain_steps_dropped_total = snapshot.counters.chain_steps_dropped,
        rc_field_repair_budget_exhausted_total = snapshot.counters.budget_exhausted,
        rc_field_repair_budget_exhausted_lanes_total = exhausted_lanes.total,
        rc_field_repair_budget_exhausted_lanes_omitted = exhausted_lanes.omitted,
        rc_field_repair_budget_exhausted_by_lane = ?exhausted_lanes.rows,
        rc_field_repairs_per_request = ?snapshot.counters.repairs_per_request,
        rc_acting_field_verdicts_total = snapshot.acting.len(),
        rc_acting_field_verdicts = ?bounded_acting(snapshot),
        rc_acting_field_verdicts_omitted = acting_omitted(snapshot),
        rc_field_verdict_rows_total = verdicts.total,
        rc_field_verdict_rows_omitted = verdicts.omitted,
        rc_field_verdict_rows = ?verdicts.rows,
        rc_usage_writer_degraded = writer_degraded,
        // Probe state from the scheduler's OWN snapshot rather than re-derived, so
        // the queue numbers here are the numbers the scheduler enforces its bounds
        // against.
        rc_probe_activations_total = probes.activations_total,
        rc_probe_queued = probes.queued,
        rc_probe_in_flight = probes.in_flight,
        rc_probe_backing_off = probes.backing_off,
        rc_probe_last_settlement = probe_settlement_token(probes),
        rc_probe_next_retry_ms = next_retry_ms(probes),
        rc_probe_oldest_due_ms = oldest_due_ms(probes),
        rc_probe_clear_persistence_failures_total = probes.clear_persistence_failures_total,
        "{FIDELITY_SNAPSHOT_MESSAGE}",
    );
}

/// How long an unchanged snapshot stays silent before it is emitted again.
///
/// The repeat is what lets an operator tell "nothing changed" from "the poller
/// stopped" or "the line was lost" by reading the log alone.
pub(super) const SNAPSHOT_HEARTBEAT: Duration = Duration::from_hours(1);

/// The daemon-scoped change gate on the fidelity line: the fingerprint of the last
/// admitted line and when it was admitted.
///
/// One per daemon, shared by every status request, so the decision spans polls --
/// a request-scoped value could not tell a repeat from a first sighting.
#[derive(Debug, Default)]
pub(super) struct FidelityGate {
    last: Mutex<Option<GateMark>>,
}

#[derive(Debug, Clone, Copy)]
struct GateMark {
    fingerprint: u64,
    last_emit: Instant,
}

impl FidelityGate {
    /// Whether a line with `fingerprint` is emitted at `now`, recording it if so.
    ///
    /// Check and set under ONE lock, so two concurrent polls over the same state
    /// cannot both observe the old mark and both emit.
    fn admit(&self, fingerprint: u64, now: Instant) -> bool {
        let mut last = self.last.lock();
        let due = last.is_none_or(|mark| {
            mark.fingerprint != fingerprint
                || now.saturating_duration_since(mark.last_emit) >= SNAPSHOT_HEARTBEAT
        });
        if due {
            *last = Some(GateMark {
                fingerprint,
                last_emit: now,
            });
        }
        due
    }
}

/// The two bounded per-lane renders the line carries.
struct LaneRenders<'a> {
    lossy: &'a Bounded<routectl_router::LaneLossyRepairs>,
    exhausted: &'a Bounded<routectl_router::LaneBudgetExhaustions>,
}

/// Fingerprint of everything the line carries EXCEPT the two clock-derived fields,
/// `rc_probe_next_retry_ms` and `rc_probe_oldest_due_ms`.
///
/// Both move on every poll while nothing an operator acts on has changed -- one
/// counts down, the other up -- so including either would admit a line per poll and
/// defeat the gate. Every other field on the line is a count, a token, or a rendered
/// row with no clock-derived content. The `?`-rendered fields are hashed through the same
/// `Debug` rendering the line uses, so a field added to a row type moves the
/// fingerprint without this function having to learn about it.
fn line_fingerprint(
    snapshot: &FidelitySnapshot,
    verdicts: &Bounded<RenderedVerdict>,
    lanes: &LaneRenders<'_>,
    writer_degraded: bool,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    let counters = &snapshot.counters;
    for count in [
        counters.repair_attempted,
        counters.repair_lossy,
        counters.repair_succeeded,
        counters.verdicts_learned,
        counters.outstanding_unconfirmed,
        counters.disproved_requests,
        counters.preflight_actions,
        counters.parser_unlocalized,
        counters.chain_steps_committed,
        counters.chain_steps_dropped,
        counters.budget_exhausted,
        snapshot.probes.activations_total,
        snapshot.probes.clear_persistence_failures_total,
    ] {
        count.hash(&mut hasher);
    }
    for count in [
        snapshot.acting.len(),
        acting_omitted(snapshot),
        verdicts.total,
        verdicts.omitted,
        lanes.lossy.total,
        lanes.lossy.omitted,
        lanes.exhausted.total,
        lanes.exhausted.omitted,
        snapshot.probes.queued,
        snapshot.probes.in_flight,
        snapshot.probes.backing_off,
    ] {
        count.hash(&mut hasher);
    }
    writer_degraded.hash(&mut hasher);
    probe_settlement_token(&snapshot.probes).hash(&mut hasher);
    hash_debug(&mut hasher, bounded_acting(snapshot));
    hash_debug(&mut hasher, &verdicts.rows);
    hash_debug(&mut hasher, &lanes.lossy.rows);
    hash_debug(&mut hasher, &lanes.exhausted.rows);
    hash_debug(&mut hasher, &counters.repairs_per_request);
    hasher.finish()
}

/// Feed `value`'s `Debug` rendering into `hasher` without allocating it.
fn hash_debug<T: std::fmt::Debug + ?Sized>(hasher: &mut DefaultHasher, value: &T) {
    struct HashWriter<'a>(&'a mut DefaultHasher);
    impl std::fmt::Write for HashWriter<'_> {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            self.0.write(s.as_bytes());
            Ok(())
        }
    }
    // Writing into a hasher cannot fail; the `Result` is the trait's shape only.
    let _ = write!(HashWriter(hasher), "{value:?}");
    // Terminator, so adjacent renderings cannot trade bytes across the boundary.
    hasher.write_u8(0xff);
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
fn emit_observer_event(emission: &FidelityEmission, snapshot: &FidelitySnapshot) {
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
/// deployment has learned, and this line may be emitted on any poll.
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

/// How long the longest-waiting leasable job has been due, in whole
/// milliseconds, or -1 when no job is leasable.
///
/// The same negative sentinel as [`next_retry_ms`], for the same reason: zero is
/// a real answer (a job that became due this instant).
fn oldest_due_ms(probes: &routectl_router::ProbeSchedulerSnapshot) -> i64 {
    probes.oldest_due_age.map_or(NO_DUE_PROBE, |age| {
        i64::try_from(age.as_millis()).unwrap_or(i64::MAX)
    })
}

/// Rendered oldest-due age for a scheduler with no leasable job.
pub(super) const NO_DUE_PROBE: i64 = -1;

#[cfg(test)]
#[path = "field_verdict_log_tests.rs"]
mod tests;
