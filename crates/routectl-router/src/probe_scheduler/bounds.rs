//! Every bound the probe scheduler enforces, as code CONSTANTS.
//!
//! None is an operator knob and none is read from the environment: they are
//! the safety envelope that keeps background validation from competing with
//! served traffic, and a parameter a process can widen is an exemption that
//! leaves no diff.

use std::time::Duration;

/// Ceiling on jobs tracked at once, in every phase combined (waiting,
/// leased, or backing off). Activation past it is refused rather than
/// queued: an unbounded backlog of background work outlives the traffic
/// that motivated it and competes with served requests.
pub const PROBE_QUEUE_DEPTH: usize = 16;

/// Ceiling on leases held at once. Background validation is a
/// best-effort side channel, so it gets a small fixed share of upstream
/// concurrency rather than one slot per queued job.
pub const PROBE_MAX_CONCURRENCY: usize = 2;

/// How long a probe operation may run. The WORKER enforces this by wrapping
/// the operation future, which drops it on expiry and settles the lease; the
/// scheduler itself never expires a lease on a clock.
pub const PROBE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Backoff after the first failed attempt. Every later attempt doubles
/// this, clamped to [`PROBE_BACKOFF_CEILING`].
pub const PROBE_BACKOFF_BASE: Duration = Duration::from_mins(1);

/// Ceiling on the computed backoff, so the doubling cannot schedule a
/// retry beyond a bounded horizon.
pub const PROBE_BACKOFF_CEILING: Duration = Duration::from_hours(1);

/// Ceiling on terminal tombstones held at once.
///
/// Separate from -- and deliberately LARGER than -- [`PROBE_QUEUE_DEPTH`],
/// because the two bound different things. The queue bounds concurrent
/// in-flight work; tombstones accumulate one per identity SETTLED during an
/// incarnation, so a deployment with more lanes than the queue depth would
/// saturate terminal capacity almost immediately and fail closed for the rest
/// of the incarnation. Sized for a realistic lane count (one identity per
/// model-seat pair carrying the closed-table capability) while staying a
/// fixed bound: memory is at most this many keys plus an incarnation number.
pub const PROBE_TOMBSTONE_CAPACITY: usize = 256;

/// Event name of the bounded terminal-capacity saturation diagnostic.
pub const PROBE_TOMBSTONE_SATURATED_EVENT: &str = "probe_tombstone_capacity_saturated";

/// Event name of the bounded beta-retention refusal diagnostic.
pub const PROBE_PAYLOAD_REFUSED_EVENT: &str = "probe_payload_retention_refused";

/// Attempts one job may spend before it is abandoned. A lane whose probe
/// keeps failing is dropped rather than retried indefinitely; real
/// traffic re-activates it on a later generation.
///
/// It also bounds the EXECUTABLE FREE-PLAN LENGTH, which is easy to miss: an
/// advance through the plan charges an attempt, so a plan longer than this can
/// never walk all of its steps -- the job is abandoned mid-plan. Today's free
/// plan is one executable step (`CountTokens`), so the two do not collide; a
/// future plan with more free steps than this must raise this bound alongside,
/// or its later steps are unreachable.
pub const PROBE_MAX_ATTEMPTS: u32 = 3;

/// Deferrals one job may accumulate before it is EVICTED from the queue.
///
/// A deferral charges no attempt, which is correct -- no question was asked --
/// but "charges nothing" cannot mean "occupies a queue slot forever". A lane
/// whose breaker stays open indefinitely would otherwise hold one of the
/// [`PROBE_QUEUE_DEPTH`] slots for the whole incarnation, and enough such lanes
/// would fill the queue, leaving no capacity for HEALTHY lanes that could
/// actually be probed.
///
/// So occupancy is bounded SEPARATELY from the attempt budget. At this ceiling
/// the job is evicted and its slot released -- and deliberately NOT
/// tombstoned: the identity was never answered, so later real traffic on a
/// recovered lane must be free to activate it again. Eviction gives the slot
/// back; a tombstone would take the lane out for the incarnation.
///
/// This bounds occupancy PER ACTIVATION and returns capacity to the queue. It
/// is NOT a priority guarantee: traffic that keeps reactivating an unprobeable
/// lane can keep re-acquiring a slot, so a healthy lane competes for capacity
/// rather than being assured of it. The unbounded case -- one activation
/// holding a slot for a whole incarnation -- is what the ceiling rules out.
///
/// Higher than [`PROBE_MAX_ATTEMPTS`] because a deferral is a cheaper event
/// than a failed dial: it costs no upstream call, so a lane deserves more
/// patience for "not now" than for "asked and failed".
pub const PROBE_MAX_DEFERRALS: u32 = 8;

/// The backoff before a job's next attempt, given how many attempts it
/// has already spent.
///
/// Geometric from [`PROBE_BACKOFF_BASE`] and clamped to
/// [`PROBE_BACKOFF_CEILING`]. The exponent is clamped before the shift,
/// not after: a shift of 32 or more on a `u32` is not a defined
/// operation, and the ceiling already wins many orders of magnitude
/// below that, so clamping it changes no answer.
#[must_use]
pub const fn backoff_for_attempt(attempt: u32) -> Duration {
    const MAX_EXPONENT: u32 = 20;
    let raw = attempt.saturating_sub(1);
    let exponent = if raw > MAX_EXPONENT {
        MAX_EXPONENT
    } else {
        raw
    };
    let scaled = PROBE_BACKOFF_BASE.saturating_mul(1u32 << exponent);
    if scaled.as_nanos() > PROBE_BACKOFF_CEILING.as_nanos() {
        PROBE_BACKOFF_CEILING
    } else {
        scaled
    }
}
